# Known gaps to fix later

Found in reviews and left for a later release on purpose. Each says what goes wrong; remove an entry when it is fixed.

## Single program (verify on Windows before relying on it)

- An update while an MCP client is connected: the installer may end the MCP session (`databrain.exe mcp`), or hit "file in use" on the `databrain-mcp.exe` launcher.
- If an MCP client kills `databrain-mcp.exe` without closing its stdin, the child `databrain.exe mcp` may keep running (no job object ties them).
- `databrain-jobs run` / `list` output in a terminal goes through the GUI-subsystem launcher's console attach; check it prints in cmd and PowerShell.

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

## Lineage (review of 2026-11-13; see [LINEAGE.md](LINEAGE.md))

PII masking (`crates/ai/src/pii.rs`) masks a result column by what lineage traces it to, and falls back to "the SQL names a sensitive column" only when a column has no source at all. Where lineage traces a column wrongly or only in part, the value reaches the AI unmasked. Each of these was run with the rule `users.email`:

- **`* REPLACE`** (BigQuery, DuckDB, Snowflake, ClickHouse): `select * replace (email as name) from users` returns the emails as `name`, unmasked. `expand_star` ignores the wildcard's options: `REPLACE` expressions aren't traced, and `EXCLUDE` / `EXCEPT` columns are still drawn as flowing (that one only over-masks).
- **`UNPIVOT` / `PIVOT`**: `select * from users unpivot (val for col in (email, name)) p` returns emails in `val`, unmasked. The source becomes a `Table` node named after its whole text (`users UNPIVOT(…) p`), which no rule matches, and the table under it (`users`) is not a node at all. The same node lands in `unknown_tables`, so "Check with database" searches the server for that text.
- **Table functions over a column**: `select f.value from users u, lateral flatten(input => u.email) f` (Snowflake) and `… lateral string_to_table(u.email, '@') as g(v)` (Postgres) are unmasked: the function is a `Table` node with no link to its arguments. `UNNEST` is traced; other functions are not.
- **Hive / Databricks `LATERAL VIEW`**: `select t.tag from users u lateral view explode(split(u.email, '@')) t as tag` is unmasked. `select.lateral_views` isn't read, so `t.tag` falls to the struct-access guess below.
- **A partly traced expression**: `select o.id || email as x from orders o join users u on …` is unmasked. `email` can't be placed (two tables, columns unknown: a warning, no edge) but `o.id` is, so `x` has a source and the fallback never runs. Masking always passes `NoCatalog`, so every unqualified column in a join is in this case.
- **Struct-access guess**: `alias.col` with an alias that is no source is read as field `col` of a column named `alias` (`resolve_source`), without a warning when a table with unknown columns can take it. A wrong trace instead of none.

Fix direction: lineage reports per output column whether every reference in it was placed (an `untraced` flag on `Column`), and `masked_columns` treats such a column like one with no source; `expand_star` honours `EXCLUDE` / `REPLACE` / `RENAME`; `PIVOT`, `UNPIVOT` and table functions keep an edge from the table or columns they read. Each needs a masked and an unmasked test in `pii.rs`.

Wrong or missing lineage (the diagram):

- **Column lists cut the chain**: `with c(a, b) as (select id, name from users) select a from c`, `(select …) s(a, b)` and a recursive `r(n)`: `rename_columns` renames the node's columns but the edges into it keep the old names, so nothing feeds `c.a`.
- **Columns named like a pseudo-column are dropped silently**: `select level, name from users` gives `level` no edge and no warning (`refs::PSEUDO` holds `level`, `user`, `rowid`, `rownum`, `sysdate`… for every dialect). They should be values only where the dialect has them and no source has such a column.
- **Select aliases in `HAVING` / `QUALIFY`** (`having total > 5`, `qualify rn = 1`: MySQL, Snowflake, BigQuery, DuckDB): a false "Unknown column total", and no filter edge.
- **`CROSS APPLY` / `OUTER APPLY`** subqueries don't see the tables before them (only `LATERAL` does): false "Unknown table or alias" warnings, and the correlation isn't a join edge.
- **Lambda parameters** (`list_transform(tags, x -> x + 1)`) are read as columns: a false "Unknown column x".
- **`INSERT` without a column list** into a table whose columns aren't known, or with more values than the table has columns, names the target columns after the query's (`insert into orders select …, id as extra` adds `extra` to `orders`, no warning).
- **`BEGIN`, `COMMIT`, `SET`, `USE`** each warn "… statements are not traced yet; their tables are shown without columns". `USE` and `SET search_path` are not followed: later bare names resolve as before them.
- **Bare names** resolve to the cached object with the shortest schema id (`Workspace::meta_get`), not the session's schema; inside a view's definition they should resolve from the view's schema.
- **Case**: `"Orders"` and `orders` are one table node, and `"U"` and `"u"` one alias.
- `CONNECT BY` / `START WITH` columns aren't marked used. A table dropped and created again keeps the edges of its first version. `other()` lists the CTE names of an untraced statement as tables. `Refs::window` is set and never read.

Cost:

- **Quadratic in edges**: `Builder::edge` scans every edge for a duplicate, `add_out` and `warn` scan too. 400 `create table … as select *` statements over 30 columns (24,000 edges) take 13 s in a debug build; four CTEs over a 1,500-column table 0.6 s. Keep a set of edge keys.
- The app runs `analyze` on the async runtime (up to ten times per "Check with database"), and history learning runs `join_pairs` once per entry: neither is bounded or moved to a blocking thread.
- `fingerprint`: `a = -1` and `a = 1` differ (the sign is a token), and `VALUES (…), (…)` differs by row count, so the same query with other values can count as new.
- No fuzz target or corpus (the splitter and classifier have both).

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
