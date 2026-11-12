# Known gaps to fix later

Found in reviews and left for a later release on purpose. Each says what goes wrong; remove an entry when it is fixed.

## Single program (verify on Windows before relying on it)

- An update while an MCP client is connected: the installer may end the MCP session (`databrain.exe mcp`), or hit "file in use" on the `databrain-mcp.exe` launcher.
- If an MCP client kills `databrain-mcp.exe` without closing its stdin, the child `databrain.exe mcp` may keep running (no job object ties them).
- `databrain-jobs run` / `list` output in a terminal goes through the GUI-subsystem launcher's console attach; check it prints in cmd and PowerShell.
