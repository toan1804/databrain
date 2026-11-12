# Known gaps to fix later

Found in reviews and left for a later release on purpose. Each says what goes wrong; remove an entry when it is fixed.

## Single program (verify on Windows before relying on it)

- An update while an MCP client is connected: the installer may end the MCP session (`databrain.exe mcp`), or hit "file in use" on the `databrain-mcp.exe` launcher.
- If an MCP client kills `databrain-mcp.exe` without closing its stdin, the child `databrain.exe mcp` may keep running (no job object ties them).
- `databrain-jobs run` / `list` output in a terminal goes through the GUI-subsystem launcher's console attach; check it prints in cmd and PowerShell.

## Query engine (review of 2026-11-12; see [QUERY_ENGINE.md](QUERY_ENGINE.md))

Paths are in `crates/query-engine/src/` unless given in full. Items 1, 2 and 5 were confirmed by reading the code a second time; the others are from one reading and should be reproduced with a test before they are fixed.

### High

1. **A load dry run can commit.** `dry_run_load` (`load.rs`) runs the real plan in a transaction and relies on the final rollback; the step's before/after SQL runs as written, and nothing refuses `COMMIT`, `ROLLBACK` or `BEGIN` in it (`transaction()` only looks for DDL on MySQL). After SQL `COMMIT;` in Sync mode: the dry run writes the sample rows, deletes the rows not in the sample, and commits. A real load has the same hole: after a user `COMMIT`, a failure still says "rolled back: nothing was changed".

### Medium

2. **Loads skip the production confirmation.** `prepare_load` checks only `read_only`; the production reasons live only in `run.rs` (`safety_reasons`). "Load into… → Replace table" on a production connection drops and recreates the table without a prompt (`crates/app/src/api/results.rs` `load_result`, `LoadIntoDialog.tsx`).
3. **Grid edits inside an open transaction say "nothing was saved" when earlier changes were.** In manual commit, `apply_edits` (`edit.rs`) has no transaction of its own, so a conflict on change N rolls nothing back; changes before it stay pending and go in with the user's next Commit.
4. **Only Postgres, MySQL, SQL Server and SQLite report an open transaction.** `Session::in_transaction` defaults to `false` (`crates/connector-core/src/lib.rs`); idle close and `reconnect` (`sessions.rs`) trust it. A typed `BEGIN` on DuckDB, Snowflake or Oracle: Refresh in the explorer or the idle timer drops the session and the uncommitted work, with no badge. `apply_style` (`edit.rs`) also sends its own `BEGIN … COMMIT` into a transaction it can't see on DuckDB and Snowflake.
5. **A panicking run leaves its tab busy.** The `jobs` entry is removed in one place at the end of the run task (`run.rs`); there is no guard. After a panic every later run on that tab gets "already running", and its session is never idle-closed.
6. **The busy check is advisory outside `start_run`.** `explain` (`plan.rs`), `apply_edits` (`edit.rs`), `end_transaction` (`sessions.rs`) and export (`export.rs`) look at `jobs` but claim nothing: a run can start mid-apply on the same session and be committed by the edit's transaction, or a Rollback can land mid-apply.
7. **Outputs whose rows are gone stay `Live`.** `crates/ai/src/tools/query.rs` (sample rows) and `crates/app/src/api/results.rs` remove rows straight from the `ResultStore`; the registry (`outputs.rs`) still lists the output, which fails on use. `ai:{session}` tabs are never closed, so their last outputs stay active (never evictable) until restart.
8. **No byte cap on a result.** `collect_stream` (`run.rs`) keeps every batch; active outputs are outside the 1 GB budget (`outputs.rs`). Wide rows can exhaust memory even with a row limit.

### Low

- `disconnect` (`sessions.rs`) doesn't cancel running jobs: the next statement of a script reconnects, so a script's `BEGIN … COMMIT` is split across sessions.
- Export on a session with an open transaction returns on cancel without draining the stream (`export.rs`), unlike `run.rs`: a late server cancel can hit the tab's next statement.
- Stop is ignored while a run connects or signs in (`begin_if_manual`, `session_for`: the cancel token isn't watched).
- SQL Server plan: when `SET SHOWPLAN_ALL OFF` fails (`plan.rs`), the tab's session stays in showplan mode and later statements don't execute; the session isn't dropped.
- Oracle plan cleanup's `collect()` future is never awaited (`plan.rs`); whether the `DELETE` completes is not verified.
- `set_pinned` (`outputs.rs`) sets `pinned` before the snapshot is written: on a failed write the output is pinned in memory but not saved.
- The `exported` map (`export.rs`) only grows.
- `with_tab` (`sessions.rs`) drops the session by key, not identity: two failing catalog calls can drop each other's fresh session.
- Sessions are dropped while the `sessions` mutex is held (`sessions.rs`); whether a connector's drop can block is not verified.
- Blocking calls on async threads: `add_history` and `set_setting("outputs_seq")` per statement (`run.rs`, `outputs.rs`), file writes in `export_stream`.
- Transaction statements are defined three times and already disagree: `edit::apply_style`, `traits().load.transactions`, and literals in `end_transaction` (Snowflake and DuckDB for edits but not loads; SQL Server's guarded rollback only in loads).
- `explain` doesn't check for a single statement (`cost_statement` does); the plain path relies on drivers refusing multi-statement text.

### Ways to fix them (ideas, in the order suggested)

1. **Load as plan → validate → execute** (fixes 1, 8; medium). A `LoadPlan` whose user statements are classified once: transaction control is refused, and the dry run and the real run use the same validated plan, so the "rolled back" claim can be tested. A byte budget in `collect_stream` that truncates with a notice. Risk: before/after SQL that relied on `COMMIT` is refused.
2. **Job lifecycle as a guard** (fixes 5 and the export drain; small). A `JobGuard` owns the `jobs` entry, `JobFinished` and output cleanup on any exit, panic included; run and export share the cancel-then-drain code.
3. **One guarded execution path** (fixes 2, 6; medium). `engine.guarded(session_key, origin, intent)` does read-only, the production reasons and the session claim and returns a lease; run, load, edit, explain, export and end-transaction all take it, so a new entry point can't skip a check. Risk: loads and job steps on production connections get a prompt they don't have today (jobs need a stored approval).
4. **Tri-state transaction status** (fixes 3, 4 and the three definitions; small to medium). `Open` / `None` / `Unknown` from the session, with "unknown" treated as busy by idle close and reconnect, and one `TxDialect` in `SourceTraits` used by edits, loads and Commit/Rollback. Grid edits in an open transaction use a savepoint. Risk: sessions kept longer where the state is unknown.
5. **Split `outputs.rs`** (fixes 7; small, mostly moving code). `outputs/registry.rs`, `outputs/duckdb.rs`, `compare.rs`; the registry is the only owner of result removal (`ResultStore::remove` not callable from other crates, or a hook), and AI tabs are closed when their chat ends.
6. **Typed session roles** (medium; low risk). `enum SessionRole { Tab, Notebook, Meta, Index, Ai, JobStep, Load, Export }` instead of string keys (`__meta__`, `ai:…`), with idle close, reconnect, sandbox and cleanup decided per role.

## Explorer and AI index (left from the review of 2026-11-12)

- **Snowflake, Databricks and DuckDB miss new routines on refresh.** Their explorer fingerprint is the index's (tables and columns), but their listings include functions, procedures, sequences and macros: a new one shows only after the schema's own Refresh. Fixed by the single fingerprint in the metadata-store design (step 3).
- **Two stores for the same facts.** The explorer cache and the AI index are still filled separately (`meta_table` reconciles them per table). The chosen design makes `meta_*` the only copy, with an allow-list for what the AI may use; it is not built yet.
- PostgreSQL row estimates are refreshed only when the schema changes (the explorer fingerprint leaves out `reltuples`).
- The ER diagram (`schema_tables`) is always read live and doesn't work offline.
- `clear_explorer_cache` is a command the UI never calls.

## Connector core (review of 2026-11-12; see [CONNECTOR_CORE.md](CONNECTOR_CORE.md))

Paths are in `crates/connector-core/src/` unless given in full. From one reading: reproduce with a test before fixing.

### Medium

- **`execute` is "one statement" by convention only** (`lib.rs`). The MySQL driver allows several statements per call by default; with `NO_BACKSLASH_ESCAPES` on the server, `select 'a\'; drop table t; -- '` is one read to the splitter and classifier and two statements to the server. Turn the driver flag off.
- **Lineage's check classifies before binding** (`crates/app/src/api/lineage.rs`, through `Session::check_sql`): a raw parameter value reaches `EXPLAIN …` unclassified. Classify the bound text.
- **Cypher write words miss `INSERT` and `TERMINATE`** (`language.rs`); Neo4j syntax not verified.
- **Redaction misses** (`sql/redact.rs`): `SET PASSWORD FOR … = '…'`; the old password in `ALTER USER … IDENTIFIED BY … REPLACE …`; Redshift `SECRET_ACCESS_KEY`, Snowflake `SECRET_STRING`, DuckDB `BEARER_TOKEN` and `ENCRYPTION_KEY`; `AccountKey=` in a connection string; `IDENTIFIED BY VALUES '<hash>'`; an unquoted password with `#` or `$` is cut short; secrets passed by position (`s3(url, key, secret)`, `MySQL(…)`, `sp_addlinkedsrvlogin`).
- **`Session::in_transaction` defaults to `false`** (`lib.rs`): the contract behind the query-engine item on lost transactions. `Capabilities.transactions` is true for Oracle, Snowflake, BigQuery and DuckDB, which don't report an open one.
- **Values that fail to convert show as NULL without a notice on SQL Server and Oracle**: `BatchBuilder` (`value.rs`) only counts them, and those connectors never read the count.

### Low

- `Capabilities.transactions`, `cancel`, `schemas` and `read_only_sessions` are read nowhere (only `ssh` is); Oracle's `read_only_sessions: true` contradicts `server_read_only()` (`types.rs`).
- `is_string_literal` (`sql/params.rs`) accepts an unterminated literal (`'''`, `'abc''`): the value swallows the SQL after it.
- `parse_datetime` (`value.rs`) drops an offset after a fraction (`13:04:05.25+02:00`) but rejects one without; used for zoned columns in `crates/connectors/cloud/src/common.rs`.
- Timestamp builders take `Timestamp` and `TimestampTz` alike; `infinity` and BC text become NULL; `Date → Timestamp` multiplies unchecked; `Int → Float64` is treated as lossless (`value.rs`).
- The splitter cuts valid SQL (safe, but the statement breaks): Postgres `E'it\'s; x'`, Oracle `q'[it's; x]'`, Snowflake `BEGIN … END;` blocks.
- Structure: five hand-written lexers (splitter and params, two in redact, `referenced_results`, `changes_default`) beside the parser's; `QueryLanguage::Cql` has no connector; `lib.rs` mixes the traits with quoting and catalog helpers.

### Ways to fix them (ideas, in the order suggested)

1. **A "must not be a read" corpus per dialect, and a property test** (small; do with the high fixes). One file of statements per kind that must never be `Read`, and a test that classifying a whole text is never weaker than classifying its split pieces. Keeps the fixes above from regressing.
2. **Reads by allow-list** (fixes high 1–4 and 6 as a class; medium). `Read` only for a parsed query whose functions are all known built-ins; everything else is `Unknown`. Risk: more prompts until the built-in lists are complete per engine.
3. **Typed execution request** (fixes the multi-statement and bind-before-classify gaps; medium). `execute` takes a statement that can only be built from classified, bound text; multi-statement is off in every driver. Load and plan paths need their own way through.
4. **A feature enum instead of silent defaults** (small to medium; touches every connector). `Session::supports(Feature)`, a tri-state transaction status, and the unused `Capabilities` fields removed, so a missing implementation is visible instead of meaning "no".
5. **Redaction by token stream** (medium). A secret keyword, then the next literal, plus a table of functions whose secrets are positional. Risk: over-hiding in saved history.
6. **One `ParsedStatement`** (large). Span, tokens, AST or parse error, and kind, shared by split, parameters, classify, read-only and `table_refs`: no lexer drift, and binding before classifying can't be skipped. Risk: gaps in the parser's tokenizer become split failures.
