# tiberius 0.13.0, vendored

The crates.io release of tiberius 0.13.0 (`Cargo.toml` points
`[patch.crates-io]` here), with one opt-in addition for DataBrain:

- `Config::new_password(…)` sends LOGIN7's fChangePassword flag and the new
  password in ibChangePassword (obfuscated as the password is), so SQL
  Server changes the password as part of the sign-in. Once a password has
  expired (error 18487) or must be changed (18488) that is the only way the
  server takes a new one: it refuses the sign-in, so no session could run
  `ALTER LOGIN`. Used for SQL Server logins only (`crates/connectors/mssql`).
  Not set by default: nothing else changes. The new password stays in a
  `SecretString` (redacted from `Debug`, wiped on drop) until it is written
  into the zeroized LOGIN7 buffer, like the password.

Changed files: `src/tds/codec/login.rs` (the field, its encoding and tests),
`src/client/config.rs` (the setter), `src/client/connection.rs` (passed to
the login). `src/lib.rs` allows warnings (upstream code).

Removed from the copy: `tests/`, `examples/`, `docs/`, `docker/` (fixtures
of the TLS unit tests), CI and Nix files, `Cargo.lock`, `CHANGELOG.md`,
cargo's packaging files and the manifest's `[[test]]` / `[[example]]`
entries. To run its unit tests, copy it out of the workspace with
`docker/` from the crates.io release and run
`cargo test --lib --no-default-features --features tds73,rustls`
(all 959 pass). To update: copy the new release from `~/.cargo/registry`,
then apply the same change (`git log -p -- vendor/tiberius/src`). Offer it
upstream; drop the copy once a release has it.

Security advisories: `cargo deny` matches RustSec advisories against
crates.io packages, so one for tiberius isn't reported while this copy is
used. Check https://rustsec.org/packages/tiberius.html when updating, and
before a release.
