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
- Keep indexing local and rebuildable. `.memroute/` is derived data and must stay untracked.
- Never add passwords, tokens, cookies, private keys, personal data, private hostnames, internal URLs, or full sensitive request payloads.
- Prefer generic examples such as `example.com`, `<collection-root>`, and `<memroute-repository>`.
- Preserve the L0--L3 workflow: routing output is evidence selection, not a replacement for reading source sections or verifying L3 authorities.

## Change and Verification Workflow

1. For behavior changes, update or add focused unit tests in `src/main.rs`.
2. Run `cargo fmt --check` and `cargo test` from this repository.
3. Before committing, scan tracked content for machine paths and sensitive data.
4. Do not commit build output, `.memroute/`, or a caller's collection configuration and knowledge files.

## Skill Maintenance

The skill must remain self-contained and portable. Its command examples must invoke the checked-out CLI through:

```bash
cargo run --manifest-path <memroute-repository>/Cargo.toml -- --root <collection-root> index
```

When users run the command from `<collection-root>`, `--root <collection-root>` may be omitted. Keep the skill's governance guidance aligned with the CLI command names and output contract.
