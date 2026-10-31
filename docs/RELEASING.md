# Building and releasing

## Bundles

`npm run app:build` produces a self-contained app for the build machine (macOS: `target/release/bundle/macos/DataBrain.app` and a `.dmg`; Windows: `.msi`/`.exe`; Linux: `.deb`/`.AppImage`). The bundle includes `databrain-mcp` and the DuckDB extensions; SQLite, DuckDB and (on Linux) OpenSSL are linked statically. Build on each target platform (DuckDB extensions are platform-specific). External tools stay optional: Oracle Instant Client (offered in-app), Kiro CLI, and cloud CLIs for CLI sign-in.

## Builds without bundled DuckDB

`app:build:no-duckdb` (`scripts/build-system-duckdb.mjs`) builds with `--no-default-features --features system-duckdb` and no bundled extensions, and names the installers `DataBrain-no-duckdb_…`. The `duckdb` crate is then built without its bundled engine; its C functions are defined in `crates/duckdb-dynload` as calls into the library loaded at runtime (generated from libduckdb-sys by `scripts/gen-duckdb-dynload.py`; run it again when the `duckdb` crate version changes, it is pinned). `node scripts/build-system-duckdb.mjs --cargo <cargo args>` runs any cargo command for this build, e.g. its test with a real library: `DATABRAIN_DUCKDB_LIB=/path/libduckdb.dylib node scripts/build-system-duckdb.mjs --cargo test -p databrain-connector-duckdb --no-default-features --features system --test system_engine`.

## Releasing the app

Push a tag `vX.Y.Z` (matching `version` in `crates/app/tauri.conf.json`): the Release workflow (`.github/workflows/release.yml`) builds the installers for macOS (Apple Silicon, with and without DuckDB, and Intel), Windows and Linux and attaches them to a draft release. Publish the draft; running apps then see it in their update check. The builds are not code-signed. The CI workflow builds and tests every push and pull request on macOS, Linux and Windows.

## Oracle thin driver releases

The helper lives in `crates/connectors/oracle/agent` (Go). `node scripts/build-oracle-agent.mjs` builds every platform reproducibly into `target/oracle-agent/`, writes the gzip files to upload to `target/oracle-agent/dist/`, and updates `agent/SHA256SUMS` (compiled into DataBrain). Upload the `.gz` files to `{base}/v{RELEASE}/` (`agent::DEFAULT_BASE_URL`, `agent::RELEASE`; the `oracle_agent_url` setting or `DATABRAIN_ORACLE_AGENT_URL` override the base). `tauri dev` and `tauri build` build the helper for your computer when Go is installed (`target/oracle-agent/`), and DataBrain uses that copy directly, so builds made on a machine with Go never download it.

Releasing a new helper (each release gets its own download folder, so installed apps keep working with the one they were built for, and a new app downloads its own):

1. Bump `RELEASE` in `crates/connectors/oracle/src/agent.rs` (e.g. `0.1.1`).
2. `node scripts/build-oracle-agent.mjs` (Go 1.27; the sums depend on the Go version) and commit `agent/SHA256SUMS` with the change.
3. Publish the five `.gz` files under the tag `oracle-agent/v{RELEASE}`:

   ```sh
   gh release create oracle-agent/v0.1.1 target/oracle-agent/dist/*.gz \
     --repo toan1804/databrain --title "Oracle thin driver 0.1.1" --notes "Reconnects after a lost connection." --latest=false
   ```

4. Check one download before releasing the app (the URL DataBrain uses):

   ```sh
   curl -sL https://github.com/toan1804/databrain/releases/download/oracle-agent/v0.1.1/databrain-oracle-agent-darwin-arm64.gz | gunzip | shasum -a 256
   ```

   The sum must equal the `darwin-arm64` line of `agent/SHA256SUMS`. Then build and release the app as usual.
