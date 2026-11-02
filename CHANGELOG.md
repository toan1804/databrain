# Changelog

Notable changes to DataBrain, newest first. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/). Dates are release tags (`git tag`).

## Unreleased

### Security

- Sign-in redirects: the local listener only answers requests addressed to itself (a page on another name resolving to 127.0.0.1 is refused) and only pages of origins the sign-in allows (Snowflake's SSO page); it no longer sends `Access-Control-Allow-Origin: *`. OpenRouter sign-in uses a free port and a random callback path instead of port 3000 without a state.
- With "Allow write statements" on, production still refuses and confirms: a query calling a function that could write needs the user's confirmation on Prod, and elsewhere asks instead of running automatically. T-SQL like `SELECT 1 EXEC xp_cmdshell 'whoami'` (no separator) no longer counts as a read, and each statement of a script is classified by its own keyword.
- Kiro: kiro-cli runs in a private folder of the user's (owner-only, under the user's cache folder) instead of a fixed folder in the shared temp directory, which another user could create first on a multi-user computer.
- `:name` values, load INSERTs and SQL export share one text escaper. On MySQL a value used `\'`, which `NO_BACKSLASH_ESCAPES` reads as a backslash and the end of the string; quotes are now doubled and backslashes doubled. Snowflake loads now escape backslashes, and BigQuery/Databricks loads use `\'` (they read `''` as two strings). A typed literal containing a backslash is quoted as a value instead of kept.
- ClickHouse and BigQuery identifiers escape backslashes inside backticks: a table or column name ending in `\` could close its quotes and add SQL.
- SSH: an SSH server's key is trusted only after you see its fingerprint and choose Trust; it was pinned silently on first use, and an imported connections file could pin an attacker's key in advance (imported fingerprints are now dropped).
- Oracle Instant Client is loaded only from a folder you chose, the one saved for connections, or the one DataBrain installed. A copy found in Downloads, the home folder or the library search path is shown with "Use the one found", not loaded and trusted on its own.
- run_query on a DuckDB connection (the Results connection too) checks every `results.<name>` it reads like query_outputs does: outputs from a connection with AI off, not shared with MCP, or not sharing result rows were readable there.
- query_result masks sensitive columns' values before the local query runs: a CTE column list (or any rename) returned them in clear.
- A query calling a non-built-in function needs the user's confirmation wherever the AI may not write, also on Postgres, MySQL, SQLite, DuckDB and ClickHouse when the connection isn't read-only (`select public.write_fn()`, `dblink_exec(…)` and `set_config('search_path', …)` ran unasked). On read-only connections, functions that act outside the read-only session (`dblink*`, `set_config`, server and file functions) still need it.
- MongoDB: commands are classified, and their collections found for table rules, with the shell parser that runs them. A `$out` or `$merge` stage written as `{ $out : 'copy' }` passed as a read and could replace a collection.
- `EXPLAIN` is classified by the statement it explains: `EXPLAIN (ANALYZE) DELETE …` and other option lists counted as a read, so the AI could run them (also on a production connection with auto-read), and so could the Plan tab's Analyze. A plain `EXPLAIN` of a write now counts as that write too.
- SQL from the AI, MCP and AI-run jobs runs on DuckDB in a sandbox (no file, network or extension access unless the connection allows it). Files attached to a DuckDB connection stay readable in it. Job steps whose SQL the AI wrote stay sandboxed when the job runs by hand, on its schedule or in the background, not only when the AI starts the run; a step you change yourself, or mark with "Trust this SQL", becomes yours. Jobs the AI made before this are found in the audit log and marked once.
- Jobs the assistant creates, changes or runs follow the connection's AI policy, including load steps and their before/after SQL.
- Functions that could write (e.g. `SELECT dbo.write_fn()`) need the user's confirmation wherever the AI may not write, not only on read-only connections.
- Read-only connections are enforced beyond the write classifier.
- PII masking holds under aliases, expressions and `SELECT *`.
- Passwords in connection strings go to the secret store; secrets in SQL, job logs, AI chat transcripts and the audit log are redacted, including what was stored before.
- Oracle Instant Client: the download is checked against Oracle's SHA-256, and the folder's libraries against a SHA-256 manifest at every launch, before they load.
- MCP: what DataBrain would ask the user about is asked through the client (MCP elicitation). Clients without elicitation are refused unless the connection turns on "Accept the MCP client's approval".
- Key-pair JWT signing moved from the `rsa` crate (RUSTSEC-2023-0071) to aws-lc-rs. Dependencies are checked with cargo-deny in CI.

### Added

- Parameter sets: save `:name` values under a name and apply them in any tab or notebook; jobs take parameters (a set and values of their own) that fill their steps on every run, can run once with another set, and take `--set`/`--param` from `databrain-jobs run`.
- Explorer: rows and size of a table on hover, from the database's statistics, on every engine (size where the engine keeps it).
- Load mode "Sync (mirror) by key" for job load steps and "Load into…": merge by key, then delete the table's rows that are no longer among the rows; the report counts the deletions, and a step without rows is refused. The assistant can use it in jobs.
- History: search by several words in any order across SQL, errors and connection names; filter by connection and status; run an entry again, or on another connection.
- Per-connection connect timeout and idle disconnect (General section), overriding Settings → Queries for that connection.
- ClickHouse is a load target for job load steps and "Load into…": new MergeTree tables with `Nullable` columns, Insert, Delete rows then insert (`TRUNCATE`), and Replace. Update and Merge by key are not offered there.
- Lineage: with "Check with database", views the SQL reads are traced through their definitions into the tables they read (views of views too).
- Amazon Redshift through the Postgres connector, with IAM sign-in through the AWS CLI.
- ClickHouse over its HTTP interface.
- ER diagram per schema.
- Inline AI ghost completion and SQL snippets in the editor.
- Saved grid views and per-table quick filters.
- Result grid: click a header to select the column, double-click to sort; the check Edit makes is shown before editing.
- Lineage: zoom controls and layout direction.
- Jobs: simpler step panels with outcomes; resume a failed run.
- Import connections from DataGrip; save imported connections without a user.
- Load into a table accepts table names in another case.
- Explorer arrow keys (tree navigation).
- Diagnostic log (`tracing`, daily rolling file).
- Developer ID signing, notarization and in-app updates for signed releases.

### Fixed

- Jobs: run logs are pruned. Each job keeps the full log of its latest 20 runs, within the history retention (Settings → Queries); older runs keep their summary.
- The window no longer goes blank on an unexpected error or a bad value kept in the window's storage: values are checked (and dropped when they don't fit), and an error shows what happened with Reload and "Reset window layout and reload".
- Assistant: an approval shows Approved or Declined only once DataBrain took the answer; if the assistant had already stopped waiting it says so, and a failed answer shows an error and can be sent again.
- Assistant: Stop takes effect while it waits for your approval (or for an editor change), instead of after the ten-minute wait.
- Oracle: loading timestamps with a time zone works; they were sent as text, which Oracle rejected, and are now `TIMESTAMP '… +02:00'` literals.
- Jobs: a scheduled run that fails to start no longer uses up its slot; it is tried again on the next check instead of waiting for the next period.
- Workspace: an older DataBrain refuses a workspace saved by a newer one (it could drop fields it doesn't know when saving) and says why; before updating an existing workspace, DataBrain keeps a copy beside it (`workspace.db.bak-before-v<N>`).
- Assistant: streamed answers no longer show � when a character (Vietnamese text, emoji) is split across network chunks, and tool-call arguments are no longer corrupted that way.
- Jobs no longer run twice when the computer sleeps past a run's lease: a run that lost its lease to another runner (the background runner) stops before its next step, and the lease renewal stops it too.
- Assistant: stopping it during a batch of tool calls no longer breaks the conversation (every later question failed at the provider). Calls that didn't run get a "not run" result, and conversations already affected are repaired when they continue.
- Lineage: recursive CTEs without a column list resolve the recursive step's columns (the anchor's names are used), and CTEs that read themselves without `RECURSIVE` (SQL Server, Oracle) are recognised instead of shown as database tables.
- Lineage: `WITH … UPDATE`, `WITH … DELETE`, `WITH … INSERT` and `WITH … MERGE` are traced through their CTEs instead of shown as not traced.
- MCP: a call the client cancels (`notifications/cancelled`) stops at once: DataBrain no longer keeps waiting for the user's answer or running its query.
- Opening a session on a server (a tab, the explorer, the assistant, a job) gives up after the connect timeout with a message, as "Test connection" does, instead of waiting for ever on a server that never finishes the handshake. The Oracle thin driver's first download is not counted.
- MCP: a query the client never answers no longer looks like a hang. DataBrain's question waits 2 minutes instead of 5, tells the client it is waiting (progress), then ends the call with a message naming the fixes (auto-read, or "Accept the MCP client's approval"), and asks no further question in that session. The connection's AI settings explain this when MCP is on with "Ask me every time".
- A password or key read from the OS keychain that waits on a permission prompt (another binary, such as databrain-mcp, reading an item the app saved) no longer blocks the app or the MCP server: the read runs off the async runtime and gives up after the connect time limit with a message that says what to allow.
- Refresh on a connection signs in again before listing, so rights granted since the login show in the explorer (Oracle applies roles to new sessions only). Tabs running a statement or in a transaction keep their session.
- ⌘↵ in a text field (dialogs, AI prompt, cell editor) no longer runs the active tab's SQL.
- Find in results: a slow answer for an older search no longer replaces the current one.
- Dialogs keep Tab inside, give focus back on close, and are named by their title; the first field gets focus instead of the Close button.
- The AI chat no longer jumps to the bottom while you read further up.
- Job steps no longer wait forever after their upstream ended.
- DuckDB: one shared database per file between sessions; cancel keeps the connection alive.
- Reserved words are quoted; no global shortcuts behind dialogs; no system context menu outside text fields.

### Changed

- Streaming answers re-render only the message that changed.
- The UI has ESLint (React hooks rules), component tests in jsdom and Playwright smoke tests.
- Large files were split by concern (store, explorer, AI panel, results, notebook, job editor, connection form, query engine, app API); query tabs and notebook cells share one editor setup.
- The UI's backend types are generated from Rust (ts-rs).
- CI moved to Node 22 and macOS 15 runners.

## 0.2.1 - 2026-10-25

### Added

- Scheduled jobs can run while DataBrain is closed (opt-in OS timer).
- Windows and Linux builds in CI, a release workflow and an update check.
- Charts: pie and scatter, and dashboards of pinned charts.
- Schema compare with a migration script.
- Connections: import from DBeaver, pgpass or a file; export without secrets; idle sessions disconnect; MongoDB and Neo4j.
- Result grid: value panel, follow foreign keys, edit cells and rows with a SQL preview, Load into a table, import files into tables, selection summary, transposed rows.
- Editor: open and save `.sql` files, query plans (EXPLAIN / EXPLAIN ANALYZE), a transaction indicator with Commit/Rollback and manual-commit tabs, next/previous statement keys.
- Completion finds tables and columns by their comments.
- Reopen closed tabs (⇧⌘T); whole-word find in results.
- Oracle: SQL*Plus substitution variables (`&name`, `&&name`); retry on TTC errors.
- Lineage follows chains across statements; load checks the target path first.
- AI: several conversations at once, auto-approve mode, fixes for connection errors, per-connection table read/write rules, guidance on how indexing works.
- MCP tool annotations (`readOnlyHint`, `destructiveHint`, `idempotentHint`).
- Source traits: what each database type is like, declared once per connector.

### Fixed

- SQL Server: TLS through rustls (macOS's TLS broke off handshakes with some servers); "Select top 100 rows"; databases listed.
- Export: typed Postgres arrays, exact decimals, PostGIS, Excel-safe CSV; a full export reads through the tab's open transaction.
- Statement splitting: procedure bodies, `CASE` in blocks, `WITH FUNCTION`, scripts without semicolons.
- Large results show in the grid; Oracle BLOB/CLOB type names with the thin driver.
- Runs stop at the row limit without losing session state.
- MCP was blocked by DataBrain.

## 0.2.0 - 2026-10-17

### Added

- AI and jobs: the assistant creates, manages, edits and fixes jobs, works in the Jobs tab and uses several connections at a time.
- AI budget and usage display; the AI panel is draggable.
- A build without the DuckDB engine (uses the DuckDB installed on the computer).
- Command palette lists jobs.
- History: shows the latest 50 queries, delete single entries, auto-delete settings; AI and MCP queries are hidden by default.
- Result grid shows each column's data type.
- Explorer shows functions and procedures by default; the explorer shows the cache first and refreshes after.
- Editor folding; Format on DDL.
- Oracle thin driver can be installed from a downloaded file and reconnects after a lost connection.
- Choose the terminal used for sign-ins.

### Fixed

- Jobs: Stop works, and a step that can't connect no longer hangs the run.
- Fix with AI fixes the statement that failed.
- AI remembers the conversation; the AI picker works like the model picker.
- The Downloads folder is no longer asked for at startup.
- Notebook layout and folder issues.

## 0.1.1 - 2026-10-13

### Added

- Exports run in the background.

### Fixed

- The model list re-rendered in the background and overflowed the window.

## 0.1.0 - 2026-10-08

First release.

- Connections to PostgreSQL, MySQL, SQL Server, Oracle (thin driver through go-ora, no Instant Client needed), SQLite, DuckDB, Snowflake, Databricks and BigQuery, with folders and a metadata cache.
- Editor with completion (aliases, functions, packages, procedures), formatting, `:name` parameters and tips for slow queries.
- Result grid with find, filters and export, including columns over 2 GB.
- Outputs: every result has a handle and can be queried with DuckDB as `results.<name>`, compared and pinned.
- Notebooks.
- Column and table lineage.
- Jobs scheduled while DataBrain runs.
- DDL for tables, views and functions.
- AI assistant with providers including Kiro, a knowledge index, notes and `@mentions` of outputs.
- Themes.
