# go-ora 2.9.0, vendored

The release of github.com/sijms/go-ora/v2 v2.9.0 (the helper's `go.mod`
points `replace` here), with one opt-in addition for DataBrain:

- `ConnectionConfig.NewPassword` (in `configurations/database_info.go`):
  set, the sign-in sends it as `AUTH_NEWPASSWORD`, encrypted as the
  password is, with the change-password logon mode (0x2) and without the
  logon bit (`NoNewPass`, 0x1), as Oracle's own thin drivers do (with that
  bit the server ignores the new password and answers ORA-28001 again).
  Oracle changes the password as the user signs in, which is the only way
  it takes one once the password has expired. The helper's
  `change_password` request uses it (`../main.go`). Not set, nothing
  changes: other sign-ins send what they did.

Changed files: `auth_object.go`, `connection.go`,
`configurations/database_info.go` (each change is marked "DataBrain").

Removed from the copy: the `_test.go` files and `TestIssues/`, and the
module's test-only requirements (gorm, xorm) with its `go.sum`. To update:
copy the new release from `~/go/pkg/mod`, then apply the same change
(`git log -p -- crates/connectors/oracle/agent/go-ora`). Offer it upstream;
drop the copy once a release has it.

Security advisories: `govulncheck` and GitHub match advisories against the
module path, which this `replace` hides. Check
https://pkg.go.dev/vuln/list?q=go-ora and the go-ora releases when updating,
and before a helper release.
