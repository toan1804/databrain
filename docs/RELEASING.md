# Building and releasing

## Bundles

`npm run app:build` produces a self-contained app for the build machine (macOS: `target/release/bundle/macos/DataBrain.app` and a `.dmg`; Windows: `.msi`/`.exe`; Linux: `.deb`/`.AppImage`). The bundle includes the `databrain-mcp` and `databrain-jobs` launchers (which start `databrain mcp` / `databrain jobs`: one copy of the engine) and the DuckDB extensions; SQLite, DuckDB and (on Linux) OpenSSL are linked statically. Build on each target platform (DuckDB extensions are platform-specific). External tools stay optional: Oracle Instant Client (offered in-app), Kiro CLI, and cloud CLIs for CLI sign-in.

## Builds without bundled DuckDB

`app:build:no-duckdb` (`scripts/build-system-duckdb.mjs`) builds with `--no-default-features --features system-duckdb` and no bundled extensions, and names the installers `DataBrain-no-duckdb_…`. The `duckdb` crate is then built without its bundled engine; its C functions are defined in `crates/duckdb-dynload` as calls into the library loaded at runtime (generated from libduckdb-sys by `scripts/gen-duckdb-dynload.py`; run it again when the `duckdb` crate version changes, it is pinned). `node scripts/build-system-duckdb.mjs --cargo <cargo args>` runs any cargo command for this build, e.g. its test with a real library: `DATABRAIN_DUCKDB_LIB=/path/libduckdb.dylib node scripts/build-system-duckdb.mjs --cargo test -p databrain-connector-duckdb --no-default-features --features system --test system_engine`.

## Releasing the app

Push a tag `vX.Y.Z` (matching `version` in `crates/app/tauri.conf.json`): the Release workflow (`.github/workflows/release.yml`) builds the installers for macOS (Apple Silicon, with and without DuckDB, and Intel), Windows and Linux and attaches them to a draft release. Publish the draft; running apps then see it in their update check (and, with an updater key, `latest.json` for in-app updates). The CI workflow builds and tests every push and pull request on macOS, Linux and Windows.

## Checksums and provenance

Each installer and updater bundle gets a [build provenance attestation](https://docs.github.com/actions/security-for-github-actions/using-artifact-attestations) from the Release workflow, and the release gets a `SHA256SUMS` file listing every file on it (also attested). To check a download:

```sh
shasum -a 256 -c SHA256SUMS --ignore-missing          # the file matches the release
gh attestation verify DataBrain_0.3.3_aarch64.dmg --repo toan1804/databrain   # built by this repository's workflow
```

`SHA256SUMS` is written after the builds and `latest.json`; a file attached to the draft by hand afterwards is not in it (run the workflow again, or the checksums job, before publishing).

## Signing and in-app updates

Each part turns on when its secrets are set (Settings → Secrets and variables → Actions) and is skipped otherwise, so unsigned builds keep working.

### macOS: Developer ID and notarization

Without it, macOS blocks the first open of a downloaded build (right-click → Open, or System Settings → Privacy & Security → Open Anyway).

| Secret | What |
| --- | --- |
| `APPLE_CERTIFICATE` | The "Developer ID Application" certificate with its private key, exported from Keychain Access as `.p12`, base64: `base64 -i cert.p12 \| pbcopy` |
| `APPLE_CERTIFICATE_PASSWORD` | The password chosen when exporting the `.p12` |
| `APPLE_SIGNING_IDENTITY` | The certificate's name, e.g. `Developer ID Application: Your Name (TEAMID1234)` (`security find-identity -v -p codesigning`) |
| `APPLE_ID` | The Apple ID of the developer account (notarization) |
| `APPLE_PASSWORD` | An app-specific password for that Apple ID (appleid.apple.com → Sign-In and Security) |
| `APPLE_TEAM_ID` | The team ID (10 characters) |

The workflow imports the certificate into a temporary keychain, Tauri signs the app with the hardened runtime and `crates/app/Entitlements.plist`, sends it to Apple's notary service and staples the ticket, and the workflow checks the result with `codesign`, `spctl` and `stapler`.

The entitlements allow loading libraries DataBrain did not sign (`com.apple.security.cs.disable-library-validation`): DuckDB's extensions, a DuckDB library installed on the computer, and Oracle Instant Client. The DuckDB extensions are shipped gzipped in signed builds (`DUCKDB_EXT_GZIP=1` for `fetch-duckdb-extensions.mjs`) and unpacked into the app-data folder at start: the notary service rejects executable code that isn't signed with the Developer ID, and signing the extensions ourselves would break DuckDB's own signature on them.

### In-app updates (Tauri updater)

With an updater key, the update banner installs the new version and restarts the app instead of linking to the download. Updates are verified against the public key built into the app.

1. Make a key pair once, and keep the private key and its password safe (losing them means existing installs can't update in place any more):
   ```sh
   cd ui && npx tauri signer generate -w ~/.tauri/databrain-updater.key
   ```
2. Secrets: `TAURI_SIGNING_PRIVATE_KEY` (the content of `~/.tauri/databrain-updater.key`) and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`.
3. Variable (not secret): `TAURI_UPDATER_PUBKEY`, the content of `~/.tauri/databrain-updater.key.pub`.

The builds then also make updater bundles (`.app.tar.gz` on macOS, the NSIS installer on Windows, the AppImage on Linux) with `.sig` files, and the "Updater manifest" job writes `latest.json` on the release (`scripts/updater-manifest.mjs`). The app reads `https://github.com/toan1804/databrain/releases/latest/download/latest.json`; each build looks up its own entry (`darwin-aarch64`, `darwin-aarch64-no-duckdb`, `darwin-x86_64`, `windows-x86_64`, `linux-x86_64`). Builds made without the key, and Linux `.deb`/`.rpm` installs, keep the download link.

The first release built with the key still has to be installed by hand; updates from it on install in place.

### Windows

Not code-signed yet: SmartScreen warns on the first run.

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
