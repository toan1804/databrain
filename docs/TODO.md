# Known gaps to fix later

Found in reviews and left for a later release on purpose. Each says what goes wrong; remove an entry when it is fixed.

## Single program (verify on Windows before relying on it)

- An update while an MCP client is connected: the installer may end the MCP session (`databrain.exe mcp`), or hit "file in use" on the `databrain-mcp.exe` launcher.
- If an MCP client kills `databrain-mcp.exe` without closing its stdin, the child `databrain.exe mcp` may keep running (no job object ties them).
- `databrain-jobs run` / `list` output in a terminal goes through the GUI-subsystem launcher's console attach; check it prints in cmd and PowerShell.

## Connector core (review of 2026-11-12; see [CONNECTOR_CORE.md](CONNECTOR_CORE.md))

Every finding of the review was fixed on 2026-11-13, with the never-read corpus and the whole-vs-pieces property (`crates/connector-core/tests/classify_corpus.rs`), tri-state `Session::transaction_state` with the unused `Capabilities` fields removed, keyword and positional secrets in redaction, and one scanner for strings and comments (`skip_token`). A `Session::supports(Feature)` enum wasn't added: the optional features already say "not supported" through their own return values (`object_versions` → `None`, `check_sql` → `false`). The redaction functions keep their own, dialect-free lexing on purpose: they scrub any text (history, logs, error messages) without knowing its database. Since 2026-11-14 the SQL secret rules read one tokenizer in six readings, sessions run only `Checked` statements or `Statement::internal` SQL, and each statement is parsed once (`ParsedStatement`); the splitter stays the authority on where statements end, so a gap in sqlparser's tokenizer can't become a split failure.

Idea still open:

1. **Function allow-list for reads** (medium; a product decision). Unparsed text, hidden statements and the acting-function list are covered; left is treating a SELECT that calls a function outside the built-in list as `Unknown`. Only SQL Server (functions can run extended procedures) and Oracle (autonomous-transaction functions) need it, and there it would refuse many ordinary reads on read-only connections (`regexp_count`, `sys_context`, `dbms_lob.substr`, any user function) until the built-in lists are complete. The AI path already asks about such functions (`unknown_functions`).

## AI assistant (review of 2026-11-14)

The review's privacy gaps and bugs were fixed on 2026-11-15 (a connection's rules hold wherever it is reached from, jobs are reviewed again when they run, the row cap, stream errors, the job tools' silent fallbacks). Left:

- **Tool arguments are read from loose JSON.** The job tools now refuse a schedule or an export format they can't read, but the other tools still take each field by hand (`args.get(..)`), with undocumented aliases (`insert`, `upsert`, `mirror`). Typed structs (`deny_unknown_fields`) that also produce the schema sent to the model would remove the class.
- **Table rules and several schemas on a `search_path`**: an unqualified name that may resolve in more than one schema (Postgres `search_path` of several, a SQL Server schema other than `dbo`), or whose schema the server couldn't say, is refused by rules that depend on its schema. Resolving each name on the server (`to_regclass`, `OBJECT_ID`) would allow the ones that are unambiguous. Checked live on Postgres only, not on SQL Server, Snowflake or Databricks.
- **PII rules and non-SQL views**: MongoDB views (`viewOn`) and other non-SQL sources aren't traced through their definitions; their columns are masked by name only (a rule on a table masks the same column name in the view).
- **Notes about dropped or renamed tables stay** (hidden from the model, never removed), and there is no limit on the number of notes or proposals.
- **A schema that becomes empty keeps its tables in the AI's catalog** until the next full index (not confirmed; `kn_replace_schema_read` with an empty list).
- **Workspace SQLite on the async runtime**: `knowledge::retrieve` and a schema's index write run on the runtime's threads; a learn pass commits once per sighting.
- **Providers**: the OpenAI-compatible stream is still read as complete when it ends without `[DONE]` or a finish reason (some local servers end that way); Gemini's `thoughtSignature` is not kept with a tool call (newer models may want it back; not confirmed); the SSE reader has no cap on one line or one event; inline completions cancelled before the first byte are counted as zero tokens.

## Connectors (review of 2026-11-15)

Fixed on 2026-11-15: catalog queries built with a literal a backslash could end (MySQL, Snowflake, Databricks, Redshift, Postgres, BigQuery paths), the crashes on text outside ASCII (MongoDB, Neo4j), Databricks running a write twice, `results.<name>` hidden behind a comment, and the DuckDB sandbox writing into attached folders. Left, most serious first:

**Stop and the row limit** (fixed on 2026-11-15 and 2026-11-16 except as follows)
- **Snowflake, BigQuery, Databricks:** the cancel on a failed poll is tested against a mock server only, not a live account; a Databricks or BigQuery submit that fails before the server answers can't be cancelled (no statement or job id yet).
- **Not tested live:** Oracle (Instant Client): Stop and the wait-for-session check; SQL Server: Stop, the row limit on `EXEC`, reconnect sign-in (SQL Server 2022 crashes under emulation on the arm64 podman VM, and Azure SQL Edge never finished a login); ClickHouse: `KILL QUERY` at the row limit (fake server only). The Oracle helper's exit when DataBrain goes is tested in Go, not against a server.
- **MongoDB:** a command with the user's own `comment`, or on a server before 4.4, isn't tagged, so Stop only drops it here (an open cursor is still killed). **Neo4j:** a tight loop notices `TERMINATE TRANSACTION` only at its next check.

**Wrong results or state** (fixed on 2026-11-15 except as follows; the rest was checked by unit tests only, live where noted)
- **Oracle:** the Instant Client (thick) path, its per-statement read-only transaction and `DBMS_OUTPUT` (32,767-byte binds), is not tested live (the thin one is, on Oracle Free 23). Read-only is not enforced by the server (confirmed live on 23): a `COMMIT` inside a block, or DDL, ends the READ ONLY transaction, and what is committed after it stays; the classifier refuses blocks and DDL on a read-only connection, and what isn't committed is rolled back as the statement ends.
- **SQL Server:** `money` past about ±900 billion is read by the driver (tiberius) as a float, so its last digits are lost (a notice says so); `datetimeoffset` is text with its offset, so it sorts as text, not by instant (sorting by instant needs the result store and outputs to know the type).
- **SQLite:** a value that doesn't fit its column's type after the first batch shows as NULL, with a notice: the column types are fixed by the schema sent before the rows stream; fixing it needs the result store to widen a column mid-stream.
- **Neo4j:** whether the server decodes `\uXXXX` outside strings is not confirmed; the classifier reads both ways. Durations are read from the driver's debug output (neo4rs 0.8.0): recheck when upgrading.
- **Databricks:** the 100,000-row metadata check relies on `manifest.truncated` (as documented; not seen on a live workspace).
- **ClickHouse:** the mid-stream exception fix is tested against a fake server only.

**Connections and secrets**
- **TLS:** Postgres and MySQL `Prefer` and `Require` without a CA file encrypt without checking the certificate (libpq's meaning); give a CA file (`ssl_root_cert`) to verify. The Postgres CA file is not tested against a TLS server.
- **MySQL:** the live connection inside mysql_async keeps its own copy of the connection options (the password with them); side connections (Stop, replacement) read it again.
- **Redshift read-only** isn't enforced by the server as far as DataBrain knows (`SET SESSION CHARACTERISTICS` is deprecated there): DataBrain's classifier refuses writes, and function calls ask.

## Review of v0.4.1..main (2026-11-15)

A review of the 54 commits since v0.4.1 (PRs #40–#53). Findings were checked on main at 2ab9270 unless the entry says otherwise. Each says where, what goes wrong, and the fix the review suggests.

**Fixed on 2026-11-15** (one commit each, on `fix/review-v0.4.1`): the nine bugs to fix before the next release (a DuckDB spill folder per instance; table rules refuse names that may resolve in several schemas, and fail closed when the lookup fails; scheduled AI steps learn the current schema; manual commit asks the server before leaving out BEGIN and reads autocommit only from settings; the `E'\''` / `"c\"` redaction leak; quadratic `sp_*` redaction; ClickHouse lake table functions; `#[ignore]` on the table-rules live tests; no second run of a DuckDB write), and the ten to fix: spill folders deleted on quit, older results on disk freed when the disk runs low, the first move to disk outside the result's lock, a server move making the next index ask, a provider switch checking every restricting connection, the sandbox refusing to seal without its spill folder, SQL Server lock hints, `EXEC` and failures syncing `@@TRANCOUNT`, dropped foreign keys leaving the ER cache. Left from them:

- **DuckDB `memory_limit` is per instance** (`app/src/api/outputs.rs` `duckdb_memory_limit`): each DuckDB database may use RAM − results budget − floor, so several working at once can ask for more than the computer has. DuckDB has no limit shared between instances and a sealed sandbox can't change its limit; the CHANGELOG now says so instead of claiming the two can't over-promise.
- **Switching a conversation's provider** is refused while any connection the AI may use doesn't allow the new provider, even one the conversation never reached: recording the connections whose data entered a session would allow more.

**Smaller**

- In the AI's DuckDB sandbox, an output on disk is copied into a table of its own that also spills to disk, unchecked by `low_disk`.
- The `get_array_memory_size` entry (buffers shared between batches counted once per batch) was removed from this file in #45 without a fix.
- `SessionRole::is_ai()` is never used: nothing checks that AI or MCP SQL runs on an AI-role session key.

## UI (second review, 2026-11-15)

A review of the fixes found fourteen bugs; they are fixed, with the refused case tested where it can be outside a running app. Left:

- **Query tips** don't see a column more than about 500 characters into a call as wrapped by it (`coalesce(` with thousands of arguments): the price of keeping long predicates fast.
- **SQL text with no known dialect** (a query tip without a connection) is read as DuckDB reads it: `$$` opens a string.

## Jobs (improvements of 2026-11-16)

- **The assistant's job tools** set only an interval or one daily time: not several times, monthly, cron, a time zone, the missed-run policy, "run after", limits, retries or an incremental column.
- **Bulk loads** exist for PostgreSQL (`COPY`) and Databricks (Parquet in a staging volume, one `INSERT … SELECT`). Not done: MySQL `LOAD DATA LOCAL` (it turns conversion errors into warnings and truncates where an INSERT fails, and `local_infile` is off by default), ClickHouse native/RowBinary, SQL Server bulk insert (TDS bulk API), Snowflake staged files, BigQuery load jobs.
- **Databricks bulk loads:** files are uploaded one at a time (not in parallel); a load stopped by a crash or a killed app leaves its `databrain-load-…` folder in the staging volume (deleted on failure and Stop otherwise); columns whose cast isn't the INSERT literal's (text into a number column, a timestamp into a DATE) send the whole step by INSERTs rather than casting those columns another way.
- **`INSERT … SELECT` on the server** (a load whose rows come from a query on the same connection, without passing through DataBrain) was tried and dropped: it runs the query again after the load's own DELETE/DROP and before SQL (a Replace of a table from itself empties it), loads rows other than the step's (volatile functions, `LIMIT` without `ORDER BY`, a capped result), and Postgres refuses text into typed columns the INSERT path takes. Only as an explicit opt-in, plain inserts, no before SQL, and a query that provably doesn't read the target.
- **No retry test with a load** that fails with a connection error midway; the decision is unit-tested (`reads_only`, `retryable`, `load_retry_refused`, `kept_nothing`), and what a failed load reports is tested on SQLite (rolled back, before SQL that may commit, failed before writing). A failed commit and a database without transactions are tested only through `kept_nothing`.
- **A step left behind** (it didn't stop within 30 s) may still run: a query step's engine task keeps its tab until the statement ends (the next run of the step fails saying so), and a dropped load's last statement may go on on the server after its session is closed (on a database without transactions, a new load can overlap it). Nothing stops either on the server. A load running INSERTs at once (Databricks, up to 4) can leave as many statements that may still commit; before, at most one.
- **Job memory share:** a step left behind keeps its reservation until its statement really ends; a job that runs after another and reads that job's results (`results.x`) may find them freed once its own steps fill the job share (the other job isn't running), and Resume doesn't run the other job again.
- **Knowing what happened:** failures notify only on the desktop (no webhook or e-mail); a run doesn't record the job version it ran; 200 runs and 5,000 log lines per run are fixed, and a run's log can't be exported.
- **Code:** step and run statuses are strings in Rust and the UI, not a shared enum; `execute` rescans every step after each one ends.
- **chrono / chrono-tz are unmaintained** (RUSTSEC-2026-0543, -0555; ignored in `deny.toml`): DataBrain's own uses (job schedules and their time zones, `databrain-jobs`, export file names, the Neo4j, Oracle and Databricks connectors) could move to `jiff`, but arrow, parquet, neo4rs and russh keep chrono in the build, so it would add a crate without removing one. Revisit when arrow moves off chrono or a vulnerability is reported.
- **Run date:** the assistant's `run_job` can't run for a given date (no `run_date` argument); a range of days ("run for 1–15 November") is one "Run for this date" per day; the `current_date` hint reads the step's SQL text only (not a parameter set's `{}` expressions such as `current_date - 1`); a queued "run after" start takes the date of the other job's latest success, which a newer run of it may have replaced meanwhile.

## `dbr` command (idea, 2026-11-18)

A short command that opens files in DataBrain or queries from a terminal, for what DuckDB's own CLI can't do: DataBrain's file reading (Excel sheets, blank headers, Delta/Iceberg), its grid, charts, export and AI on the result, and its saved connections with their rules.

- **`dbr data.parquet orders.xlsx`**: opens the app (or the window already running) with a tab per file, `select * from …` run on the local DuckDB connection, as "Query a local file…" does.
- **`dbr report.sql`** (and double-click / "Open with" on a `.sql` file): opens it in a file tab, as ⌘O does (saved back with ⌘S, reloaded when changed outside); `--run` runs it, `-c "Sales PG"` picks its connection.
- **`dbr query "<sql>" [files…]`**: runs in the terminal on the bundled DuckDB and prints a table, `--csv` or `--json`, for pipes.
- **`dbr query -c "Sales PG" "<sql>"`**: any saved connection by name, with its credentials, read-only setting, production confirmation, PII masking and history; the part DuckDB's CLI can't do.

To build: a launcher like `databrain-mcp` / `databrain-jobs` (`dbr` → `databrain open …` / `databrain query …`; almost no app size); the single-instance plugin to pass "open these files" to a running window; a file association for `.sql` (and optionally `.parquet`, `.csv`) in `tauri.conf.json`; an "Install the `dbr` command" item (as VS Code's `code`), since the launchers aren't on PATH; and in the build without bundled DuckDB, a clear message when no DuckDB library is installed. Start with opening files and `.sql` in the app, then `query -c`; plain `query` only if it gets used.

## Loads into other databases (review of 2026-11-17, checked against vendor docs)

Every target but PostgreSQL (`COPY`) and Databricks (staged files, INSERTs 4 at once) loads with multi-row INSERTs, one after the other. In order of value for the work, with what the docs say may go wrong:

- **ClickHouse, larger INSERTs** (done: 100,000 rows / 16 MB by default): each INSERT writes a new part, and small ones are slow and lead to "too many parts". `max_query_size` (256 KB) bounds the SQL text, not the rows streamed after `VALUES`. Watch: on Replicated tables `insert_deduplicate` is on by default and drops an INSERT whose block is identical to a recent one, so two batches with the same rows (a table loaded twice, identical data) are kept once; that is true today and doesn't depend on the batch size.
- **ClickHouse binary inserts:** `INSERT … FORMAT RowBinary` (or Parquet) as the HTTP body instead of SQL text; the same deduplication applies.
- **MySQL, larger defaults** (done: 5,000 rows / 3 MB, capped under the server's own `max_allowed_packet`, read when a load starts): `max_allowed_packet` is 64 MB by default on 8.0 but 4 MB on 5.7, often lower on hosted servers; a statement past it gets ER_NET_PACKET_TOO_LARGE and **the connection is closed**, so the split-on-too-large retry can't help there (and loads run in a transaction anyway), hence the cap.
- **Oracle array binding** (done, both drivers; the thin helper 0.1.5's binaries must be uploaded to `oracle-agent/v0.1.5` before a build with it ships): it doesn't use batch-error mode (a call fails at its first bad row and keeps none), so an error names the call's rows, not the failing one. Loads used to send `INSERT ALL` of 100 rows (max 1,000), each a statement Oracle parses again: 2M rows was 20,000 statements. Batch-error mode would report per-row errors only with client and server 12.1+, and fails the call past too many error rows (OCI-38902). go-ora's open issues on NULLs in array binds don't hit the helper's `sql.NullString` arrays (tested live, a column of only NULLs included). Tested live against Oracle 23 Free on both drivers; not against older servers.
- **BigQuery load jobs** (`jobs.insert` with the rows as Parquet or newline JSON): one job, all or nothing, no query cost. Watch: every load job (failed ones too) counts toward the per-table daily modification limit (applied per partition on partitioned tables), so a job that runs often can hit it; large files need a resumable upload (multipart is for small ones); and loading converts values by the file format's rules, not the INSERT literal's, so it needs a fit check per column as Databricks' `dbx_fits`.
- **SQL Server, 1,000 rows per INSERT by default:** the default stays at 500 rows until compile times of 1,000-row VALUES lists on a wide table are measured.
- **SQL Server, more than 1,000 rows per statement:** `INSERT … SELECT … FROM (VALUES …) v(cols)` has no 1,000-row cap, but the cap exists to keep compile times sane: measure before adopting, it may be slower than 1,000-row INSERTs.
- **SQL Server bulk copy:** the fastest path, but `tiberius`'s `bulk_insert` always sends `INSERT BULK t (cols)` with no hints, and without `FIRE_TRIGGERS` and `CHECK_CONSTRAINTS` SQL Server fires no INSERT triggers and skips CHECK and FOREIGN KEY constraints, then marks them untrusted. That writes what INSERTs wouldn't: it needs our own `INSERT BULK … WITH (FIRE_TRIGGERS, CHECK_CONSTRAINTS)` over tiberius's bulk token stream, and every value built as its column's TDS type (money types aren't supported).
- **Snowflake array binding** (done, behind the connection's "Array binds for loads", off by default: turn it on by default once `snowflake_array_loads_write_what_inserts_do` has passed against a live account): typed binds (`FIXED`, `REAL`, `BOOLEAN`, `BINARY`, `TEXT`), 1 MB of values a call, under the login's `CLIENT_STAGE_ARRAY_BINDING_THRESHOLD`. "Array bind currently not supported for this query type" falls back to INSERTs; Snowflake publishes no error code for it, so the message is matched.
- **DuckDB appender** fed Arrow (already used for results): it runs in the connection's transaction (the load's), checks NOT NULL and keys when it flushes rather than per row, and writes every column unless built with a column list (`appender_with_columns`), so columns the rows don't have need their defaults that way.
- **Parallel INSERTs** fit only Databricks: the others load in one transaction on one session (PostgreSQL, MySQL, SQL Server, Snowflake, DuckDB, SQLite), where several sessions would give up the all-or-nothing load; Oracle gains more from array binding; ClickHouse wants bigger inserts, not more; BigQuery a load job.

Sources: ClickHouse [too many parts](https://clickhouse.com/docs/resources/support-center/knowledge-base/troubleshooting/exception-too-many-parts), [deduplication on retries](https://clickhouse.com/docs/guides/developer/deduplicating-inserts-on-retries); MySQL [packet too large](https://docs.oracle.com/cd/E17952_01/mysql-8.0-en/packet-too-large.html); [rust-oracle batch](https://github.com/kubo/rust-oracle/issues/29), [OCI-38902](https://docs.oracle.com/en/error-help/db/oci-38902), [go-ora issues](https://issues.ecosyste.ms/hosts/GitHub/repositories/sijms%2Fgo-ora/issues?page=2); BigQuery [quotas](https://docs.cloud.google.com/bigquery/quotas), [API uploads](https://docs.cloud.google.com/bigquery/docs/reference/api-uploads); SQL Server [table value constructor](https://learn.microsoft.com/sql/t-sql/queries/table-value-constructor-transact-sql), [triggers in bulk import](https://technet.microsoft.com/en-us/ms187640(v=sql.105)), [tiberius client source](https://docs.rs/tiberius/0.12.2/src/tiberius/client.rs.html); Snowflake [gosnowflake #540](https://github.com/snowflakedb/gosnowflake/issues/540), [ADBC driver](https://arrow.apache.org/adbc/15/driver/snowflake.html); DuckDB [appender](https://duckdb.org/docs/current/data/appender).

## AI job tools (review of 2026-11-17)

The escapes found by the red-team review are fixed (see CHANGELOG, Security). Left:

- **Duplicate across a rebase:** a copy of the assistant's step whose original the assistant changes before the copy is saved is saved as yours (the server finds no step of the assistant's with the copy's older SQL). Narrow window.
- **Rebase drops your edits to a step the assistant deleted** without saying so.
- **`set_job_node_results`** reads, changes and writes the job's spec without the save lock or a new version: a run ending during a save can write back the older spec under the newer version.
- **Check-then-save isn't atomic:** `save_job` and `update_job` check the version, then save; two saves in the same moment can both pass (no step changes owner: marks are judged against the version read).
- **Approval noise:** renaming an upstream step lists "runs after new (was old)" on every step after it, though their rows don't change.
- **Not followed:** the assistant's rows reached through `USE results`, a view made earlier, or a table name built from a `:param`; tables the assistant's steps wrote on other databases.
- **Name check not repeated after approval:** a tab or notebook taking the name meanwhile is refused later (the job step keeps it).
- **Missing tests:** a load with explicit `select … from results.x` SQL of the assistant's rows; a chain of steps following them.
