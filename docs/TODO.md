# Known gaps to fix later

Found in reviews and left for a later release on purpose. Each says what goes wrong; remove an entry when it is fixed.

## Single program (verify on Windows before relying on it)

- An update while an MCP client is connected: the installer may end the MCP session (`databrain.exe mcp`), or hit "file in use" on the `databrain-mcp.exe` launcher.
- If an MCP client kills `databrain-mcp.exe` without closing its stdin, the child `databrain.exe mcp` may keep running (no job object ties them).
- `databrain-jobs run` / `list` output in a terminal goes through the GUI-subsystem launcher's console attach; check it prints in cmd and PowerShell.

## Results on disk (left from the spill work of 2026-11-14)

- Charts and pinning refuse a result on disk, and the AI's local aggregate tool sends the AI to `query_outputs` instead.
- A result on disk isn't saved when the app quits (tab results are saved up to 64 MB anyway), so its tab shows nothing after a restart.
- Files of results on disk are plain Parquet in DataBrain's folder, sensitive columns included (they're masked only on the way to the AI). They're deleted with the result, or at the next start after a crash; until then they're as readable as the user's other files.
- Column statistics of a result on disk count distinct values up to a million ("+" past it); the top values come from the values counted.
- A sort or filter of a result on disk applies to the rows loaded in the grid only (at most 256 MB of them, `WINDOW_BYTES`); every row needs SQL on `results.<name>`.

## Query engine (review of 2026-11-12; see [QUERY_ENGINE.md](QUERY_ENGINE.md))

The review's findings are fixed (loads can't control their transaction and ask on production connections, a run's job guard, one transaction dialect with a savepoint for grid edits, session claims for plans, edits, Commit/Rollback and exports, a memory cap per result, outputs freed with their rows, and the low items). Left, as refactors:

- **One guarded entry point.** Read-only and production checks are still made by each entry point (`start_run`, `prepare_load`, `edit_target`), next to the `claim`. `engine.guarded(session_key, origin, intent)` returning the claim after those checks would keep a new entry point from skipping one.
- **Split `outputs.rs`** into `outputs/registry.rs`, `outputs/duckdb.rs` and `compare.rs`. `ResultStore::remove` is still public: other crates are expected to call `OutputRegistry::release_result` (an output whose rows are gone would stay listed otherwise); making it crate-private needs the result store's other users (`library.rs`) to go through the registry too.
- **AI chat tabs** (`ai:{session}`) close when the chat is deleted, not when it is left: the last outputs of an open chat stay active (never evicted) until then or a restart.
- **Typed session roles.** `enum SessionRole { Tab, Notebook, Meta, Index, Ai, JobStep, Load, Export }` instead of string keys (`__meta__`, `ai:…`), with idle close, reconnect, sandbox and cleanup decided per role.
- `blocking()` (`lib.rs`) hands the worker thread over only on a multi-thread runtime; on a current-thread one the workspace and file writes still block it.

Left from the final review (2026-11-13):

- MySQL statements that commit by themselves without being transaction control (`SET PASSWORD`, `RESET …`, `LOAD INDEX INTO CACHE`) in a load's before/after SQL: a dry run on MySQL uses a transaction and would keep its sample rows after one. Use the transaction method on MySQL only when every user statement is a read or DML.
- A grid edit that fails with an error that ends the whole transaction (SQL Server `XACT_ABORT`, a deadlock, DuckDB's aborted transaction): the savepoint rollback fails and the message says the earlier changes are pending when they are gone.
- The cost preview only looks at the tab (it must not make Run busy): when it reconnects after a connection error while a script runs, the script's next statement lands on the new session.
- The memory cap is per result: a script whose results are all active can hold several times the budget; `get_array_memory_size` counts buffers shared between batches once per batch.
- `release_result` on a pinned output when no snapshot directory is set removes the output (only the AI's sample path calls it).
- Implicit transactions (SQL Server `IMPLICIT_TRANSACTIONS ON`, Snowflake `AUTOCOMMIT = FALSE`) keep the session (`TxState::Unknown`) but don't show Commit/Rollback or warn on closing the tab (`QueryEngine::in_transaction` counts `Open` only). Not followed: `SET ANSI_DEFAULTS ON`, `autocommit = 'false'` written as a string. A one-batch `begin tran … commit` on SQL Server stays "open" (conservative). With implicit transactions on, manual commit's own `BEGIN` may nest on SQL Server.
- Job steps: a DuckDB Query step reading a tab's row-limited `results.x` on purpose now fails before it runs (it would pass part of the rows on); say so in the job editor.
- `loadResultAsking`: when the server asks (the UI didn't know the connection is production), progress and Stop already show during the prompt.

## Explorer and AI index (left from the review of 2026-11-12)

- PostgreSQL row estimates are refreshed only when the schema changes: the explorer fingerprint leaves out `reltuples`, and since index runs use the same fingerprint (2026-11-13) they skip such schemas too, so the AI's row estimates go stale as well.
- The ER diagram (`schema_tables`) is always read live and doesn't work offline.
- `clear_explorer_cache` is a command the UI never calls.

Left from the final review of the metadata store (2026-11-13):

- A table's search text includes the names of the tables its foreign keys point at, so searching for a table outside the scope ranks allowed tables that reference it (the keys themselves are scrubbed).
- Scope rules match names ignoring case: on Postgres, allowing `sales` also allows `"Sales"`.
- The scope survives pointing a connection at another server (`meta_check_identity` keeps the rules): schemas allowed on the old server are allowed on the new one once browsed. Decide whether a server change should clear it.
- `meta_clear` and `kn_clear` are not one transaction each. The v20 migration gives a schema whose last index run failed a rule too.
- `user_version` is read outside the migration transaction: two processes upgrading at once (the app and `databrain-mcp`) can make one fail with "table already exists" (it opens on the next try).
- An index run's empty foreign key list never clears keys cached before (bulk metadata often has none): keys dropped on the server stay in the ER diagram and the AI's joins until the table is described again.
- An AI note's target is stored as `schema.table` unquoted: a name with `.`, a space, `&` or `,` reads back differently.
- `index_tables` answers "not available" for connection, sign-in and permission errors too (one answer for missing and refused tables), which hides why a read failed.
- The pre-check in `index_tables` and `describe_named` resolve a bare name with the explorer cache, not the AI's scope; the check after the describe is what decides.

## Connector core (review of 2026-11-12; see [CONNECTOR_CORE.md](CONNECTOR_CORE.md))

Every finding of the review was fixed on 2026-11-13, with the never-read corpus and the whole-vs-pieces property (`crates/connector-core/tests/classify_corpus.rs`), tri-state `Session::transaction_state` with the unused `Capabilities` fields removed, keyword and positional secrets in redaction, and one scanner for strings and comments (`skip_token`). A `Session::supports(Feature)` enum wasn't added: the optional features already say "not supported" through their own return values (`object_versions` → `None`, `check_sql` → `false`). The redaction functions keep their own, dialect-free lexing on purpose: they scrub any text (history, logs, error messages) without knowing its database.

Left from the final review (2026-11-13):

- Databricks raw strings `r'…'` end at the first `'` on the server but are read with backslash escapes by the splitter (only matters if the API ever runs several statements).
- DuckDB `select e'\''; drop …`: the whole text classifies as a read while its pieces don't (sqlparser's DuckDB dialect has no E-strings); runs classify the pieces.
- Functions with side effects still read as reads: Postgres `brin_summarize_new_values`, `brin_summarize_range`, `gin_clean_pending_list`, `pg_stat_reset_subscription_stats`, `pg_sync_replication_slots`, `pg_replication_origin_session_reset`; DuckDB `sqlite_attach`; Databricks `http_request`; ClickHouse `url()`, `remote()`, `mysql()`, `s3()` (its server applies read-only itself). `SELECT … FOR UPDATE` is a read (locks; Oracle doesn't refuse it in read-only mode).
- Transaction tracking: a failing `COMMIT` (or `commit prepared`) on Postgres resets the count while the transaction stays open (statements are observed before they run). SQL Server could ask the server for `@@TRANCOUNT` instead of following text.
- Redaction keeps: connection strings and settings outside quotes in AI text (`Server=h;Password=pw;`, `PGPASSWORD=pw`), URL query tokens (`?access_token=…`), the part after `\"` in `identified by "p\"w"`, and positional `pgp_sym_encrypt(data, 'pw')` / `aes_encrypt(x, 'key')`.

Ideas still open:

1. **Function allow-list for reads** (medium; a product decision). Unparsed text, hidden statements and the acting-function list are covered; left is treating a SELECT that calls a function outside the built-in list as `Unknown`. Only SQL Server (functions can run extended procedures) and Oracle (autonomous-transaction functions) need it, and there it would refuse many ordinary reads on read-only connections (`regexp_count`, `sys_context`, `dbms_lob.substr`, any user function) until the built-in lists are complete. The AI path already asks about such functions (`unknown_functions`).
2. **Typed execution request** (medium to large). `Session::execute` taking a statement that can only be built from classified, bound text. Every path that runs user text already binds then classifies (`bind_parameters`; lineage since this change); the type would make that structural, but changes all 13 connectors and every caller, and generated SQL (loads, plans, metadata) needs a way through. `mysql_async` has no switch for multi-statement mode, so that part stays in the classifier (both readings of backslashes in `classify`).
3. **Redaction by token stream** (medium). The keyword-then-literal rules and the table of positional secrets cover the known misses; a tokenizer-based rewrite would make new forms easier to add. Risk: over-hiding in saved history.
4. **One `ParsedStatement`** (large). Span, tokens, AST or parse error, and kind, shared by split, parameters, classify, read-only and `table_refs`. `skip_token` removed the drift between the splitter and the parameter finder; this would also remove the second tokenization by sqlparser. Risk: gaps in the parser's tokenizer become split failures.
