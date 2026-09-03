# MemRoute

**MemRoute** is a portable, local knowledge router for agent workspaces. It uses [Tantivy](https://github.com/quickwit-oss/tantivy) to index the Markdown knowledge already owned by a workspace collection and returns small, cited context plans instead of injecting entire document trees into an agent session.

- **Local-first** — indexes stay in the collection's `.memroute/` directory and can be deleted and rebuilt at any time.
- **Traceable** — every result includes its workspace, source path, heading path, and exact line range.
- **Scoped** — routing starts in the owning workspace and only crosses workspace boundaries when explicitly requested.
- **Risk-aware** — high-risk routes surface L3 authority entries for manual verification.
- **Agent-ready** — includes an `agent-workspace-init` Skill for creating and operating governed workspaces.

> [!IMPORTANT]
> MemRoute selects candidate evidence. It does **not** make a result true, grant permission to modify a system, or replace verification against the original source of authority.

## Contents

- [Concepts](#concepts)
- [Requirements](#requirements)
- [Quick start](#quick-start)
- [Collection layout](#collection-layout)
- [Configuration](#configuration)
- [Commands](#commands)
- [Agent Skill](#agent-skill)
- [Safety and privacy](#safety-and-privacy)
- [Development](#development)
- [License](#license)

## Concepts

MemRoute uses progressive context loading:

| Layer | Purpose | Typical source |
|---|---|---|
| **L0** | Always-read rules and knowledge navigation | `AGENTS.md` |
| **L1** | Task profile: goal, entities, environment, action, and risk | User request / task record |
| **L2** | 3--8 routed evidence sections | `memory/`, current task, or selected task history |
| **L3** | Original, mutable, or high-risk authorities | Source code, APIs, platform pages, live configuration, and operational systems |

The CLI indexes only Markdown in each enabled workspace's `AGENTS.md`, `memory/`, and `tasks/` paths. It creates heading-level chunks and classifies them as:

- `route` — a row from the `## 知识导航` table in `AGENTS.md`.
- `stable_fact` — reusable knowledge from `memory/`.
- `runbook` — operational knowledge from `memory/`.
- `task_experience` — task records under `tasks/`.
- `external_authority` — an L3 verification entry derived from a source reference or knowledge-navigation boundary.

## Requirements

- A current stable [Rust toolchain](https://www.rust-lang.org/tools/install) with Cargo. The project uses Rust edition 2024.
- A workspace collection containing a `memroute.toml` configuration file.

## Quick start

Clone this repository and set paths for the cloned repository and your own collection. The collection is **your data**; it is not stored in this repository.

```bash
git clone <repository-url> memroute
export MEMROUTE_REPOSITORY="$PWD/memroute"
export COLLECTION_ROOT=/path/to/your/collection
```

Create the collection configuration shown in [Configuration](#configuration), then build the local index:

```bash
cargo run --manifest-path "$MEMROUTE_REPOSITORY/Cargo.toml" -- --root "$COLLECTION_ROOT" index
```

Route a task to a small L2 context plan:

```bash
cargo run --manifest-path "$MEMROUTE_REPOSITORY/Cargo.toml" -- \
  --root "$COLLECTION_ROOT" \
  route "investigate API timeout" \
  --workspace platform \
  --intent investigate \
  --entity api-gateway \
  --environment production \
  --limit 3 \
  --json
```

If the shell's current directory is the collection root, `--root` is optional:

```bash
cd "$COLLECTION_ROOT"
cargo run --manifest-path "$MEMROUTE_REPOSITORY/Cargo.toml" -- index
```

To install the CLI locally instead of using `cargo run`:

```bash
cd "$MEMROUTE_REPOSITORY"
cargo install --path .
memroute --root "$COLLECTION_ROOT" index
```

## Collection layout

A collection can contain one or more independently owned workspaces.

```text
<collection-root>/
├── memroute.toml
├── .gitignore
├── platform/
│   ├── AGENTS.md
│   ├── memory/
│   │   └── api-operations.md
│   ├── tasks/
│   │   └── investigate-timeout/
│   │       └── task.md
│   └── scripts/
│       └── investigate-timeout/
└── .memroute/                  # generated, ignored by Git
```

Recommended ownership boundaries:

| Path | Purpose |
|---|---|
| `AGENTS.md` | L0 rules, knowledge navigation, workspace boundaries, and safety constraints. |
| `memory/` | Confirmed, reusable, and traceable facts or runbooks. |
| `tasks/<task-name>/task.md` | Task-specific goal, evidence, decisions, and execution history. |
| `scripts/<task-name>/` | Scripts used only by that task. |
| `.memroute/` | Generated Tantivy index and source-hash manifest; never a source of truth. |

## Configuration

Create `<collection-root>/memroute.toml` and register each workspace explicitly:

```toml
# Generated index and manifest locations, relative to this file.
index_dir = ".memroute/index"
manifest_path = ".memroute/manifest.json"

# Paths excluded while scanning workspace Markdown.
ignore_paths = ["backups", "generated", "data", ".memroute", ".git"]

[[workspace]]
name = "platform"
path = "platform"
enabled = true

[[workspace]]
name = "data"
path = "data"
enabled = true
```

Also ignore the generated index in `<collection-root>/.gitignore`:

```gitignore
.memroute/
```

Each enabled workspace must have a unique `name`, a directory under the collection root, and should provide an `AGENTS.md` with a `## 知识导航` section. A navigation table may link L2 files and identify the L3 authority boundary:

```markdown
## 知识导航

| 任务主题 | 优先加载的 L2 知识 | L3 核验/扩展边界 |
|---|---|---|
| API 运行与超时 | `memory/api-operations.md` | Service dashboard, deployment configuration, and API contract |
```

## Commands

All commands accept `--root <collection-root>`. JSON output is intended for agent integrations and includes paths and line ranges that should be read from the original source.

| Command | Purpose |
|---|---|
| `index [--force]` | Build or refresh the local index. Source hashes skip an unchanged collection unless `--force` is supplied. |
| `search <query>` | Search indexed chunks by text, workspace, kind, entity, or environment. |
| `route <query> --workspace <name>` | Generate a bounded L2 plan and, for high-risk work, L3 authority entries. |
| `audit` | Find broken knowledge-navigation references, unrouted memory files, and collection-size warnings. |
| `review --workspace <name> --task <task-name>` | Suggest reusable conclusions from a task record; it never edits knowledge files. |
| `impact <path>` | Show indexed chunks and likely navigation entries affected by a source path. |

### Index

```bash
memroute --root "$COLLECTION_ROOT" index
memroute --root "$COLLECTION_ROOT" index --force
```

### Search

Use `--kind` more than once to combine types. Valid values are `route`, `stable-fact`, `runbook`, `task-experience`, and `external-authority`.

```bash
memroute --root "$COLLECTION_ROOT" \
  search "timeout" \
  --workspace platform \
  --kind runbook \
  --entity api-gateway \
  --environment production \
  --json
```

### Route

`route --limit` must be between **3** and **8**. The default scope is the owning workspace; use `--include-workspace` only when the workspace navigation or task boundary justifies it.

```bash
memroute --root "$COLLECTION_ROOT" \
  route "migrate database storage" \
  --workspace platform \
  --intent migrate \
  --entity database-cluster \
  --environment production \
  --include-workspace data \
  --limit 6 \
  --risk high \
  --json
```

For `--risk high`, MemRoute returns `l3_authorities` in addition to `route_matches` and `l2_context`. Before acting, open the cited original systems and verify current state, permissions, and change requirements.

### Audit, review, and impact

```bash
memroute --root "$COLLECTION_ROOT" audit --json

memroute --root "$COLLECTION_ROOT" \
  review --workspace platform --task investigate-timeout --json

memroute --root "$COLLECTION_ROOT" \
  impact platform/memory/api-operations.md --json
```

## Agent Skill

The portable workspace-initialization Skill is located at:

```text
skills/agent-workspace-init/SKILL.md
```

Load or register that directory in an Agent-Skill-compatible environment. The Skill defines:

- the L0--L3 context model;
- workspace and task directory conventions;
- a minimal `memroute.toml` template;
- task-start, high-risk verification, audit, impact, and review workflows;
- the requirement that commands invoke this checked-out repository through `Cargo.toml`, rather than relying on a machine-specific installation path.

The Skill can be used as the governance layer for a collection while the `memroute` binary supplies local retrieval.

## Safety and privacy

MemRoute is designed to keep retrieval local, but the indexed source content remains the responsibility of the collection owner.

- Do not place credentials, tokens, cookies, private keys, personal data, full sensitive request bodies, or private endpoints in indexed files.
- Do not commit `.memroute/`, real task records, or an organization's `memroute.toml` and knowledge data to this public repository.
- Treat index output as a pointer to evidence, not as authoritative content.
- For production changes, permission changes, deletion, migration, or data-consistency work, use `--risk high` and verify the original L3 authority before making changes.

## Development

Run the verification suite from the repository root:

```bash
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
```

Before publishing changes, also inspect the tracked files for machine-specific paths and sensitive information. Repository-specific rules are maintained in [`AGENTS.md`](AGENTS.md).

## License

MemRoute is dual-licensed under **MIT OR Apache-2.0**, as declared in [`Cargo.toml`](Cargo.toml).
