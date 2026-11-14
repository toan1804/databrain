# Connector core

`crates/connector-core` is what every connector and every caller of a connector share: the `Connector` and `Session` traits, the per-engine facts (`SourceTraits`), the value and stream types results travel in, and the SQL handling that decides what a statement is before it runs. It talks to no database and has no Tauri dependency. This page says what the crate owns, what each trait method and each SQL stage promises, and what each one takes for granted.

Main files (`crates/connector-core/src/`):

| File | What it holds |
|---|---|
| `lib.rs` | `Connector`, `Session`, `TxState`, `ConnectorRegistry`, the option names `SANDBOX_OPTION`, `CONNECT_TIMEOUT_OPTION`, `IDLE_DISCONNECT_OPTION` |
| `catalog.rs` | Default catalog helpers (`search_by_listing`, `default_bulk_metadata`, `assemble_metadata`), `query_text` / `query_table`, index and list parsing |
| `quote.rs` | `quote_ident`, `quote_path`, `quote_object`, `split_path`, `quote_literal` |
| `types.rs` | `ConnectorKind`, `ConnectionConfig`, `ConnectorInfo`, `Capabilities`, `FieldSpec`, catalog types (`SchemaInfo`, `DbObject`, `ColumnInfo`, `ObjectDetail`, `TableLayout`, `CostEstimate`, `SchemaMetadata`), search ranking |
| `traits.rs` | `SourceTraits` and one constant per kind, `SqlSyntax`, `LoadTraits`, `preview_query` |
| `value.rs` | `Value`, `ColType`, `Column`, `BatchBuilder`, date and time parsing and formatting |
| `stream.rs` | `ExecOptions`, `StreamEvent`, `QueryStream`, `StreamSender`, `Collected` |
| `error.rs` | `ConnectorError`, `ErrorKind` |
| `sql/splitter.rs` | `split_statements`, `split_with`, `statement_at`, `leading_keyword` |
| `sql/params.rs` | `find_parameters`, `render_param`, `quote_string`, `substitute_parameters` |
| `sql/classify.rs` | `classify`, `StatementKind`, `Classification` |
| `sql/read_only.rs` | `read_only_allows`, `unknown_functions` |
| `sql/redact.rs` | `redact_text`, `redact_secrets`, `redact_url_passwords`, `redact_key_values` |
| `language.rs` | Classification and table references for Cypher and MongoDB commands |
| `documents.rs` | `RowBatcher`: rows of documents or records to Arrow |
| `tables.rs` | `table_refs`: which tables a script reads and writes |
| `conn_state.rs` | `ConnState`: what a connection carries besides single statements |
| `external.rs` | `ExternalTables`, `OutputMasks`, `referenced_results`: query outputs as `results.<name>` |
| `invariants.rs` | Properties of splitting, classifying and quoting, for the seeded test and `fuzz/` |

## What the crate owns, and what it doesn't

Owns: the contract between the app and a database driver; the one description of each source type; the Arrow shape of a result; splitting, parameters, classification, the read-only rule and redaction of SQL text.

Does not own:

| Thing | Where |
|---|---|
| Driver code, catalog queries, server-side cancel, native type mapping | `crates/connectors/*` |
| When a statement runs, on which session, confirmation prompts, the row limit | `crates/query-engine` ([QUERY_ENGINE.md](QUERY_ENGINE.md)) |
| What the AI may run (`decide`, table rules, PII) | `crates/ai/src/policy.rs`, `access.rs`, `pii.rs` |
| Credentials | `crates/auth` (`CredentialSource`); the core only passes it to `connect` |
| INSERT / MERGE text for loads | `crates/export`, driven by `LoadTraits` |
| Where redacted text is stored | `crates/workspace` |

The core decides *what a statement is*. Whether it may run is decided by the callers: `start_run` and `prod_needs_confirmation` (`query-engine/src/run.rs`) and `review_sql` (`ai/src/policy.rs`).

## `Connector`

One per database type, registered in `ConnectorRegistry` by the app (`register`, `get`, `infos`).

| Method | Required | Default | Used for |
|---|---|---|---|
| `info()` | yes | | `ConnectorInfo`: form fields, auth methods, `Capabilities`, `traits` |
| `kind()` | no | `info().kind` | |
| `validate(cfg)` | no | `Ok(())` | A message before saving or testing; required `FieldSpec`s are checked by the app already |
| `auth_context(cfg)` | no | empty | OAuth endpoints and CLI hints |
| `prepare(cfg)` | no | nothing | One-time local setup (a driver download); outside the connect time limit |
| `connect(cfg, creds)` | yes | | A new `Session`. Credentials are read from `creds` at that moment and not kept |

`Capabilities` has only `ssh` (the connection form). Whether the server itself keeps a read-only connection from writing is `ConnectorKind::server_read_only()` (`types.rs`); other features are reported by the session method that provides them.

## `Session`

One live connection, shared behind `Arc`; methods take `&self` and the connector serializes access where its driver needs it.

Required: `kind`, `server_version`, `ping`, `execute`, `list_schemas`, `list_objects`, `describe`.

| Defaulted method | Default | What the default means |
|---|---|---|
| `transaction_state()` | `TxState::Unknown` | Unknown counts as open: the session is kept (not idle-closed or replaced on Refresh), so a connector that doesn't implement it never loses work, but it also never gets the Commit/Rollback badge. Every connector implements it: Postgres, MySQL, SQL Server, DuckDB and Snowflake from the statements run (`ConnState`, MySQL also from the server's status flag), SQLite from the driver, and `None` for engines without transactions across statements (Oracle with autocommit, BigQuery, Databricks, ClickHouse, MongoDB, Neo4j). `in_transaction()` is "not `None`". With implicit transactions on (`ConnState`), `Unknown` until a write, `Open` after it until COMMIT or ROLLBACK. |
| `list_catalogs()` | `None` | A two-level engine: the explorer starts at schemas |
| `list_catalog_schemas(catalog)` | filters `list_schemas()` | Every schema is listed to show one catalog |
| `explorer_fingerprints()` | `None` | Every schema is re-read on each explorer check and index run. It must cover everything the explorer lists (tables, views, routines, sequences, columns, comments) |
| `object_versions(schema)` | `None` | A changed schema is re-listed whole, not table by table. When implemented it must cover exactly what `list_objects` returns, keyed by `version_key` |
| `list_objects_named(schema, names)` | lists the schema, filters | |
| `object_ddl(schema, name, kind)` | `describe().ddl` for relations, `None` otherwise | No DDL for routines; table DDL only as complete as `describe` makes it |
| `search_objects`, `search_schema`, `search_routines` | list and filter (`search_by_listing`, …) | Correct but slow on big catalogs: whole schemas are transferred |
| `package_members` | empty | Oracle only |
| `bulk_metadata(schemas)` | `default_bulk_metadata`: `list_objects` + `schema_columns` per schema | Two or more queries per schema; a schema's error is in its entry, not the call's |
| `schema_columns(schema)` | `describe` per object; objects that fail are skipped | One round trip per table |
| `schema_object_counts()` | `None` | The index can't be sized before it runs |
| `check_sql(sql)` | runs `traits().explain` + `sql` through `execute`, shifts the error position back | `Ok(false)` when `explain` is `None`. The text is sent as given: the caller passes one statement it has classified as a read |
| `table_layout(schema, name)` | primary key from `describe` | No index or partition hints |
| `estimate_cost(sql)` | `None` | Must agree with `SourceTraits::cost_preview` |

`execute(sql, opts)` takes one statement and returns a `QueryStream` (below). Nothing in the core checks that `sql` is one statement; the splitter is what makes it so, and what a driver does with text holding several is the connector's business.

## `SourceTraits` and the generated JSON

`traits.rs` has one `SourceTraits` constant per `ConnectorKind`, returned by `ConnectorKind::traits()`. Shared code reads its fields instead of matching on the kind.

| Group | Fields | Read by |
|---|---|---|
| Language | `language`, `parser`, `dialect_name`, `formatter` | `classify`, `table_refs`, lineage, AI prompts, the UI's Format |
| Names | `quote`, `upper_folding`, `levels`, `whole_path_quotes`, `always_qualify_catalog`, `catalog_noun`, folders | `quote_ident`, `split_path`, the explorer, completion |
| Rows | `row_limit`, `preview` | `preview_query`, `cap_rows` in `ai/src/policy.rs` |
| Checks | `explain`, `cost_preview` | `Session::check_sql`, the cost preview |
| Network | `cloud`, `local`, `network`, `ssl_setting` | the connection form, `probe.rs` |
| Lexing | `syntax` (`SqlSyntax`) | the splitter and `find_parameters` |
| Loads | `load` (`LoadTraits`, `None` = not a load target) | `crates/export`, job load steps |

`STANDARD`, `STANDARD_SYNTAX` and `STANDARD_LOAD` are the starting points; a new source overrides what differs. `every_kind_has_consistent_traits` checks the fields against each other.

The struct is `Serialize` and goes to the UI inside `ConnectorInfo.traits`. `ui/src/lib/sourceTraits.json` is a copy for the UI's tests and pure helpers; the app replaces it with the backend's at start. `traits::tests::ui_copy_is_up_to_date` fails when the copy is stale, and `UPDATE_SOURCE_TRAITS=1 cargo test -p databrain-connector-core traits` rewrites it. Other shared types reach the UI through `ts_rs` (`ui/src/lib/bindings/*.ts`, regenerated by `cargo test`).

## Values and types

A connector decodes each native value into a `Value` and pushes rows into a `BatchBuilder`, which emits Arrow `RecordBatch`es of one fixed schema.

| `Value` | `ColType` | Arrow type |
|---|---|---|
| `Bool` | `Bool` | Boolean |
| `Int(i64)` | `Int64` | Int64 |
| `UInt(u64)` | `UInt64` | UInt64 |
| `Float(f64)` | `Float64` | Float64 |
| `Text` | `Utf8` | Utf8 |
| `Bytes` | `Binary` | Binary |
| `Date(i32)` days since 1970-01-01 | `Date` | Date32 |
| `Timestamp(i64)` microseconds, no zone | `Timestamp` | Timestamp(µs) |
| `TimestampTz(i64)` microseconds, UTC | `TimestampTz` | Timestamp(µs, "UTC") |
| `Time(i64)` microseconds since midnight | `Time` | Time64(µs) |

- There is no decimal, 128-bit integer, interval, UUID, JSON or list type. A connector picks `Utf8` (exact text) or `Float64` for them and keeps the native name in `Column::db_type`, stored as field metadata `databrain.db_type` (`META_DB_TYPE`).
- Every field is nullable. Duplicate column names are kept.
- `ColBuilder::append` converts between kinds where it can: integers and booleans across the numeric builders, text parsed into numbers, dates and times (`parse_date`, `parse_datetime`, `parse_time`), any value into `Utf8` through `value_to_string`. A value that doesn't fit becomes NULL and is counted in `coercion_failures()`; reporting the count (a `Notice`) is the connector's job.
- The timestamp builders take `Timestamp` and `TimestampTz` values alike; the column type alone says whether the instant is UTC.
- `parse_datetime` reads `YYYY-MM-DD[ T]HH:MM:SS[.ffffff]` with an optional `Z`. It does not read offsets; a connector whose driver sends them parses them itself (`parse_timestamptz` in `connectors/postgres/src/types.rs`).
- `push_row` pads a short row with NULL and drops extra values.

`documents.rs` builds the same batches for sources without a fixed schema (MongoDB, Neo4j). `RowBatcher` holds the first `INFER_ROWS` (1,000) rows, takes the columns in first-seen order and infers each type (all ints → `Int64`, ints and floats → `Float64`, one other type → that type, mixed → text; nested values are JSON text). Fields that first appear later are dropped and named in a notice at the end; values that don't match the inferred type become NULL, also with a notice.

## The stream contract

`QueryStream::channel(capacity)` makes a bounded tokio channel of `Result<StreamEvent>`; the connector's producer task holds the `StreamSender`.

| Event | Rule |
|---|---|
| `Downloading(msg)` | At most once, before `Schema`: the server is done, the result is being fetched |
| `Schema` | Once, before any `Batch`; absent for statements without rows |
| `Batch` | Zero or more, each of that schema |
| `Notice` | Any time |
| `Done(ExecSummary)` | Last. A stream that closes without it did not finish: `collect()` reports "stream ended without completion" |
| `Err(ConnectorError)` | Ends the result for the consumer; the producer should stop after sending it |

- **Backpressure** is the channel bound: `send` waits while the consumer is behind. The bound counts events, not bytes.
- **Consumer gone.** `send` returns `false` once the receiver is dropped; a producer must stop then. `is_closed()` asks without sending. `blocking_send` is for producers on a blocking thread.
- **Cancel.** `ExecOptions::cancel` is cancelled by the engine on Stop or at the row limit; the connector stops producing, cancels on the server and ends with `ConnectorError::cancelled()`.
- **`max_rows`** is a hint (the row cap plus one). A connector may pass it to the server; the engine cuts the result either way.
- `ExecOptions::default()` (batch 1,000, a token nobody holds, no cap) is what `check_sql`, `query_text` and `query_table` use: those calls can only be stopped by dropping the stream.
- `output_masks` is for DuckDB sessions that load `results.<name>` tables (see below).

## The SQL pipeline

```
editor text
  │ split_statements(text, kind)            sql/splitter.rs
  ▼ StatementSpan { start, end, sql }
  │ substitute_parameters(sql, kind, values) sql/params.rs
  ▼ the SQL that will be sent
  │ classify(sql, kind)                     sql/classify.rs
  ▼ Classification { kind, missing_where, keyword }
  │ read_only_allows(sql, &c)               sql/read_only.rs   (read-only connections; production prompt for Session)
  │ unknown_functions(sql, kind)            sql/read_only.rs   (AI policy)
  ▼ run
  │ redact_text(sql or message)             sql/redact.rs      (history, job logs, AI transcripts and audit)
```

`QueryEngine::plan` splits and classifies; `bind_parameters` substitutes and classifies again, so the checks see the SQL that is sent (`query-engine/src/run.rs`).

| Stage | Guarantees | Assumes |
|---|---|---|
| Split | Spans are in order, don't overlap, and `text[start..end] == sql`. Comment-only pieces are dropped. Text inside quotes, comments and dollar quotes never ends a statement | The lexical rules in `SqlSyntax` match the server's. The splitter is hand-written and separate from sqlparser's tokenizer; its reading of comments and quoted text (`skip_token`, incl. `E'…'` and `q'[…]'` where the dialect has them) is shared with the parameter finder and `referenced_results` |
| Parameters | A value is one number, `NULL` / `TRUE` / `FALSE`, one string literal as typed, or a quoted string (`quote_string`), unless `raw`. `:name` inside strings, quoted names and comments is left alone | The same lexical rules as the splitter (both use `skip_token`). `raw` values and Oracle `&name` are inserted as typed |
| Classify | One `StatementKind` for the text; several parsed statements give the highest kind in the order `Read < Session < Transaction < Dml < Ddl < Other < Unknown` | The text is what the server will run (parameters already in). The sqlparser dialect reads it as the server does |
| Read-only | `true` only for `Read`, `USE` (not `USE ROLE` / `USE SECONDARY`) and `SET` of a name in `READ_ONLY_SETTINGS` | The `Classification` is of this same text |
| Redact | Only literal values (and the bare word after `IDENTIFIED BY`) are replaced by `***`; the statement keeps its shape | Text, not a parse: it works on SQL, error messages and connection strings alike |

### Splitting (`sql/splitter.rs`)

- `split_with` walks the bytes once. It skips `--`, `/* */` (the first `*/` ends it), `#` and `//` where the dialect has them, `'…'`, `"…"`, backticks, `[…]`, triple quotes and `$tag$…$tag$`, with backslash escapes where `SqlSyntax` says so.
- **Blocks** keep their inner `;`. `block_kind` looks at the first words of the piece (`block_starts`, `CREATE [OR REPLACE] <block_objects>`, MySQL `DEFINER`, Oracle `WITH FUNCTION`): Oracle blocks end at a `/` line, SQL Server ones at a `GO` line, `EndWord` dialects (SQLite, Postgres, MySQL, BigQuery) at the `;` after the matching `END`, counted over `BEGIN` / `CASE` / `END`.
- **Line directives**, checked at line starts outside quotes: MySQL `DELIMITER x`, Oracle `/`, SQL Server `GO [n]`.
- **Blank lines** end a statement written without `;` when the next line starts with a word of `STATEMENT_STARTS` and the piece before parses as one statement on its own (`parses_alone`). Not in a T-SQL script that contains `declare`.
- Languages that don't split on `;` (`QueryLanguage::splits_on_semicolon`, MongoDB) return the whole text (`language::split_whole`).
- `statement_at(spans, cursor)` picks the statement for "run at cursor".

### Parameters (`sql/params.rs`)

- `find_parameters` returns `:name` spans (off where `colon_params` is false). Not parameters: `::`, `:=`, a colon right after a name, digit, quote, bracket or `$` (`col:field`), and Oracle `:new` / `:old`. On Oracle it also returns `&name` / `&&name`, in code and inside `'…'`, marked `textual`.
- `render_param` decides the text of a value; `quote_string` is the one text escaper (also used by loads and SQL export): quotes doubled, backslashes doubled where the dialect may read them, `\'` on BigQuery and Databricks.
- `substitute_parameters` replaces every span or returns the names without a value.

### Classification (`sql/classify.rs`)

| Kind | Meaning | Callers treat it as |
|---|---|---|
| `Read` | Returns rows, changes nothing | runs |
| `Session` | `SET`, `USE`, `RESET`, `ATTACH`, `PRAGMA`… | prompt on production unless `read_only_allows`; AI asks |
| `Transaction` | `BEGIN`, `COMMIT`, `ROLLBACK`, savepoints | no production prompt; refused on read-only; AI asks |
| `Dml`, `Ddl` | Changes data / schema (`is_write()`) | prompt, refused on read-only |
| `Other` | Anything else that parses or has a known first word (`KILL`, `VACUUM`, `ANALYZE`) | as a write |
| `Unknown` | Can't be told | as a write: the user is asked |

How `classify` gets there:

1. Not SQL: `language::classify`.
2. Parsed by sqlparser with the kind's dialect: `classify_ast` per statement. Queries are walked through set operations and CTEs (`query_kind`): DML in a CTE is `Dml`, `SELECT … INTO` is `Ddl`. `EXPLAIN` takes the kind of what it explains, with or without `ANALYZE`. Other statements take `keyword_kind` of their own first word.
3. `check_control`: `Session` and `Transaction` are trusted, so they must be only that. `BEGIN` / `START` / `END` count only in their transaction forms, and any later word that acts makes the statement `Unknown`.
4. A statement below `Dml` that calls an acting function (`ACTING_FUNCTIONS`, any Snowflake `system$` function not in `SNOWFLAKE_READS`, `seq.nextval`, `NEXT VALUE FOR`) is `Other`.
5. Not parsed: `keyword_kind` of the first word, then stricter. `WITH` containing a DML word is `Dml`. A `SELECT` stays `Read` only when it has no DML word (`contains_dml_word`), no acting word (`contains_action_word`) and names no acting function; any other unparsed read-looking text is `Unknown`. Unparsed `Session` is `Unknown`; unparsed `Transaction` must consist of transaction grammar (`transaction_grammar`).

`missing_where` is set for `UPDATE` and `DELETE` without `WHERE` (parsed statements only). `keyword` is the first word after comments and opening parentheses.

### Read-only and functions (`sql/read_only.rs`)

- `read_only_allows(sql, &c)` is the rule for read-only connections (`start_run`, `read_only_decision` in the AI policy) and for which `Session` statements skip the production prompt (`prod_needs_confirmation`).
- `unknown_functions(sql, kind)` lists the functions a statement calls that are not in `PURE_FUNCTIONS` (a one-part name only: `evil.lower` is unknown). SQL that doesn't parse gives one entry saying so; non-SQL gives none. The AI policy asks the user about them where the server has no read-only session (`function_writes`).
- `ConnectorKind::server_read_only()` names the engines whose own read-only session stops writes inside functions: Postgres, MySQL, SQLite, DuckDB, ClickHouse.

### Redaction (`sql/redact.rs`)

`redact_text` is what callers use; it runs three passes:

| Pass | Hides |
|---|---|
| `redact_secrets` | The literal after a word of `SECRET_WORDS` (optionally `=`, `(`, `PASSWORD(`), and after `IDENTIFIED BY` / `IDENTIFIED WITH plugin BY|AS`; quoted, `E'…'`, `N'…'`, dollar-quoted, or a bare word after `IDENTIFIED BY`. Run again on text inside double quotes, because error messages quote SQL that way |
| `redact_url_passwords` | The password of `scheme://user:password@host` |
| `redact_key_values` | The value of `name = value` / `name: value` when the name is a secret's (`secret_name`: the words above or a suffix such as `password`, `pwd`, `_token`, `api_key`). Outside strings only quoted values; inside strings and comments bare values too (connection strings) |

A new secret syntax is added by word (`SECRET_WORDS`, `secret_name`) with a test for the hidden and the untouched case. Positional secrets (the fifth argument of a function) are not recognised.

## Other query languages (`language.rs`)

| Language | Split | Classified by |
|---|---|---|
| Cypher (Neo4j) | at `;` with the standard lexer | Words anywhere in the text: constraint or index with `CREATE` / `DROP` → `Ddl`; a write clause word → `Dml`; `CALL` → `Unknown`; otherwise `Read` only when the first word is a read clause, else `Unknown` |
| MongoDB commands | whole text | The connector's own parser, registered once per process with `set_mongo_analyzer` (`MongoInfo`: kind, collections read and written, `$lookup` joins). A command it can't parse is `Unknown`. Without an analyzer, text rules (`mongo_kind`); `$out` / `$merge` anywhere make an aggregate a write |

`mongo_table_refs` and `mongo_joins` feed `table_refs` and lineage. `unknown_functions` returns nothing for these languages.

## Smaller modules

- **`conn_state.rs`.** `ConnState::observe(sql, kind)` is called by a connector before each statement and keeps two facts: a transaction depth (nested for SQL Server) and a "has session state" flag (`SET`, `USE`, temporary tables, locks, prepared statements…). `in_transaction()` backs `Session::in_transaction`; `replaceable()` says the connection can be thrown away at the row limit instead of draining the result. It reads statement text only and errs towards "stateful"; what a procedure does is not seen.
- **`tables.rs`.** `table_refs(sql, kind)` returns `TableRefs { reads, writes, opaque, changes_default }` for the AI's table rules (`ai/src/access.rs`) and job dependencies. It fails closed: statements starting with a word of `OPAQUE_KEYWORDS`, unparsed SQL and other languages are `opaque`, and tables in the non-query parts of a write count as written. `USE` and `SET search_path` set `changes_default`.
- **`external.rs`.** Query outputs as `results.<name>` tables in DuckDB sessions. The engine implements `ExternalTables` and fills the `ExternalTablesSlot` the connector was built with; `referenced_results(sql)` finds the names. `OutputMasks` (in `ExecOptions`) lists, per result id, the columns to load as `***`; with masks set, an output that isn't listed is refused.
- **`invariants.rs`.** `check_split_classify` (spans cover their text in order; a non-read stays a non-read with a comment or whitespace around it) and `check_quoting` (`quote_ident` then `split_path` gives the name back). Public but `#[doc(hidden)]`: used by the crate's seeded test and the fuzz targets.

## Errors

`ConnectorError { kind, message, code, position }`, with `Result<T>` as the crate's alias.

| `ErrorKind` | Meaning | Callers |
|---|---|---|
| `Auth` | Sign-in refused (also from `AuthError`) | the engine refreshes the credential once |
| `Connection` | The link is gone | `with_meta` / `with_tab` reconnect once; a run drops the session |
| `Query` | The server refused the statement | shown with `code` and `position` |
| `Cancelled` | Stopped on request (`is_cancelled()`) | run status `Cancelled` |
| `Unsupported` | The engine or build can't do it | shown |
| `Config` | Settings are wrong or missing | shown in the connection form |
| `Internal` | A bug or an Arrow error | shown |

`position` is the 1-based character in the SQL when the server gives one; `check_sql` shifts it back by the length of the `EXPLAIN` prefix. Mapping driver errors to kinds is each connector's `map_err`.

## Tests

- Unit tests sit in each module (`mod tests`, about 55 in the crate): tables of `(kind, sql)` with the expected kind for the classifier, allowed and refused lists for `read_only_allows`, hidden and untouched text for redaction, split results per dialect.
- `invariants::tests` runs both properties on 3,000 generated scripts and names for every `ConnectorKind::ALL`, with a fixed seed, on each `cargo test`.
- `fuzz/fuzz_targets/split_classify.rs` and `quoting.rs` run the same properties under cargo-fuzz.
- `traits::tests` keeps `sourceTraits.json` and the traits table consistent.
- There are no tests against a real database here; each connector has its own.

Safety changes (classification, read-only, redaction) need a test for the allowed and for the refused case (AGENTS.md), and the features in [CHANGE_IMPACT.md](CHANGE_IMPACT.md) checked.
