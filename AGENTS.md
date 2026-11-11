# Notes for AI coding agents

DataBrain is a Rust workspace (`crates/`) with a Tauri 2 app and a React UI (`ui/`). Start with [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) and [CONTRIBUTING.md](CONTRIBUTING.md).

- **Adding or changing a database type:** follow [docs/ADDING_A_CONNECTOR.md](docs/ADDING_A_CONNECTOR.md). Every DataBrain feature (connection, explorer, editor, result grid, plan, export, lineage, schema compare, jobs, AI, MCP) must either work for it or show the user why not. Plan feature by feature and show the plan before writing code.
- **Shared code:** before finishing a change, read [docs/CHANGE_IMPACT.md](docs/CHANGE_IMPACT.md) and check the other features that use what you changed (e.g. the lineage layout also draws the ER diagram). Known gaps left for later are in [docs/TODO.md](docs/TODO.md): add what you defer, remove what you fix.
- **Safety checks:** statement classification, read-only connections, the AI policy, PII masking and the DuckDB sandbox decide what may run. Changes there need tests for both the allowed and the refused case. When unsure, classify as `Unknown` (the user is asked), never as a read.
- **Generated files:** `cargo test` regenerates `ui/src/lib/bindings/*.ts`; `UPDATE_SOURCE_TRAITS=1 cargo test -p databrain-connector-core traits` regenerates `ui/src/lib/sourceTraits.json`. Commit them with the Rust change.
- **Checks:** `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets` (no warnings), `cargo +1.89 check --workspace --all-targets` (MSRV), `cargo test --workspace`, and `cd ui && npm run lint && npx tsc -b && npx vitest run`.
- **Commits:** `fix(scope): …`, `feat(scope): …`, `refactor(scope): …`, `docs: …`, with a body explaining why. Add a `CHANGELOG.md` line under **Unreleased** for user-visible changes.
