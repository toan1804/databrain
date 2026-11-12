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

## Explorer and AI index (left from the review of 2026-11-12)

- PostgreSQL row estimates are refreshed only when the schema changes: the explorer fingerprint leaves out `reltuples`, and since index runs use the same fingerprint (2026-11-13) they skip such schemas too, so the AI's row estimates go stale as well.
- The ER diagram (`schema_tables`) is always read live and doesn't work offline.
- `clear_explorer_cache` is a command the UI never calls.

## Connector core (review of 2026-11-12; see [CONNECTOR_CORE.md](CONNECTOR_CORE.md))

Every finding of the review was fixed on 2026-11-13, with the never-read corpus and the whole-vs-pieces property (`crates/connector-core/tests/classify_corpus.rs`), tri-state `Session::transaction_state` with the unused `Capabilities` fields removed, keyword and positional secrets in redaction, and one scanner for strings and comments (`skip_token`). A `Session::supports(Feature)` enum wasn't added: the optional features already say "not supported" through their own return values (`object_versions` → `None`, `check_sql` → `false`). The redaction functions keep their own, dialect-free lexing on purpose: they scrub any text (history, logs, error messages) without knowing its database.

Ideas still open:

1. **Function allow-list for reads** (medium; a product decision). Unparsed text, hidden statements and the acting-function list are covered; left is treating a SELECT that calls a function outside the built-in list as `Unknown`. Only SQL Server (functions can run extended procedures) and Oracle (autonomous-transaction functions) need it, and there it would refuse many ordinary reads on read-only connections (`regexp_count`, `sys_context`, `dbms_lob.substr`, any user function) until the built-in lists are complete. The AI path already asks about such functions (`unknown_functions`).
2. **Typed execution request** (medium to large). `Session::execute` taking a statement that can only be built from classified, bound text. Every path that runs user text already binds then classifies (`bind_parameters`; lineage since this change); the type would make that structural, but changes all 13 connectors and every caller, and generated SQL (loads, plans, metadata) needs a way through. `mysql_async` has no switch for multi-statement mode, so that part stays in the classifier (both readings of backslashes in `classify`).
3. **Redaction by token stream** (medium). The keyword-then-literal rules and the table of positional secrets cover the known misses; a tokenizer-based rewrite would make new forms easier to add. Risk: over-hiding in saved history.
4. **One `ParsedStatement`** (large). Span, tokens, AST or parse error, and kind, shared by split, parameters, classify, read-only and `table_refs`. `skip_token` removed the drift between the splitter and the parameter finder; this would also remove the second tokenization by sqlparser. Risk: gaps in the parser's tokenizer become split failures.
