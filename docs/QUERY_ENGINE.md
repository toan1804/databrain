# Query engine

`crates/query-engine` is the one place that opens database sessions and runs statements on them. The UI commands (`crates/app/src/api/`), the AI tools (`crates/ai/src/tools/`), MCP and job steps (`crates/app/src/jobs.rs`) all call the same `QueryEngine`. This page says what the crate owns, which session runs what, how a run goes from `RunRequest` to events and stored rows, and which safety checks live here.

Main files:

| File | What it holds |
|---|---|
| `lib.rs` | `QueryEngine`, its shared state, `RunRequest`, `RunResponse`, `PlannedStatement`, `JobHandle`, `Live` |
| `connect.rs` | Credentials per connection, SSH tunnel, connect timeout, `test_connection` |
| `probe.rs` | DNS and TCP check before a driver connects (`target`, `reach`) |
| `sessions.rs` | `session_for`, idle close, `disconnect`, `reconnect`, `close_tab`, manual commit, `with_meta` / `with_tab` |
| `run.rs` | `plan`, `bind_parameters`, `safety_reasons`, `start_run`, `run_and_wait`, `run_job`, `collect_stream`, `cancel` |
| `events.rs` | `JobEvent`, `EventSink`, `EventHub`, `StatementOutcomeView`, export progress types |
| `outputs.rs` | `OutputRegistry` (handles, names, versions, pinning, budget, snapshots), `OutputTables` (DuckDB `results.<name>`), compare SQL |
| `load.rs` | Load steps: `load_rows`, `write_output`, `dry_run_load`, `check_load_target` |
| `edit.rs` | Grid edits: `source_table`, `edit_target`, `edit_statements`, `apply_edits` |
| `plan.rs` | `explain`, `estimate_cost`, plan parsers per engine |
| `export.rs` | Full export: `export_query`, `cancel_export` |
| `metadata.rs` | Catalog reads on the metadata session |
| `schema_diff.rs`, `related.rs` | Schema compare and its script; foreign-key links of a result |
| `testdata.rs`, `testdata_pattern.rs` | Made-up rows for a table |
| `error.rs` | `EngineError` and conversions |

## What the crate owns, and what it doesn't

Owns: live sessions and their tunnels, which run may use which session, running statements and stopping them, the read-only and confirmation checks of a run, run events, output handles and their memory budget, loads, grid edits, plans, full exports, catalog calls.

Does not own:

| Thing | Where |
|---|---|
| Talking to a database (`Session::execute`, catalog queries, server-side cancel) | `crates/connectors/*`, trait in `crates/connector-core/src/lib.rs` |
| Splitting and classifying SQL, `read_only_allows`, parameters | `crates/connector-core/src/sql/` |
| Whether a transaction is open (`Session::transaction_state`: open, none, or unknown, which counts as open) | each connector; `crates/connector-core/src/conn_state.rs` follows the statements for most |
| The rows of a result, views, paging | `crates/result-store` |
| INSERT / MERGE / CREATE TABLE text, file writers | `crates/export` |
| Saved connections, settings, history, the explorer cache | `crates/workspace` |
| AI policy, table rules, PII masking, approval | `crates/ai` |
| Tauri: the engine reports through `EventSink` and has no Tauri dependency | `crates/app` |

## `QueryEngine` and its shared state

One `Arc<QueryEngine>` per process (`QueryEngine::new` in `lib.rs`). Every map is behind its own `parking_lot::Mutex`; none is held across an `.await`.

| Field | Key → value | Entries leave when |
|---|---|---|
| `sessions` | `(connection_id, tab)` → `Live { session, _tunnel, sandboxed, last_used }` | idle close, `disconnect`, `reconnect`, `close_tab`, `release_sessions`, `drop_session` |
| `connecting` | same key → a `tokio::sync::Mutex` (`ConnectGate`): one connection attempt per key | the last waiter drops its gate |
| `jobs` | job id → `JobHandle { tab_id, session_key, cancel, tab_closed }` | `run_job` ends; an export on a shared session ends |
| `creds` | connection id → `CredentialSource` (token caches survive reconnects) | `forget_credentials`, `set_interaction` |
| `tests`, `exports` | id → `CancellationToken` | the test or export ends |
| `exported` | export id → file written | never (process lifetime) |
| `manual_commit` | set of session keys | `set_manual_commit(key, false)` (the app calls it on tab close) |
| `outputs` | `Arc<OutputRegistry>` (own lock, see below) | |

`jobs` is also the "busy" list: `start_run` checks and claims a tab and its session key under one lock. `explain`, `apply_edits`, `end_transaction` and `export_query` only look at it.

## Sessions

A session is one database connection, found by `(connection_id, tab)`. `session_for` (`sessions.rs`) returns the live one or connects: `Connector::prepare`, `preflight` (`probe.rs`), then tunnel + stored secret + handshake inside the connect timeout (`connect_timeout_for`; not applied to browser sign-in or local files). A refused sign-in refreshes the credential once and tries again.

| Tab key | Used by | Closed by |
|---|---|---|
| the editor tab id | runs of the tab, its plans, grid edits, Commit / Rollback | `close_tab`, idle close |
| `nb:{notebook}` (`RunRequest::session_key`) | every cell of a notebook; each cell keeps its own `tab_id` for results | `close_tab` from `api/library.rs` |
| `__meta__` (`META_TAB`) | explorer, completion, `check_sql`, `describe`, test data's reads, cost preview while the tab is busy | `reconnect`, idle close |
| `__index__` (`INDEX_TAB`) | `bulk_metadata`: reading columns for the AI, schema compare, related rows. Local files use `__meta__` (`index_tab`) | `end_index_session`, `reconnect` |
| `ai:{session}`, `ai-outputs:{session}`, `ai-sample:{session}` | AI and MCP tools (`crates/ai/src/tools/query.rs`) | idle close |
| `job:{job}:{node}`, `…:load`, `…:export`, `dryrun:…` | job steps (`crates/app/src/jobs.rs`) | `release_sessions`, `close_tab` |
| `__load__:{id}`, `__loadcheck__:{uuid}` | "Load into…" and its dry run (`api/results.rs`) | `close_tab` right after |
| `__export__:{id}` | a full export on its own session | `drop_session` when it ends |

Rules:

- **Sandbox.** SQL from `Origin::Ai` or `Origin::Mcp` on DuckDB runs in a session opened with `SANDBOX_OPTION` unless the connection's AI policy allows external access (`sandboxed`). A session opened without the sandbox is never handed to such a run: it is replaced. A DuckDB file has one database instance, so opening one kind closes idle sessions of the other kind.
- **In use.** A session is busy when a job names its key, when someone else holds its `Arc` (`Arc::strong_count > 1`: a catalog call, an export, a load) or when `in_transaction()` is true. `close_idle_with` and `reconnect` keep busy sessions; `disconnect`, `close_tab` and `release_sessions` drop the map entry regardless (a statement still running keeps its handle until it ends).
- **Idle close.** The app calls `close_idle_with(idle_disconnect())` every minute (`crates/app/src/lib.rs`). Limit: the connection's `idle_disconnect_minutes` option, else the setting (default 60, `0` = never).
- **Reconnect.** `with_meta` / `with_tab` retry once on a connection error with a new session. Runs do not: a connection error fails the statement, `run_job` drops the session and the next run connects again. `reconnect(connection)` is the explorer's Refresh: closes `__meta__` and every session that is not busy.
- **Concurrency.** One run per tab and per session key (`busy` error otherwise). Catalog calls share `__meta__` and may overlap; the connector serializes them.

## Life of a run

```
RunRequest ──► start_run (run.rs, synchronous)
   1. plan: split_statements + classify           (connector-core)
   2. bind_parameters (:name), classify again
   3. read-only connection: every statement must pass read_only_allows → else "invalid"
   4. not confirmed and safety_reasons non-empty  → RunResponse::NeedsConfirmation
   5. output name: validate_name, check_owner (job steps own their names)
   6. jobs lock: tab or session key already there → "busy"; else insert JobHandle
   7. outputs.begin_run(tab): the tab's previous outputs become evictable
   8. tokio::spawn(run_job)                       → RunResponse::Started { job_id, statements }

run_job, per statement, stops at the first error or cancel
   StatementStarted
   begin_if_manual: manual commit + DML/DDL + no transaction → BEGIN
   run_statement: session_for (cancellable) → Session::execute(batch 2000, max_rows = limit + 1)
   collect_stream: Schema → ResultStore::create("{job_id}:{index}")
                   Batch  → push; Progress at most every 150 ms; past the limit → cut, cancel, drain ≤ 15 s
                   Downloading / Notice
                   Done   → finish; any other way out removes the stored result (Unfinished guard)
   outputs.add → handle rN     history row (workspace.add_history)
   StatementFinished | StatementFailed (connection error: the session is dropped)
after the loop
   set_name on the last output, remove the JobHandle (free outputs if the tab closed meanwhile)
   OutputsChanged, then JobFinished { status, in_transaction }
```

- Events go to the `EventSink` given to `QueryEngine::new`. The app passes an `EventHub`, which forwards to the UI (`job-event`) and to a broadcast channel of 1,024 events.
- `run_and_wait` (AI, MCP, job steps, compare) passes a `Waiter`: an unbounded channel that gets only `StatementFinished`, `StatementFailed` and `JobFinished`, so a lagging broadcast subscriber can't lose the end of a run. `NeedsConfirmation` becomes an error there; these callers set `confirmed: true` after their own checks.
- `row_limit` `None` or `0` keeps every row. Rows stay in memory (`ResultStore`); there is no limit in bytes.

## Cancellation

| What | How |
|---|---|
| A run | `cancel(job_id)` cancels the job's token. Each statement runs with a child token passed to the connector in `ExecOptions::cancel`; the connector stops the server side and the stream ends with a `cancelled` error. The loop checks the token before each statement. Status `Cancelled`. |
| Row limit reached | `collect_stream` cancels only that statement's child token, then reads the stream to its end (15 s at most) so the cancel can't hit the next statement. |
| Closing a tab | `close_tab` cancels its jobs, drops its sessions, frees its unnamed, unpinned outputs. |
| `run_and_wait` | forwards the caller's token to `cancel` once. |
| Export, connection test | `cancel_export`, `cancel_test`: own token maps. |
| Load | the caller's token, checked before each statement and passed to it (`exec` in `load.rs`). |
| Indexing | `end_index_session` drops `__index__`; closing the connection stops the statement. |

`begin_if_manual`, `apply_edits`, `explain`, `end_transaction` and dry runs have no cancel.

## Transactions

- **Auto-commit** is the default: the engine sends statements as written.
- **Manual commit** (`set_manual_commit`, per session key; the UI offers it for Postgres, MySQL, SQL Server, SQLite, DuckDB and Snowflake, `TX_KINDS` in `EditorPane.tsx`): before DML (and DDL, except on MySQL) `begin_if_manual` sends the engine's BEGIN (`edit::apply_style`) when `in_transaction()` is false.
- **State** is `Session::transaction_state()`: the badge (`QueryEngine::in_transaction`, `JobFinished.in_transaction`) and Commit/Rollback use only a known open one; idle close and Refresh keep a session whose state is unknown.
- **Commit / Rollback**: `end_transaction` refuses while a job uses the session, then sends `COMMIT` (repeated while a transaction is still open, for SQL Server's nesting; 32 at most) or `ROLLBACK`.
- A session with an open transaction is never closed for being idle and survives `reconnect`. `disconnect` and `close_tab` drop it, which rolls back on the server.
- Loads and grid edits open their own transaction (below). Three places name the statements: `edit::apply_style` (manual commit, edits), `traits().load.transactions` (loads), and `COMMIT` / `ROLLBACK` in `end_transaction`.

## Safety checks: who checks what

| Check | Where | Applies to |
|---|---|---|
| Classification (`Read`, `Dml`, `Ddl`, `Session`, `Transaction`, `Other`, `Unknown`) | `connector-core/src/sql/classify.rs` | everything below |
| Read-only connection: only reads and harmless `USE` / `SET` | `start_run` (`read_only_allows`); `prepare_load`; `edit_target`. Postgres also sets the transaction read-only in its connector | runs, loads, grid edits |
| UPDATE / DELETE without WHERE asks | `safety_reasons` | runs with `confirmed: false` |
| Production connection: all but reads, transaction control and harmless `USE` / `SET` ask | `prod_needs_confirmation`, `safety_reasons` | runs with `confirmed: false` |
| One run per tab / session | `start_run` (`jobs`) | runs; others only look |
| Full export runs one read statement | `export_query` | export |
| `EXPLAIN ANALYZE` only for reads | `explain` | plans |
| Cost preview: one statement, read or DML | `cost_statement` | cost preview |
| DuckDB sandbox for AI and MCP SQL | `sandboxed`, `session_for`; enforced in the DuckDB connector | runs and loads with `origin` Ai / Mcp |
| Output columns the AI must not see | `RunRequest::output_masks` → `ExecOptions` | AI runs on DuckDB |
| AI policy, table rules, approval, row cap, PII | `crates/ai/src/policy.rs`, `access.rs`, `pii.rs`, before `run_and_wait` | AI and MCP |

Not asked by the engine: loads (`load_rows`), grid edits (`apply_edits`) and `end_transaction` do not go through `safety_reasons`. They check read-only only; a production target is not confirmed here. Before / after SQL of a load step runs as written. Grid edits show their statements in the UI first; the app builds them again on the server (`api/results.rs` `apply_edits`), so the UI can't send SQL of its own.

## Outputs

Every result set of a run becomes an output (`OutputRegistry::add`): handle `rN` (counter saved in the `outputs_seq` setting), optional name, SQL, connection, limit, `truncated`. The rows are in the `ResultStore` under `result_id`; the registry holds metadata and decides when rows are freed.

| State | Meaning |
|---|---|
| `active` | From its tab's latest run (`begin_run` clears it). Never evicted. |
| recent | `Live`, not active, not pinned. Freed least recently used first above the budget (`enforce_budget`, 1 GB by default, counted from the size at registration). |
| pinned | Never evicted; `set_pinned` writes a Parquet snapshot under `<app data>/outputs/` and an `outputs` row. Loaded on first use after a restart (`OnDisk` → `ensure_loaded`). |
| `Evicted` | Rows gone, metadata kept; at most `MAX_ENTRIES` (400) entries in all, oldest evicted ones dropped first (`trim_entries`). |

- **Names.** `set_name` moves a name to the newest output; older ones become `name__1` … (`KEEP_VERSIONS` = 5). A job step's name belongs to it (`check_owner`).
- **Freed by:** `close_tab` (the tab's unnamed, unpinned outputs), `enforce_budget`, `remove` / `remove_unpinned`, a version past the fifth, an unfinished statement (`Unfinished` in `run.rs`).
- **On quit** `persist_active` saves each tab's latest outputs (64 MB each, 512 MB in all) and drops saved ones that are no longer current; `set_snapshot_dir` restores them for tabs that still exist.
- **SQL over outputs.** `OutputTables` implements `ExternalTables`: DuckDB sessions read `results.<handle or name>` through `ensure_loaded` and `sql_ready_all` (decimal text becomes `DECIMAL`).
- **Compare.** `diff_sql_opts`, `compare_summary_sql`, `key_candidates`, `unique_check_sql` build DuckDB SQL over two outputs; the app runs it on the "Results (DuckDB)" connection (`results_connection`).

Code that removes rows from the `ResultStore` directly (`api::release_result`, the AI's `get_sample_rows`) leaves the output entry as it is.

## Load

`load_rows` / `write_output` (`load.rs`) write rows into a table on the session `tab` (its own: `__load__:…` or `job:…:load`).

1. `prepare_load`: read-only check, probe the table (`SELECT … WHERE 1 = 0`), match columns by name, `check_load_target` when it is missing, build the plan (`Act`): before SQL, create / drop / delete, INSERTs (or staging table + `MERGE` / `UPDATE` + `INSERT`), Sync's delete, after SQL.
2. `transaction(dialect, plan)`: one transaction when `traits().load.transactions` has one and, on MySQL, nothing in the plan is DDL. Else statements commit one by one and the report says so.
3. `run_plan` sends the statements; INSERT text is built batch by batch (`sqlgen::InsertSql`). Progress goes to a `LoadObserver`.
4. On failure `abandon` rolls back, drops a real staging table and drops the session.

Modes (`LoadMode`): Append, Truncate (delete rows, insert), Replace (drop, create, insert), Update, Merge, Sync (merge, then delete rows whose key is not among the rows; refused with no rows).

`dry_run_load`: with a transaction the plan really runs on the sample rows and is rolled back; without one each read or DML statement is checked with `Session::check_sql` and the rest is listed as unchecked. The session is dropped afterwards.

## Grid edits, plans, export, and the rest

- **Grid edits** (`edit.rs`). `source_table` parses the result's SQL: one table, no join, grouping, DISTINCT or CTE. `resolve_source` describes it (explorer cache first when opening, live when saving) and `map_columns` says which result column shows which table column. `edit_target` needs a primary key in the result. `edit_statements` builds one UPDATE / INSERT / DELETE per change with `edit_literal`; `apply_edits` runs them on the result's session: in a transaction of its own, in one PL/SQL block on Oracle, inside the user's open transaction when there is one, else one by one. A statement that touches a number of rows other than 1 fails the apply (`conflict`).
- **Plans** (`plan.rs`). `explain` runs the engine's EXPLAIN on the tab's session (so temp tables and the open transaction are seen) and parses it into `PlanNode`s (`postgres_plan`, `arrow_tree`, `duckdb_plan`, `mssql_rows`, `oracle_rows`, `snowflake_plan`). `plan_support` says who can plan and analyze. `estimate_cost` asks `Session::estimate_cost`, 60 s at most.
- **Full export** (`export.rs`). `export_query` runs the output's SQL again without a limit and streams batches to a `FileWriter` (temp file, renamed at the end). Own session, except when the original session has an open transaction: then that one, and the tab is busy meanwhile.
- **Schema compare** (`schema_diff.rs`). `schema_diff` reads both sides with `bulk_metadata`, `diff_metadata` compares, `migration_script` writes a script for the target. Nothing runs; DROPs are comments.
- **Related rows** (`related.rs`). `result_links` lists foreign keys from the table (describe) and to it (knowledge index, else `bulk_metadata` of its schema); `related_rows_sql` builds the SELECT, which the UI runs like any query.
- **Test data** (`testdata.rs`, `testdata_pattern.rs`). `test_data_plan` describes the table, `generate_test_data` / `generate_test_data_with` make rows (keys from `MAX`, parent keys read on `__meta__`, patterns, optional AI values). It writes nothing: the app stores the rows as an output and the load path loads them. See ARCHITECTURE.md section 9.

## Errors

`EngineError { kind, message, code, position }` (`error.rs`) goes to the UI as is. `kind` is a string: `auth`, `connection`, `query`, `cancelled`, `unsupported`, `config`, `internal`, `not_found`, `invalid`, `busy`, plus `timeout`, `conflict`, `reauth_required`, `metadata`. `From` impls map `ConnectorError` (its `ErrorKind` name), workspace, result-store and auth errors. `code` carries cases the UI acts on (`missing_parameters`, `ssh_host_key_unknown`); `position` underlines the error in the editor together with `StatementFailed.statement_start`.

Refusals (read-only, busy, nothing to run) are returned by `start_run`. Statement errors arrive as `StatementFailed`; the script stops there.

## Tests

| Where | Covers |
|---|---|
| `crates/query-engine/src/tests.rs` (SQLite file, `CollectingSink`) | parameters, notebook sessions, history, row limit, manual commit, idle close, error offsets, cancel, one run per tab, confirmation, read-only, `reconnect`, credential-store timeout, unfinished results, outputs of a closed tab |
| Unit tests in `outputs.rs`, `load.rs`, `edit.rs`, `plan.rs`, `schema_diff.rs`, `related.rs`, `probe.rs`, `sessions.rs`, `testdata*.rs` | registry lifecycle and compare SQL, plan parsing, column mapping, script text, gates |
| `crates/app/tests/` (`load_result.rs`, `edit_result.rs`, `export_full.rs`, `plan.rs`, `jobs.rs`, `example_data.rs`) | the same paths through the app's API |
| `crates/ai/tests/agent_e2e.rs` | AI and MCP runs through `run_and_wait`, the sandbox |
| Connector live tests (`#[ignore]`, `DATABRAIN_*`) | real servers |

A change to a safety check needs a test for the allowed and the refused case (AGENTS.md).
