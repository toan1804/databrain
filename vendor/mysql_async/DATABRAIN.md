# mysql_async 0.36.1, vendored

The crates.io release of mysql_async 0.36.1 (`Cargo.toml` points
`[patch.crates-io]` here), with one opt-in addition for DataBrain:

- `OptsBuilder::can_handle_expired_passwords(true)` sends the
  `CLIENT_CAN_HANDLE_EXPIRED_PASSWORDS` capability. With it, MySQL lets an
  expired password sign in to a session that takes only `ALTER USER` /
  `SET PASSWORD` (without it the server refuses the sign-in with error 1862),
  which is how DataBrain changes an expired password
  (`crates/connectors/mysql`). Off by default: nothing else changes.

Removed from the copy: `tests/`, the CI file, `Cargo.lock`, `README.tpl` and cargo's packaging files
(and the manifest's `[[test]]` entries). Warnings are allowed in `src/lib.rs`
(upstream code). To update: copy the new release from `~/.cargo/registry`,
then apply the same change (`git log -p -- vendor/mysql_async/src`). Offer it
upstream; drop the copy once a release has it.

Security advisories: `cargo deny` matches RustSec advisories against
crates.io packages, so one for mysql_async isn't reported while this copy
is used. Check https://rustsec.org/packages/mysql_async.html (and
mysql_common, still from crates.io) when updating, and before a release.
