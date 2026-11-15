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

**Wrong results or state** (fixed on 2026-11-15 except as follows; the rest was checked by unit tests only, live where noted)
- **Oracle:** the thin driver's fixes need the helper release `oracle-agent/v0.1.3` published (`docs/RELEASING.md`) before an app release ships them. The Instant Client (thick) path, its per-statement read-only transaction and `DBMS_OUTPUT`, is not tested live (the thin one is, on Oracle Free 23). A `DBMS_OUTPUT` line longer than 8000 bytes is cut. Read-only is not enforced by the server (confirmed live on 23): a `COMMIT` inside a block, or DDL, ends the READ ONLY transaction, and what is committed after it stays; the classifier refuses blocks and DDL on a read-only connection, and what isn't committed is rolled back as the statement ends.
- **SQL Server:** `money` comes from the driver as a float, so values beyond about ±900 billion may be off in the last digit; `datetimeoffset` is text with its offset, so it sorts as text, not by instant. Not run against a live server (the container never finished a login).
- **SQLite:** a value that doesn't fit its column's type after the first batch (the type is chosen from the first batch) shows as NULL, with a notice.
- **Neo4j:** whether the server decodes `\uXXXX` outside strings is not confirmed; the classifier reads both ways. Durations are read from the driver's debug output (neo4rs 0.8.0): recheck when upgrading.
- **Databricks:** the 100,000-row metadata check relies on `manifest.truncated` (as documented; not seen on a live workspace).
- **ClickHouse:** the mid-stream exception fix is tested against a fake server only.

**Connections and secrets**
- **TLS:** Postgres and MySQL `Prefer` falls back to plaintext, `Require` accepts any certificate (as libpq does); MySQL's fallback also triggers on a wrong password. Only `VerifyFull` verifies; there is no CA-file option.
- **Hosts are not validated** (Snowflake account, Databricks host): `evil.com/a` or `host@evil.com` sends the credential elsewhere; an imported connection is the realistic route. Snowflake's presigned chunk URL can appear in an error message.
- **SQL Server and MySQL keep the credential** for reconnects (an Entra token expires after about an hour).
- **Oracle:** the Instant Client install uses a predictable folder in a shared `/tmp` on Linux; `DATABRAIN_ORACLE_AGENT` runs an unverified helper; the helper keeps running a statement after DataBrain dies.
- **Redshift read-only** rests on `SET SESSION CHARACTERISTICS … READ ONLY`, which may be a no-op there (not confirmed).

## Review of v0.4.1..main (2026-11-15)

A review of the 54 commits since v0.4.1 (PRs #40–#53). Findings were checked on main at 2ab9270 unless the entry says otherwise. Each says where, what goes wrong, and the fix the review suggests.

**Fixed on 2026-11-15** (one commit each, on `fix/review-v0.4.1`): the nine bugs to fix before the next release (a DuckDB spill folder per instance; table rules refuse names that may resolve in several schemas, and fail closed when the lookup fails; scheduled AI steps learn the current schema; manual commit asks the server before leaving out BEGIN and reads autocommit only from settings; the `E'\''` / `"c\"` redaction leak; quadratic `sp_*` redaction; ClickHouse lake table functions; `#[ignore]` on the table-rules live tests; no second run of a DuckDB write), and the ten to fix: spill folders deleted on quit, older results on disk freed when the disk runs low, the first move to disk outside the result's lock, a server move making the next index ask, a provider switch checking every restricting connection, the sandbox refusing to seal without its spill folder, SQL Server lock hints, `EXEC` and failures syncing `@@TRANCOUNT`, dropped foreign keys leaving the ER cache. Left from them:

- **DuckDB `memory_limit` is per instance** (`app/src/api/outputs.rs` `duckdb_memory_limit`): each DuckDB database may use RAM − results budget − floor, so several working at once can ask for more than the computer has. DuckDB has no limit shared between instances and a sealed sandbox can't change its limit; the CHANGELOG now says so instead of claiming the two can't over-promise.
- **Switching a conversation's provider** is refused while any connection the AI may use doesn't allow the new provider, even one the conversation never reached: recording the connections whose data entered a session would allow more.

**Smaller**

- The on-disk notice (`run.rs` `on_disk_notice`) still says sorting and filtering read from disk; since 0258ea8 they apply to the first rows loaded in the grid. It reaches the AI too.
- The AI's `query_outputs` advice (`ai/src/tools/query.rs`) says DuckDB reads the files "without loading them"; in the sandbox every row is copied into an in-memory table that also spills, unchecked by `low_disk`.
- Excel import silently drops end blocks with under 5% of the cells after a 3-row gap (`connectors/duckdb/src/xlsx.rs` `table_span`); the only trace is a SQL comment. Show `left_out` as a notice or in the import dialog.
- MySQL loads: the implicit-commit check reads the first two whitespace words (`query-engine/src/load.rs`), so a comment before `SET PASSWORD` hides it and `SET autocommit = 1` (which commits) is treated as harmless. Use `code_words`.
- macOS free memory (`app/src/sysmem.rs`): `mach_host_self()` is called on every read and its port reference never released; speculative and purgeable pages are left out, so free memory reads low and results go to disk early.
- Each statement of a script keeps at least 4 MB that is never evicted (`run.rs`, `.max(4 << 20)`), so N statements can keep N × 4 MB.
- The `get_array_memory_size` entry (buffers shared between batches counted once per batch) was removed from this file in #45 without a fix.
- Closing a chat races its cancelled run (`aiCancel` and `aiLeaveSession` aren't ordered), so outputs of a statement that finishes after `leave_tab` stay active.
- `SessionRole::is_ai()` is never used: nothing checks that AI or MCP SQL runs on an AI-role session key.
- Redaction doesn't know Databricks raw strings: `options (password r'pw')` is kept.
- `ui/tsconfig.tsbuildinfo`, a build artifact, is committed (changed in 4529fa5).
- Pre-existing, unchanged since v0.4.1: `redact_url_passwords` is quadratic (`"a://b:c ".repeat(n)`: 9.2 s at 80 KB); secrets inside MySQL `/*! … */`, after a Postgres nested comment that hides a quote, after MySQL `5--1`, behind `_utf8mb4'pw'`, and in procedure arguments after a comment or a line break are kept.

## UI (second review, 2026-11-15)

A review of the fixes found fourteen bugs; they are fixed, with the refused case tested where it can be outside a running app. Left:

- **Query tips** don't see a column more than about 500 characters into a call as wrapped by it (`coalesce(` with thousands of arguments): the price of keeping long predicates fast.
- **SQL text with no known dialect** (a query tip without a connection) is read as DuckDB reads it: `$$` opens a string.

