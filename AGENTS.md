# MemRoute Repository Conventions

## Scope

This repository contains the portable `memroute` CLI and the `agent-workspace-init` skill. It must not contain any user's workspace knowledge, task records, generated indexes, credentials, private URLs, machine-specific paths, or organization-specific operating details.

## Repository Layout

- `src/`: Rust implementation and unit tests.
- `skills/agent-workspace-init/SKILL.md`: reusable skill for initializing an agent workspace and using MemRoute.
- `Cargo.toml` and `Cargo.lock`: CLI package definition and locked dependencies.
- `.gitignore`: excludes local build artifacts and generated indexes.

## Implementation Rules

- Keep the CLI portable: accept collection locations through `--root`; when omitted, use the process working directory. Never add a machine-specific default path.
- Treat a collection's `memroute.toml`, `AGENTS.md`, `memory/`, and `tasks/` as user-owned input. Do not add real collection data to this repository.
- Keep indexing local and rebuildable. `.memroute/`, including the default `.memroute/events.jsonl` event log, is derived data and must stay untracked.
- Event logging is local and opt-out: commands write to `.memroute/events.jsonl` by default; `--event-log <path>` selects another local JSONL path and `--no-event-log` disables logging. A feedback command must use the same event log that produced the route `request_id`.
- Event records may contain only command names, duration, index fingerprint, workspace/scope, risk, opaque request/chunk IDs, candidate ranks/kinds and hashed opaque task/intent IDs. They must never contain query text, entities, environment values, paths, headings, titles, excerpts, URLs, source bodies, credentials, personal data, private hostnames, internal URLs, or full sensitive request payloads.
- `feedback` is append-only caller input: validate each `chunk_id` against the route request before recording `used`, `background`, `rejected` or `needs-l3`; use `blocked` with no chunk ID when execution is stopped for missing authority or another safety boundary.
- Never add passwords, tokens, cookies, private keys, personal data, private hostnames, internal URLs, or full sensitive request payloads.
- Prefer generic examples such as `example.com`, `<collection-root>`, and `<memroute-repository>`.
- Preserve the L0--L3 workflow: routing output is evidence selection, not a replacement for reading source sections or verifying L3 authorities.

## Change and Verification Workflow

1. For behavior changes, update or add focused unit tests in `src/main.rs`.
2. Follow the repository-owned `rustfmt.toml`; run `cargo fmt --check`, `cargo test`, and `cargo clippy --all-targets -- -D warnings` from this repository.
3. Treat warnings as errors. The `Cargo.toml` `[lints]` table enables Clippy `all`, `pedantic`, `nursery`, and `cargo` lint groups; new suppressions require a short local justification. `cargo_common_metadata` remains temporarily allowed only until the repository owner sets the public `package.repository` URL.
4. Before committing, scan tracked content for machine paths and sensitive data.
5. Do not commit build output, `.memroute/`, or a caller's collection configuration and knowledge files.

## Skill Maintenance

The skill must remain self-contained and portable. Its day-to-day command examples must prefer an installed `memroute` binary on `PATH`; document the local event-log and `feedback` contract without embedding organization-specific paths, identifiers or event data:

```bash
memroute --root <collection-root> index
```

Use `cargo run --manifest-path <memroute-repository>/Cargo.toml -- --root <collection-root> <command>` only for first installation or binary upgrades, and for developing, debugging, or validating source that is not installed. After source validation, use `cargo install --path <memroute-repository> --force` to update the `PATH` binary. When users run the command from `<collection-root>`, `--root <collection-root>` may be omitted. Keep the skill's governance guidance aligned with the CLI command names, event schema and feedback contract.
