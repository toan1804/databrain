# Contributing to DataBrain

Thanks for helping. Bug reports, fixes and new database types are welcome. For a vulnerability, follow [SECURITY.md](SECURITY.md) instead of opening an issue.

## Before you start

- For a bug, open an issue with the version, platform, database type and steps to reproduce.
- For a feature or a larger change, open an issue first so we can agree on the approach.
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) explains how the crates fit together; §5.4 covers adding a database type.

## Building and testing

The requirements and commands are in the README ([Build from source](README.md#build-from-source), [Test](README.md#test)). Before opening a pull request, run:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets   # no warnings
cargo test --workspace
cd ui && npm run lint && npm test && npm run build
```

CI runs the same checks, plus `cargo deny check`, the UI smoke tests and the database tests against service containers.

- `cargo test` regenerates the TypeScript bindings in `ui/src/lib/bindings`. Commit them with the Rust change.
- Tests that need a database server are `#[ignore]`d. Run the ones your change touches with `--ignored` and the `DATABRAIN_*` variables listed in the README.
- The Oracle driver helper (`crates/connectors/oracle/agent`, Go) is pinned by `agent/SHA256SUMS`. Changing it, or its Go dependencies, needs a new helper release; see [docs/RELEASING.md](docs/RELEASING.md).

## Fuzzing and coverage

The splitter, classifier and identifier quoting have cargo-fuzz targets in `fuzz/` (nightly Rust): `cargo +nightly fuzz run split_classify` from `fuzz/`. A failing input is a bug in a safety check: add it to a test before fixing. The properties are in `connector-core/src/invariants.rs`. The "Fuzz and coverage" workflow runs the targets and measures coverage weekly; run it by hand from the Actions tab.

## Pull requests

- One change per commit, with a message like `fix(jobs): …`, `feat(ui): …` or `perf(engine): …`.
- Add tests for what you fix or add, and a line to `CHANGELOG.md` under **Unreleased** (Security, Added, Changed or Fixed).
- Keep safety checks intact: anything that decides whether SQL is a read, a write or needs confirmation (`connector-core` classification, the AI policy, the DuckDB sandbox) needs tests showing both the allowed and the refused case.
- Don't commit credentials, connection files or `.env` files.

## License

By contributing, you agree that your contributions are dual licensed under the [Apache License 2.0](LICENSE-APACHE) and the [MIT license](LICENSE-MIT), as stated in the README.
