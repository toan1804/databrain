# Security policy

## Supported versions

Security fixes go into the latest release. Older versions are not patched: update from the app's banner or the [Releases](https://github.com/toan1804/databrain/releases) page.

## Reporting a vulnerability

Please report vulnerabilities privately, not in a public issue:

1. Open the repository's **Security** tab and choose **Report a vulnerability** (GitHub private vulnerability reporting).
2. If that button is missing, open an issue titled "Security contact" with no details, and a maintainer will reply with a private channel.

Include what you can of:

- the DataBrain version and platform,
- the feature involved (a connector, the AI assistant, the MCP server, jobs, sign-in, export…),
- steps to reproduce, and what an attacker gains.

You can expect an acknowledgement within a week. Once a fix is released, the advisory is published and the [changelog](CHANGELOG.md) lists the fix under **Security**. Tell us if you want to be credited.

## Scope

DataBrain is a desktop app: it keeps connections in a local workspace and passwords in the OS keychain or its own encrypted vault. Reports we are most interested in:

- SQL from the AI assistant or MCP clients running where the connection's AI policy should stop it (writes on read-only or production connections, masked columns returned in clear, sandbox escapes on DuckDB);
- credentials leaving the keychain or vault, or being written to logs, history or exports;
- the app window (webview) reaching files, programs or settings beyond what its commands allow;
- sign-in redirects, SSH host keys, downloaded helpers (the Oracle driver, updates) that can be spoofed or replaced;
- SQL injection through names or values DataBrain quotes itself.

Out of scope: someone who already controls your user account or the app's data folder, and SQL you run yourself on your own connections.
