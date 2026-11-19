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
- **Not tested live:** Oracle (Instant Client): Stop and the wait-for-session check; SQL Server: Stop, the row limit on `EXEC`, reconnect sign-in (SQL Server 2022 crashes under emulation on the arm64 podman VM; Azure SQL Edge now logs in and runs the bulk copy test, so these could be tried there); ClickHouse: `KILL QUERY` at the row limit (fake server only). The Oracle helper's exit when DataBrain goes is tested in Go, not against a server.
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
- **Bulk loads:** every target has its fast path now; what is left per database is in "Loads into other databases" below. Not planned: MySQL `LOAD DATA LOCAL` (it turns conversion errors into warnings and truncates where an INSERT fails, and `local_infile` is off by default), Snowflake staged files (array binds instead).
- **Databricks bulk loads:** files are uploaded one at a time (not in parallel); a load stopped by a crash or a killed app leaves its `databrain-load-…` folder in the staging volume (deleted on failure and Stop otherwise); columns whose cast isn't the INSERT literal's (text into a number column, a timestamp into a DATE) send the whole step by INSERTs rather than casting those columns another way.
- **`INSERT … SELECT` on the server** (a load whose rows come from a query on the same connection, without passing through DataBrain) was tried and dropped: it runs the query again after the load's own DELETE/DROP and before SQL (a Replace of a table from itself empties it), loads rows other than the step's (volatile functions, `LIMIT` without `ORDER BY`, a capped result), and Postgres refuses text into typed columns the INSERT path takes. Only as an explicit opt-in, plain inserts, no before SQL, and a query that provably doesn't read the target.
- **No retry test with a load** that fails with a connection error midway; the decision is unit-tested (`reads_only`, `retryable`, `load_retry_refused`, `kept_nothing`), and what a failed load reports is tested on SQLite (rolled back, before SQL that may commit, failed before writing). A failed commit and a database without transactions are tested only through `kept_nothing`.
- **A step left behind** (it didn't stop within 30 s) may still run: a query step's engine task keeps its tab until the statement ends (the next run of the step fails saying so), and a dropped load's last statement may go on on the server after its session is closed (on a database without transactions, a new load can overlap it). Nothing stops either on the server. A load running INSERTs at once (Databricks, up to 4) can leave as many statements that may still commit; before, at most one.
- **Job memory share:** a step left behind keeps its reservation until its statement really ends; a job that runs after another and reads that job's results (`results.x`) may find them freed once its own steps fill the job share (the other job isn't running), and Resume doesn't run the other job again.
- **Activity bar:** the background runner's runs show no step or progress (that process sends no events), and how they ended isn't shown in the bar.
- **Knowing what happened:** a run's end is told in the window (the activity bar) and on the desktop, not by webhook or e-mail; the app's desktop notification is sent only for runs seen running in this window (a run of the background runner notifies from that process, failures only); a run doesn't record the job version it ran; 200 runs and 5,000 log lines per run are fixed, and a run's log can't be exported.
- **Code:** step and run statuses are strings in Rust and the UI, not a shared enum; `execute` rescans every step after each one ends.
- **chrono / chrono-tz are unmaintained** (RUSTSEC-2026-0543, -0555; ignored in `deny.toml`): DataBrain's own uses (job schedules and their time zones, `databrain-jobs`, export file names, the Neo4j, Oracle and Databricks connectors) could move to `jiff`, but arrow, parquet, neo4rs and russh keep chrono in the build, so it would add a crate without removing one. Revisit when arrow moves off chrono or a vulnerability is reported.
- **Run date:** the step panel's `:run_date` examples for Snowflake, BigQuery and Databricks follow their documentation and aren't run against a live server (the others are, in `run_date_examples_give_what_they_say`); the assistant's `run_job` can't run for a given date (no `run_date` argument); a range of days ("run for 1–15 November") is one "Run for this date" per day; the `current_date` hint reads the step's SQL text only (not a parameter set's `{}` expressions such as `current_date - 1`); a queued "run after" start takes the date of the other job's latest success, which a newer run of it may have replaced meanwhile.

## `dbr` command (2026-11-18; flags 2026-11-19; terminal mode done 2026-11-19)

A short command that opens files in DataBrain or queries from a terminal, for what DuckDB's own CLI can't do: DataBrain's file reading (Excel sheets, blank headers, Delta/Iceberg), its grid, charts, export and AI on the result, and its saved connections with their rules.

One command, two flags decide where it runs and where the rows go:

- **`-c, --conn <name>`**: the saved connection to run on (by name, any case; an unknown or ambiguous name lists the close ones and exits). Default: the local DuckDB (files), or the connection a `.sql` file was saved with.
- **`-i, --inline`**: print the rows in the terminal instead of opening the app. Without it, `dbr` opens the app (or the running window) with a tab for each file or query.

```
dbr data.parquet orders.xlsx                 # app: a tab per file, select * from … on the local DuckDB
dbr data.parquet -i                          # terminal: the first rows of the file
dbr report.sql                               # app: the file in a file tab (⌘S saves it back)
dbr report.sql -c "Sales PG" --run           # app: the file on Sales PG, run
dbr -c "Sales PG" "select * from orders"     # app: a new tab on Sales PG with the query run
dbr -c "Sales PG" -i "select count(*) from orders"
                                             # terminal: the result as a table
dbr -c sales_pg -i --csv "select …" > out.csv
dbr -i "select * from 'data.parquet' where amount > 100"
                                             # terminal: the local DuckDB (files in the SQL)
```

What the terminal mode does:

```
$ dbr -c "Sales PG" -i "select id, status, total from orders order by id"
 id │ status │   total
────┼────────┼────────
  1 │ paid   │  120.50
  2 │ open   │   35.00
  … │ …      │       …
100 rows shown of 48,213 (--limit 0 for all) · Sales PG · 182 ms
```

- **Output:** a table sized to the terminal (long cells cut with `…`, numbers right-aligned), up to `--limit` rows (100 by default; 0 for all). `--csv`, `--json` (lines) and `--tsv` print every row in that format and imply `--inline`. When stdout isn't a terminal (a pipe or a file), the default format is CSV, so `dbr -c x -i "…" | head` and `> out.csv` work.
- **Rules:** the connection's own rules, as in the app. Read-only refuses writes. A production connection asks `Run on production "Sales PG"? [y/N]` (or refuses without a terminal, unless `--yes`). PII masking and the AI table rules don't apply (no AI), the table access rules do. Every run goes to the query history with origin `cli`.
- **Statements:** several statements run in order; each that returns rows prints its own table (`-- statement 2 of 3` between them). Errors print the database's message and exit 1; Ctrl-C stops the statement on the server.
- **`:name` parameters:** `-p name=value` (repeatable); a missing one is an error naming it (no prompts, so scripts don't hang).
- **What an argument is:** an existing path is a file (`.sql` → its text; data files → read on DuckDB); anything else is SQL. `-e "<sql>"` forces SQL (a query that happens to be a file name).
- **Exit codes:** 0 done, 1 the query failed, 2 bad arguments (unknown connection, missing parameter), 130 stopped.

To build: a launcher like `databrain-mcp` / `databrain-jobs` (`dbr` → `databrain open …` / `databrain query …`; almost no app size); the single-instance plugin to pass "open these files" to a running window; a file association for `.sql` (and optionally `.parquet`, `.csv`) in `tauri.conf.json`; an "Install the `dbr` command" item (as VS Code's `code`), since the launchers aren't on PATH; and in the build without bundled DuckDB, a clear message when no DuckDB library is installed. The terminal mode reuses the engine the jobs runner uses (sessions, the safety checks, history), with no window. Order: open files and `.sql` in the app; then `-c … -i` (the part DuckDB's CLI can't do: saved connections with their credentials and rules); then the formats and parameters.

Done: the terminal mode (`-i`, `-c`, the formats, `--limit`, `-p`, `-e`, data files, the production prompt and `--yes`, history as "Terminal", the exit codes), as `dbr` and `databrain query`; a table shows at most 1,000 rows (CSV/JSON/TSV every row). Left:

- **Opening in the app** (no `-i`): files and `.sql` in tabs, the query in a new tab on `-c`'s connection; needs the single-instance plugin (a new dependency: weigh its size) to hand them to a running window, and file associations.
- **"Install the `dbr` command"** in the app (a link on PATH, as VS Code's `code`): today it is used from the app bundle's folder.
- **Jobs from `dbr`** (done: `list`, `run`, `runs`, `stop` with a stop request read on the run's heartbeat, which also lets the app stop the background runner's runs). Left: stopping one step of another process's run, and a run started with `dbr jobs run` that is still printing when stopped from elsewhere ends with exit code 1 like any stopped run.
- **Rows while the query runs:** `--csv`/`--json`/`--tsv` write a page at a time once the engine has the result, so memory stays at the engine's result store (spilled to disk past its budget), but the first line waits for the query to finish (3M rows: about 5 s in a debug build). Streaming them as the query returns them needs an engine read that streams to a writer, as `export_query` does to a file (one read statement, its own session).
- **Windows:** the launcher ignores Ctrl-C and waits for `databrain query` (which stops its work on it); not tried on Windows yet, nor the console width read there.

## Loads into other databases (review of 2026-11-17, checked against vendor docs)

Done (checked 2026-11-19): every target has its fast path. PostgreSQL `COPY`; Databricks Parquet files in a staging volume; ClickHouse `FORMAT TabSeparated`, 100,000 rows / 16 MB an INSERT; MySQL 5,000 rows / 3 MB, capped under the server's `max_allowed_packet`; Oracle array DML (both drivers, helper 0.1.5+); Snowflake array binds and BigQuery load jobs (both behind a connection setting, off by default); SQL Server bulk copy; DuckDB's appender. A column a fast path can't take as its INSERT would sends the step by INSERTs, with a warning on the step. Left, in order of value:

- **BigQuery load jobs:** try them against a live project (the value equivalence comes from BigQuery's JSON and literal rules, the upload is tested against a mock server; every upload request carries the token, which Google may not need past the first; 429/5xx are resumed after a pause), add a comparison test like `clickhouse_tsv_loads_write_what_inserts_do` gated on a project, then turn them on by default. Every load job (failed ones too) counts toward the table's daily modification limit, so a step that runs often can hit it.
- **Snowflake array binds:** turn them on by default once `snowflake_array_loads_write_what_inserts_do` has passed against a live account. "Array bind currently not supported for this query type" falls back to INSERTs; Snowflake publishes no error code for it, so the message is matched.
- **Oracle array DML:** no batch-error mode (a call fails at its first bad row and keeps none), so an error names the call's rows, not the failing one; batch-error mode reports per-row errors only with client and server 12.1+, and fails the call past too many error rows (OCI-38902). Tested live against Oracle 23 Free on both drivers, not older servers.
- **SQL Server bulk copy, columns still on INSERTs** (their literal converts differently, or the server refuses the copy): a double into `real` or `decimal`, text into `varchar`/`char` (the `N'…'` literal maps to the code page with `?`), a decimal carried as text into `nvarchar`, `datetime`/`smalldatetime`/`money`, timestamps with more digits than the column, time zones with a changing offset, and `datetimeoffset` below scale 7 ("Invalid column attribute from bcp client": check whether tiberius sends its metadata as the server expects). Also: `INSERT … SELECT … FROM (VALUES …)` has no 1,000-row cap; measure it before using it (500-row INSERTs beat 1,000-row ones on a 30-column table: 30.3 s vs 36.5 s for 50,000 rows).
- **DuckDB appender, columns still on INSERTs:** `Date64`, nanosecond time zone timestamps, decimals over 38 digits, intervals, nested types, floats into integer or decimal columns and zoned instants into anything but `TIMESTAMPTZ` (DuckDB's casts differ from the literal there; casting through the literal's text in the `SELECT` would let them through).
- **Databricks staged files, columns still on INSERTs:** a timestamp into `DATE`, text into a number column (the user is warned and can cast; DataBrain doesn't convert).
- **ClickHouse:** RowBinary would also skip the text parsing, but needs every value encoded as its column's exact binary type. Watch: on Replicated tables `insert_deduplicate` (on by default) drops an INSERT whose block is identical to a recent one, so the same rows loaded twice are kept once, whatever the batch size.

Not planned: parallel INSERTs beyond Databricks (the others load in one transaction on one session, which several sessions would give up; Oracle gains more from arrays, ClickHouse from bigger inserts, BigQuery from a load job).

Sources: ClickHouse [too many parts](https://clickhouse.com/docs/resources/support-center/knowledge-base/troubleshooting/exception-too-many-parts), [deduplication on retries](https://clickhouse.com/docs/guides/developer/deduplicating-inserts-on-retries); MySQL [packet too large](https://docs.oracle.com/cd/E17952_01/mysql-8.0-en/packet-too-large.html); [rust-oracle batch](https://github.com/kubo/rust-oracle/issues/29), [OCI-38902](https://docs.oracle.com/en/error-help/db/oci-38902), [go-ora issues](https://issues.ecosyste.ms/hosts/GitHub/repositories/sijms%2Fgo-ora/issues?page=2); BigQuery [quotas](https://docs.cloud.google.com/bigquery/quotas), [API uploads](https://docs.cloud.google.com/bigquery/docs/reference/api-uploads); SQL Server [table value constructor](https://learn.microsoft.com/sql/t-sql/queries/table-value-constructor-transact-sql), [triggers in bulk import](https://technet.microsoft.com/en-us/ms187640(v=sql.105)), [tiberius client source](https://docs.rs/tiberius/0.12.2/src/tiberius/client.rs.html); Snowflake [gosnowflake #540](https://github.com/snowflakedb/gosnowflake/issues/540), [ADBC driver](https://arrow.apache.org/adbc/15/driver/snowflake.html); DuckDB [appender](https://duckdb.org/docs/current/data/appender).

## Table explorer (design, 2026-11-19)

One tab per table that gathers what DataBrain knows and can read about it, as Databricks' Catalog Explorer and DBeaver's table editor do: sample rows, columns, DDL, statistics, relations, history and use, without writing a query. Today these are spread out (Show DDL, the ER diagram, Generate test data, the knowledge base's notes and learned joins, column stats in a result grid) or missing (table statistics, a column profile, partitions, Delta/time-travel history).

**Opening it:** double-click a table or view in the explorer (today: inserts its name), right-click → "Open table", ⌘-click a table name in the editor, "Open table" in the command palette (search any indexed table), a table name in the AI panel. One tab per table: opening it again shows that tab.

```
┌ ▦ sales.orders · Sales PG ────────────────────────────────────────────────────── × ┐
│ ▦ orders   table · sales · Sales PG  PROD    ~1.2M rows · 340 MB · changed 2 h ago  │
│ Orders placed on the web shop (comment)                         [⟳] [★ Pin note]   │
│ [▶ Query] [⇪ Load into…] [⤓ Export…] [✱ Test data…] [⇄ Compare…] [✦ Ask AI]          │
├─────────────────────────────────────────────────────────────────────────────────────┤
│ Data · Columns 14 · DDL · Statistics · Relations 3 · Indexes 4 · History · Usage · Notes 2 │
├─────────────────────────────────────────────────────────────────────────────────────┤
│ Sample  (•) First 100  ( ) Random 1,000  ( ) Last changed      Filter [status='paid'] ▶│
│ ┌──────┬─────────────┬────────┬─────────┬────────────────────┐                     │
│ │ id   │ customer_id │ status │   total │ created_at         │                     │
│ ├──────┼─────────────┼────────┼─────────┼────────────────────┤                     │
│ │ 1001 │ 77          │ paid   │  120.50 │ 2026-11-18 09:12   │  (the result grid:  │
│ │ 1002 │ 12          │ open   │   35.00 │ 2026-11-18 09:15   │   sort, find, copy, │
│ │ …    │             │        │         │                    │   column stats)     │
│ └──────┴─────────────┴────────┴─────────┴────────────────────┘                     │
│ 100 rows · 41 ms · select * from sales.orders where status='paid' limit 100  [Open in editor]│
└─────────────────────────────────────────────────────────────────────────────────────┘
```

**Header:** name, kind (table, view, materialized view, external/foreign, Delta, Iceberg), schema, connection with its tag (PROD/TEST), the row estimate and size the database keeps (no `count(*)`), last changed, the comment, and the actions DataBrain already has, preset to this table: Query (a new tab with `select * … limit 100`), Load into…, Export…, Generate test data…, Compare… (schema compare), Ask AI (the AI panel with the table as context).

**Sub-tabs** (each loads when first shown; what a database can't say is "Not available on Snowflake" with why, not hidden):

- **Data:** sample rows in the result grid. First 100 (`LIMIT`/`TOP`/`FETCH FIRST`), random (`TABLESAMPLE` / `SAMPLE` / `ORDER BY random()` where cheap; only where the database samples without a full scan), or last changed (an `updated_at`-like column if there is one). A filter box (a `WHERE` condition, checked as a read), row limit, Open in editor. A view runs its query; on BigQuery the bytes it would scan are shown first (cost preview), and a sample past a set size asks.
- **Columns:** name, type as the database names it, nullable, default, PK/FK/unique marks, comment, the knowledge base's note and PII mark. On demand ("Profile columns", one query, asks on production or a large table): non-null %, distinct count (approximate where the database has it: `approx_count_distinct`, `APPROX_COUNT_DISTINCT`, `uniq`), min/max, the most common values, a small histogram of numbers and dates.

```
│ Column        Type            Null  Key  Comment              Non-null  Distinct  Min … Max              │
│ id            bigint          no    PK                          100%     1.2M      1 … 1,204,880           │
│ customer_id   bigint          no    FK→customers.id              100%     88,412    1 … 91,002              │
│ status        varchar(16)     no         paid/open/cancelled     100%     3         ▇▇▇▇▇▂ paid 71% open 25% │
│ total         numeric(12,2)   yes        incl. VAT               99.8%    40,113    0.00 … 9,870.00 ▁▃▇▅▂   │
```

- **DDL:** what Show DDL shows today (Format, Copy, Open in editor), in the tab.
- **Statistics:** row count (estimate, and "Count exactly" on demand), size on disk and in indexes, partitions/files (Databricks `DESCRIBE DETAIL`, BigQuery partitions, Snowflake clustering), last analyzed/vacuumed (Postgres `pg_stat_user_tables`: live/dead rows, seq/index scans), last DDL and last data change where the database records them.
- **Relations:** declared foreign keys in and out, learned and documented joins (with their state: given to the AI, waiting…), views and materialized views that read the table, and the ER diagram around it (the existing ER tab, one level out, "More" to widen).
- **Indexes & constraints:** indexes (columns, unique, size, use counts where the database keeps them), checks, uniques, triggers.
- **History:** Delta `DESCRIBE HISTORY` (version, time, operation, rows written; "Query this version" opens `VERSION AS OF`), Snowflake and BigQuery time travel (`AT`/`FOR SYSTEM_TIME AS OF`), Iceberg snapshots; elsewhere "Not available".
- **Usage:** from DataBrain's query history: how often and when it was read or written (the user's queries, jobs, the AI), the jobs whose steps read or load it (open the job), the most used queries on it (open in editor). Local only: nothing is asked of the server.
- **Notes:** the knowledge base's notes on the table and its columns, add/edit (as in Knowledge → Notes), and the AI's description of the table if one was made.

**Rules:** everything goes through the connection's safety checks (reads only, read-only connections fine; samples and profiles of a production table or one past a size ask once per tab); every query a sub-tab runs is in the query history (origin `explorer`) and shown at the bottom of its sub-tab, so the user can see and copy it; nothing is sent to the AI unless "Ask AI" is pressed. Metadata comes from the explorer cache and knowledge index first (shown at once, marked as cached with their age), then the database (Refresh).

**Per database (first version):** PostgreSQL, MySQL, SQL Server, Oracle, Snowflake, Databricks, BigQuery, DuckDB, SQLite, ClickHouse: Data, Columns, DDL, Relations, Usage, Notes. Statistics and Indexes where the catalog has them (Postgres, MySQL, SQL Server, Oracle first; Snowflake/BigQuery/Databricks sizes and partitions). History on Databricks (Delta), Snowflake and BigQuery. MongoDB: Data (`find().limit`), fields sampled from documents, indexes, collection stats; no DDL. Neo4j: labels and relationship types instead of a table (later).

**Build order:** 1. (done) the tab with header, Data (first N, a `where` condition checked as one read, by the UI before it runs and by the engine for every run of origin `explorer`, which the history shows; on BigQuery not run on opening, Run shows the bytes billed first; on a production connection not run on opening, the first Run asks once per tab; MongoDB and Neo4j say they take no condition), Columns, DDL, Notes; opened by double-click, "Open table" and the table search. Not yet in 1: the header's size and last change, Load into…/Export… buttons (the Data grid has them), Compare…, Ask AI, ⌘-click in the editor and "Open table" in the command palette; random and last-changed samples; asking before a sample of a table past a set size; metadata shown from the explorer cache first (marked cached); the SQL run shown under each sub-tab ("Open in editor" has it); the tab isn't restored after a restart; a sample on another database that bills by scan (none known besides BigQuery) would need the same check. 2. (done) Column profile ("Profile columns": non-null, distinct, approximate where the database has it, min/max of numbers and times, the first 100 columns; a column's 10 most common values) and Statistics (the catalog's figures per database, "Count exactly"), each read on a session of its own (`{tab}#profile`, `#top:{column}`, `#stats`, `#count`, closed with the tab or when its part goes away) through `api::explore_query` (origin `explorer`, the result dropped once read); a read of every row asks once per tab on production, past about a million rows or with no row estimate on a server database, and on BigQuery at each read that bills. Not yet in 2: the histogram of numbers and dates; BigQuery partitions (`INFORMATION_SCHEMA.PARTITIONS`) and Snowflake clustering depth; SQL Server's `APPROX_COUNT_DISTINCT` (2019+); Postgres partitioned tables' figures (a parent's `reltuples` and sizes are its own, not its partitions'); the profile and statistics SQL live in `ui/src/lib/explore.ts` instead of the connectors (no `Session::table_stats`), so the terminal and the AI can't read them. 3. Relations (FKs, learned joins, ER around it) and Usage (local history). 4. History/time travel and Indexes. Size: no new dependency; the profile and statistics are SQL per dialect in the connectors (a `Session::table_stats` with a default "not available").

## AI job tools (review of 2026-11-17)

The escapes found by the red-team review are fixed (see CHANGELOG, Security). Left:

- **Duplicate across a rebase:** a copy of the assistant's step whose original the assistant changes before the copy is saved is saved as yours (the server finds no step of the assistant's with the copy's older SQL). Narrow window.
- **Keep my changes and links:** a link added here that would close a loop with links added in the version saved since is left out without a message.
- **A job deleted before its open tab saves** is created again by that save (no version to check it against: the editor reads the job first and finds none); deleted between that read and the save, the save is refused.
- **Approval noise:** renaming an upstream step lists "runs after new (was old)" on every step after it, though their rows don't change.
- **Not followed:** the assistant's rows reached through `USE results`, a view made earlier, or a table name built from a `:param`; tables the assistant's steps wrote on other databases.
- **Name check not repeated after approval:** a tab or notebook taking the name meanwhile is refused later (the job step keeps it).
- **Missing tests:** a load with explicit `select … from results.x` SQL of the assistant's rows; a chain of steps following them.
