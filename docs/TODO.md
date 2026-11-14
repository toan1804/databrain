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
- **Table rules know only the first schema of a `search_path`.** Unqualified names are completed with the schema the server reports as current (`QueryEngine::default_namespace`, asked before the assistant's queries and jobs on a connection with rules). A table that isn't in that schema but in a later one of the path is still checked under the first; and an output the user made after `SET search_path` in their own session is checked under the connection's schema, not that session's. BigQuery, SQLite and DuckDB keep their fixed defaults. Checked on live Postgres, MySQL and ClickHouse (`crates/app/tests/table_rules_live.rs`) and by hand on Oracle; not yet on SQL Server, Snowflake or Databricks.
- **PII rules follow tables, not views.** A view over a masked column is another table to the rules: only a rule without a table (`email`) masks it.
- **Notes about dropped or renamed tables stay** (hidden from the model, never removed), and there is no limit on the number of notes or proposals.
- **A schema that becomes empty keeps its tables in the AI's catalog** until the next full index (not confirmed; `kn_replace_schema_read` with an empty list).
- **Workspace SQLite on the async runtime**: `knowledge::retrieve` and a schema's index write run on the runtime's threads; a learn pass commits once per sighting.
- **Providers**: the OpenAI-compatible stream is still read as complete when it ends without `[DONE]` or a finish reason (some local servers end that way); Gemini's `thoughtSignature` is not kept with a tool call (newer models may want it back; not confirmed); the SSE reader has no cap on one line or one event; inline completions cancelled before the first byte are counted as zero tokens.

## Connectors (review of 2026-11-15)

Fixed on 2026-11-15: catalog queries built with a literal a backslash could end (MySQL, Snowflake, Databricks, Redshift, Postgres, BigQuery paths), the crashes on text outside ASCII (MongoDB, Neo4j), Databricks running a write twice, `results.<name>` hidden behind a comment, and the DuckDB sandbox writing into attached folders. Left, most serious first:

**Stop and the row limit**
- **Oracle:** Stop during a large fetch drops a half-read frame from the helper; the session is then dead or hangs (`oracle/src/exec.rs`, `Agent::read` is not cancel-safe). One reader task owning the helper's output would fix the class. A statement cancelled while it waits for the session still runs (both drivers).
- **MySQL:** cancel waits on the connection mutex its own statement still holds when the kill fails or takes more than 10 s: the session blocks for good (`mysql/src/session.rs`). A cancel during row streaming often sends no `KILL QUERY`.
- **MySQL, SQL Server:** the row limit inside `CALL p()` / `EXEC p` abandons the rest of the procedure and reports success. Only classified reads should be abandoned.
- **Postgres:** inside a transaction the row limit reads the whole result to its end before finishing; a cancel that races a ready row is not sent to the server.
- **SQLite, DuckDB, Oracle (thick):** cancelling a statement that waits for the session interrupts the one that is running, and the cancelled one then runs.
- **MongoDB, Neo4j:** cancel and the row limit only drop the local future; the server keeps working. **ClickHouse** relies on the server noticing the closed response.
- **Snowflake, BigQuery, Databricks:** a failed poll returns the error without cancelling the query on the server. Snowflake's submit URL carries two `requestId`s (the abort sends the first; which one the server uses is not confirmed). BigQuery polls and cancels with the configured location, not the job's.

**Wrong results or state**
- **Oracle:** `create … procedure` whose `end;` starts a line loses its final `;` (`prepare_sql` looks for `" END"`); a thick read-only session reads as of connect time; the thin read-only transaction may reconnect per statement (not confirmed); materialized views list as tables; `RAW` shows as `RAW(0)`; `DBMS_OUTPUT` is not read.
- **Postgres, MySQL:** `transaction_state` is wrong after a failed `COMMIT` (Postgres observes the text before running it) and after DDL commits implicitly (MySQL).
- **SQL Server:** `decimal(18,0)` shows as `12345.0`, `datetime` fractions are truncated, `money` goes through a float, `datetimeoffset` loses its offset; the `Prefer` fallback to no encryption never runs (tiberius returns an error, the code expects a panic); a database named `a.b` is split wrongly in all-databases mode.
- **ClickHouse:** an exception mid-stream in a one-column result is shown as a row.
- **MongoDB:** options the shell parser doesn't know are dropped (a `collation` on `deleteMany`), `deleteMany()` without a filter deletes everything, integers above 9.0e15 lose digits; `mapReduce` with an `out` document and `createCollection … viewOn` don't report the collections they touch.
- **Neo4j:** durations and temporal values inside lists or maps are shown wrongly; a Cypher `\uXXXX` escape could hide a write keyword from the classifier (server behaviour not confirmed).
- **SQLite:** a value that doesn't match its column's declared type shows as NULL; `max_rows` is not read; tables named `sqlite?…` are hidden (`_` is a LIKE wildcard).
- **DuckDB:** attached-file views are created in the user's database (`files` schema), which fails on a read-only file and persists views in a writable one; a hostile `.xlsx` can use unbounded memory; `RETURNING` after a newline is not seen; Decimal256 outputs load as DOUBLE; one file can be opened twice (read-only and read-write); an output loaded inside a rolled-back transaction is remembered as loaded. A view the assistant made over `results.<name>` in one chat can be read by another chat on the same sandboxed instance.
- **Databricks:** catalog reads are cut at 100,000 rows without notice; BINARY shows in three forms.

**Connections and secrets**
- **TLS:** Postgres and MySQL `Prefer` falls back to plaintext, `Require` accepts any certificate (as libpq does); MySQL's fallback also triggers on a wrong password. Only `VerifyFull` verifies; there is no CA-file option.
- **Hosts are not validated** (Snowflake account, Databricks host): `evil.com/a` or `host@evil.com` sends the credential elsewhere; an imported connection is the realistic route. Snowflake's presigned chunk URL can appear in an error message.
- **SQL Server and MySQL keep the credential** for reconnects (an Entra token expires after about an hour).
- **Oracle:** the Instant Client install uses a predictable folder in a shared `/tmp` on Linux; `DATABRAIN_ORACLE_AGENT` runs an unverified helper; the helper keeps running a statement after DataBrain dies.
- **Redshift read-only** rests on `SET SESSION CHARACTERISTICS … READ ONLY`, which may be a no-op there (not confirmed).
