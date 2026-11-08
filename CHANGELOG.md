# Changelog

Notable changes to DataBrain, newest first. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/). Dates are release tags (`git tag`).

## Unreleased

### Added

- Settings → Editor: inline AI completion can use its own provider and model instead of the AI panel's, e.g. a local Ollama model while the assistant uses Kiro (which can't complete inline). A model chosen there is used over the provider's fast model.
- Documented joins: when you tell the assistant, or a document or data dictionary says, which columns two tables join on, it records the join with its source (`add_knowledge_join`, also over MCP). Tables and columns are checked against the index; the join is used by `find_join_path` and `describe_table` without being seen in queries, and stays when learning is off or forgotten. The assistant's joins wait in Knowledge → Learned under "To review" (approve or discard on the line) unless the connection saves AI notes directly.
- Knowledge tab: Index, Notes and Learned are separate tabs (Notes shows a badge for notes the AI proposed). Learned lists one join per line with a status dot, a filter by table or column and by status (given to the AI, waiting, rejected), nothing to confirm (the AI gets a join from use: 2 queries or 2 days), reject a wrong one (or undo) on the line, and the details, queries and "Make note" when you open it; a schema shared by every table is said once. Most used tables have their own list.
- Compare tab: "Show hidden characters" shows changed cells character by character, so a no-break space, a tab, a zero-width space, a BOM, an accent written apart or a badly decoded `�` can be seen; the characters that differ are highlighted and named on hover. Values that only differ in such characters are flagged "unseen".
- Compare outputs: one column can be compared with several (`debit_amt = debit_amt`, `credit_amt = credit_amt` and `debit_amt = credit_amt`); pairs typed under the columns are added next to the matched ones instead of replacing them, each with its own result column (`debit_amt → credit_amt`). Each compared pair has a ✕ to leave it out.
- Compare tab: the Before card is red and the After card green, the same colours as the old → new values in changed cells, which now have a light red or green background.
- Compare outputs: time-zone timestamps (Databricks, Snowflake, BigQuery…) compare and show in UTC, as the grid shows them, so `2026-11-04 00:00:00+00:00` equals an Oracle `2026-11-04 00:00:00` instead of becoming your computer's local time. Text with an offset (`…+07:00`, `…Z`) is read the same way.
- Compare data without writing queries: a side of a compare can be a table on any SQL connection or a CSV, TSV, Parquet, JSON or Excel file, read when you pick it and again on "Compare again". Right-click a table → "Compare data with…" (the same table on another connection), or "Compare with…" in a result's toolbar. Key columns are suggested (the table's primary key, else columns unique on both sides).
- Compare outputs opens a compare tab instead of a raw SQL query: counts of rows that are the same, changed, only before and only after (each one a filter), changed rows per column, changed cells shown as before → after, "Only columns that differ", a warning for duplicate keys, and Keys & columns, Compare again, Open SQL and As output.
- Compare outputs: options "Ignore spaces at the ends", "Ignore case", "Empty text = NULL" and a number tolerance, and a "Compare as" choice per column (date/time, date only, number, text, boolean, exact) showing both columns' types.
- The AI learns how tables join from query history (Edit connection → AI → "Learn from query history", on by default): join keys from queries you and your jobs ran, including tables without declared foreign keys, and which tables are used most. A join is given to the AI after 2 different queries or 2 days of use; the AI's own queries never count. Review, reject or forget them in Knowledge → Learned.
- `find_join_path` tool (assistant and MCP): the shortest chains of joins between two tables over declared foreign keys and learned joins.
- Clear history → "Also forget what the AI learned from history".

### Changed

- Settings → "MCP / Kiro" is now "MCP": the MCP server works with any agent that supports MCP (Claude Code, Cursor, Claude Desktop, Windsurf, Kiro CLI, …); the page says where common agents keep their MCP settings and gives the command for agents with another format.

### Fixed

- Compare outputs reported equal values as different when the two databases typed them differently: an Oracle `DATE` (`2026-11-11 00:00:00`) against `'2026-11-11'` as text, `10.50` against `'10.5'`, padded `CHAR` against `VARCHAR`, `1` against `'1'` as a key. Without keys every such row showed as removed and added; with keys, as changed on every column. Values now compare by meaning.
- DuckDB: a tab or notebook opening a database file just as the last session on it closed could open a second instance on the file and miss tables written just before ("Table … does not exist") or fail with "No more data remaining in MetadataReader" (Linux). It now waits for the closing instance.

## 0.3.1 - 2026-11-04

### Changed

- MCP: with "Accept the MCP client's approval" on, the client's own tool approval is enough for what DataBrain would ask about, with any "AI may run queries" setting. Before, it only counted for clients without elicitation, so a client that declared elicitation was still asked (or refused when it didn't show the question), and reads needed "Automatically for read-only queries".
- MCP: a call refused for want of the user's approval tells the agent how the user allows it (Edit connection → AI → "Accept the MCP client's approval").
- MCP: `create_job`, `update_job` and `run_job` are no longer offered to MCP clients. Jobs are created, changed and run in DataBrain's assistant; `list_jobs` and `get_job` still work over MCP.
- Large source files are split into modules (connector `lib.rs` files, `tools`, `jobs`, `sql`, `databricks`, the DuckDB trampolines) with no change in behaviour, and all clippy warnings are fixed.

### Added

- `docs/ADDING_A_CONNECTOR.md`: a guide for developers and AI agents on adding a database type so that every feature works for it or says why not, with a support matrix of the current connectors. `AGENTS.md` and a Kiro steering file point to it.

### Security

- A Databricks CLI profile's token is sent only to the profile's own host; a connection to another host no longer uses it.
- Query history, job logs and job run errors redact secrets like the AI audit log does (seven common password forms: `PASSWORD =`, `ATTACH 'postgres://u:pw@h'`, `MASTER_PASSWORD=`, `pwd:`, `@rmtpassword=` and others), including what was stored before.

## 0.3.0 - 2026-11-03

### Security

- query_outputs (and run_query on DuckDB) mask an output's sensitive columns as DuckDB loads `results.<name>`, as query_result already did. They only refused SQL that named a sensitive column, so `SELECT *` under a CTE column list could rename one, and a failing cast quoted its value in the error sent to the model. Now the session only sees `***`, the SQL may name the column, and an output that wasn't checked is refused. In a DuckDB file other sessions share, outputs with sensitive columns are refused (use query_outputs).
- async-executor 1.14.0 (through zbus) was yanked: updated to 1.14.1. RUSTSEC-2026-0429 (zip `extract()` aborting on a huge declared size) only reaches zip copies used on trusted archives (DuckDB's build script, signed updates), so it is listed with the other zip exceptions.
- Rust 1.88 is now the minimum (was 1.87), which lets in tiberius 0.13 (SQL Server TLS on rustls 0.23 and rustls-webpki 0.103), time 0.3.55, and zip 9 through rust_xlsxwriter 0.102. Seven advisory exceptions are gone from `deny.toml`; the three zip ones remain for zip copies DataBrain doesn't choose (DuckDB's build script, the updater). CI checks the workspace still builds on 1.88.
- Lineage and ER diagrams only use appearance colours that are plain CSS colours (hex, `rgb()`/`hsl()`, a colour name). A custom colour containing quotes or markup went into the SVG unescaped; it now falls back to the default colour.
- Sign-in and Kiro terminal launchers are written to a private folder in your cache folder, with a new name each time and never over an existing file. They had fixed names in the shared temp folder, where another user could plant a script of that name first. Old launcher scripts are removed after a day.
- Credential vault: on Windows, `vault.json` and `vault.key` get an explicit access list (your account and SYSTEM, nothing inherited) before anything is written to them. The key, the decrypted file and the cached passwords are wiped from memory when no longer used, and encryption happens in place so no copy of the plaintext is left behind in freed memory.
- Exports write to a new temporary file created beside the chosen file (never an existing file or link of that name, which was truncated or written through before) and rename it when done. A failed or cancelled export leaves the previous file whole.
- MCP server: a message from the client is limited to 8 MiB. A longer line is answered with an error and skipped as it arrives, instead of being collected in memory without limit.
- Job export steps no longer overwrite files they didn't write. If the file name is taken by another file (or a link), the rows are saved as `name (2)`, `name (3)`… and the run says so; a file an earlier run of the same job wrote is still updated. The file is written next to its destination and moved into place, so a failed or cancelled export leaves the previous file whole.
- Oracle: the downloaded thin driver is checked against its SHA-256 every time DataBrain starts it, not only when it was installed. A file changed afterwards (or a link put in its place) is not run, and is downloaded again.
- Paths sent by the app window are checked before a file is read or written: absolute paths only, no hidden files or folders (except for importing from `.pgpass`), no writing through a symbolic link, and each command accepts only its own file types (an export writes `.csv`, `.parquet`…, a query tab never saves a `.command` or `.dylib`). Loading a DuckDB library needs a file named like one (`libduckdb.dylib`, `libduckdb.so`, `duckdb.dll`) and a confirmation in a native dialog, and the saved library path and Oracle driver URL can no longer be changed through the generic settings command.
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

- Fuzzing and coverage: cargo-fuzz targets for the statement splitter and classifier and for identifier quoting (`fuzz/`), checking that spans cover the text in order, that a comment or whitespace never turns a write into a read, and that a quoted name always reads back as one name. A seeded test runs the same checks on generated input in every `cargo test`. A weekly workflow (also manual) fuzzes each target and measures Rust and UI test coverage (cargo-llvm-cov, Vitest v8).
- Releases include a `SHA256SUMS` file for every file attached, and each installer has a GitHub build provenance attestation (`gh attestation verify <file> --repo toan1804/databrain`).
- SECURITY.md (how to report a vulnerability privately), CONTRIBUTING.md and a Dependabot configuration for Rust, npm, GitHub Actions and the Oracle helper's Go modules.
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

- ClickHouse: when a result breaks off partway (ClickHouse 25.x does that when the query fails after sending rows, sometimes without the error message), the error says so, with how many rows arrived and the query_id to look up in `system.query_log`, instead of only "error decoding response body".
- DuckDB: opening a database file right after its last session closed (a reconnect, closing a tab and reopening it) could fail with "Cannot open file …wal": the new session opened a second instance while the old one was still closing the file (on Windows: "File is already open"). A query's task finishing after its session ended could also keep the file open past its close. The file now closes after its last connection, and a new session waits for that.
- The app's code is no longer one 1.9 MB file: React, the editor and the grid are separate files, and the DDL, job, lineage, schema compare and ER diagram tabs (with the SQL formatter) load when first opened. Code read at startup is about 1.6 MB, and the build warns again above 800 kB per file.
- Job logs shown live in the app keep each run's latest 5,000 lines and the latest 10 runs; they grew for as long as the app stayed open.
- A table whose name contains a dot (`"x.y"`) is quoted as one name when editing rows, opening related rows and counting rows on SQLite; it was split into two parts.
- The per-tab lock used while connecting is removed once nobody waits on it; one was kept for every tab and job step ever opened.
- A very large idle-disconnect time (minutes) no longer overflows when turned into seconds; it means practically never.
- Two runs started at the same moment in one tab (or on one session) could both start; checking for a running query and claiming the tab now happen together.
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
