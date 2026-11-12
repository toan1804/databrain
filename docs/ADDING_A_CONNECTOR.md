# Adding a database type

This guide is for developers and AI coding agents adding a new connector (database type) to DataBrain. A new type has to work with every DataBrain feature: connections, explorer, editor, result grid, plan, export, outputs, lineage, schema compare, jobs, AI assistant and MCP. Where the database can't do something, DataBrain must say so in the UI, in the error, or to the AI. A feature must never break silently or show a wrong result.

[ARCHITECTURE.md §5](ARCHITECTURE.md#5-connector-layer) explains the connector layer. This guide is the checklist. Paths are given with the symbol to look for, because line numbers move.

## Contents

1. [Rules](#1-rules)
2. [Steps](#2-steps)
3. [Feature by feature](#3-feature-by-feature)
4. [Lists kept by hand](#4-lists-kept-by-hand)
5. [Support matrix](#5-support-matrix)
6. [Tests](#6-tests)
7. [Docs](#7-docs)
8. [Notes for AI agents](#8-notes-for-ai-agents)

## 1. Rules

- **Declare, don't special-case.** Describe the database in its `SourceTraits` constant (`crates/connector-core/src/traits.rs`) and its `ConnectorInfo` (`info()` in the connector crate). Shared code reads those. Add a `match` on `ConnectorKind` only when no field fits, and then consider adding a field instead.
- **Every feature works or says why not.** For each row of [section 3](#3-feature-by-feature), decide which applies:
  - **Supported:** implement it and test it.
  - **Not supported:** set the field or flag that turns it off, so the UI hides it or the call returns `ConnectorError::unsupported(…)` / `EngineError::new("unsupported", …)` naming the database (`kind.traits().name`) and what to do instead.
  - **Partly supported:** support what works and refuse the rest with a message, as ClickHouse loads do (Insert and Replace, but no Update or Merge by key).
- **Safety checks come first.** Statement classification (read or write), the read-only setting, the AI policy and PII masking must be right for the new type before anything else. A wrong "read" lets the AI or MCP write. See [§3.13](#313-safety-classification-read-only-ai-policy).
- **Unknown means ask.** If the classifier can't tell what a statement does, it must return `StatementKind::Unknown` (asks the user), not `Read`.
- **Record what you decided.** Fill in the new column of the [support matrix](#5-support-matrix) and the connector paragraph in `docs/FEATURES.md`.

## 2. Steps

1. **Kind.** In `crates/connector-core/src/types.rs` add the variant to `ConnectorKind`, to `ConnectorKind::ALL` (bump the length) and to `as_str()`. The string is the id everywhere: serde, the UI, `sourceTraits.json`, saved connections. Never change it later.
2. **Traits.** In `crates/connector-core/src/traits.rs` add a `SourceTraits` constant, starting from the closest existing one or `..STANDARD`, and its arm in `ConnectorKind::traits()`. [§3](#3-feature-by-feature) says which feature each field drives. A new sqlparser dialect needs a `SqlParser` variant and an arm in `dialect_of` (`crates/connector-core/src/sql/mod.rs`). A new query language needs a `QueryLanguage` variant and a `classify` arm in `crates/connector-core/src/language.rs`.
3. **Server read-only.** If the server itself can make a session read-only (so functions can't write either), add the kind to `ConnectorKind::server_read_only()` and to the hard-coded list in `ui/src/components/connection/GeneralSection.tsx` (the Read-only help text).
4. **Crate.** Create `crates/connectors/<name>`. Copy the layout of `crates/connectors/clickhouse` (server over the network) or `crates/connectors/sqlite` (local file or in-process engine):

   | Module | Contents |
   |---|---|
   | `lib.rs` | `XConnector`, `impl Connector`: `info()`, `validate()`, `connect()` |
   | `config.rs` | `ConnectionConfig` → driver settings (host, port, TLS, options) |
   | `error.rs` | Driver errors → `ConnectorError` with the right `ErrorKind` (Auth, Connection, Query, Cancelled…), `code` and `position` |
   | `types.rs` | Native types → Arrow `ColType` |
   | `exec.rs` | `execute`: stream rows as Arrow batches (`BatchBuilder`), honour `ExecOptions` (row limit, cancel) |
   | `schema.rs` | Catalog queries |
   | `session.rs` | `impl Session`, plus the live tests |

   The crate uses `version.workspace`, `edition.workspace`, `license.workspace` and `[lints] workspace = true`. Add it to `members` in the root `Cargo.toml`.
5. **Register.** In `crates/app/Cargo.toml` add the crate as an optional dependency, add a feature (`x = ["dep:databrain-connector-x"]`) and put it in the `default` and `system-duckdb` feature lists. In `crates/app/src/api/mod.rs`, `registry_with_outputs`, add `#[cfg(feature = "x")] r.register(Arc::new(XConnector::new()));`. The app, `databrain-mcp` and `databrain-jobs` all use that registry.
6. **Generate.** Run `UPDATE_SOURCE_TRAITS=1 cargo test -p databrain-connector-core traits` (writes `ui/src/lib/sourceTraits.json`), then `cargo test --workspace` (regenerates `ui/src/lib/bindings/*.ts`, including `ConnectorKind.ts`). Commit both. CI fails when either is stale.
7. **Walk [section 3](#3-feature-by-feature)** and [section 4](#4-lists-kept-by-hand). Every `match` with a `_ =>` arm compiles without you, so review each one listed there.
8. **Test and document** ([§6](#6-tests), [§7](#7-docs)).

## 3. Feature by feature

Each part lists what drives the feature, what you implement or set, and how "not supported" shows.

### 3.1 Connection dialog and saving

- **Form:** built from `info().fields` (`FieldSpec::new(key, label).required().placeholder(…).help(…)`). `key` is `host`, `port`, `database`, `file_path`, or any other name, which is stored in `config.options`. Required fields are checked by the app. `Connector::validate()` adds checks with a short message ("Choose a database file").
- **Auth:** `info().auth_methods`, recommended first (`AuthMethodKind` in `crates/auth`). Labels come from `AUTH_LABELS` in `ui/src/components/connection/parts.tsx`. A CLI login needs `CLI_HELP`/`CLI_LABEL`, a token `TOKEN_LABEL`. `connect()` must refuse a credential it doesn't take with `ConnectorError::config("X requires …")`. `auth_context()` gives OAuth endpoints and CLI hints.
- **SSL:** `traits.ssl_setting = true` shows the SSL mode list. Otherwise handle TLS in `config.rs` (an option or the URL scheme).
- **SSH tunnel:** `info().capabilities.ssh`. The dialog shows the SSH section only when it is true, and `validate_config` (`crates/app/src/api/connections.rs`) refuses SSH when `traits.cloud` or `traits.local`. Set `ssh: false` for HTTPS APIs and local files.
- **Order in the type list:** `KIND_ORDER` in `ui/src/components/connection/parts.tsx`. A kind missing from it sorts last.
- **Requirement note:** `info().note` shows a runtime requirement (a client library, a driver).
- **Connect timeout and network check:** `traits.network` (`HostPort`, `HttpsHost`, `Unknown`) decides the DNS + TCP probe before connecting (`crates/query-engine/src/probe.rs`). `traits.local` skips the probe and the connect timeout. One-time setup (downloading a driver) goes in `Connector::prepare`, which the timeout doesn't count.
- **Secrets:** never store a password in `ConnectionConfig`. The credential comes from `creds.get()` right before connecting and must not be kept. A password inside a connection string field must be moved to the keychain on save (see `take_uri_password` in `crates/app/src/conn_io.rs`) and redacted (`crates/connector-core/src/sql/redact.rs`) if the database's SQL can carry one (`CREATE USER … PASSWORD`, linked servers).
- **Import:** `crates/app/src/conn_io.rs`. Add an arm to `dbeaver_kind` (DBeaver and DataGrip driver names), map fields in `parse_dbeaver`, set the default auth in `auth_for`, and parse URLs in `config_from_url` if the database has a URL form. Not importable: say so in `docs/FEATURES.md`.

### 3.2 Explorer

- **Levels:** `traits.levels`. `Two` (schema.table) or `Three` (catalog.schema.table; also implement `Session::list_catalogs`, and return `SchemaInfo::in_catalog` ids from `list_schemas`). `ThreeWithoutDatabase` is for SQL Server style names.
- **Names:** `traits.catalog_noun`, `table_folder`, `view_folder` (MongoDB "Collections", Neo4j "Labels").
- **Folders shown even when empty:** `ENGINE_GROUPS` in `ui/src/lib/catalog.ts`. A kind missing from it shows only folders that have objects.
- **Objects:** `list_objects` with the right `ObjectKind`. Routines are `Function`/`Procedure`/`Package`; use `Other` rather than a wrong kind.
- **Columns and keys:** `describe` returns `ObjectDetail` with columns (`is_primary_key` matters for grid edits), foreign keys (ER diagram, AI joins) and comments.
- **Show DDL:** `object_ddl`. The default uses `describe().ddl`. Return `Ok(None)` when the database can't produce DDL; the UI then says so.
- **Preview ("first rows"):** `traits.preview` and `traits.row_limit` (`preview_query`).
- **Search and completion:** `search_objects`, `search_schema` and `search_routines` have defaults that list schemas. Override them with one catalog query when the database has a catalog view, so large schemas aren't transferred.
- **Refresh:** `explorer_fingerprints`, `object_versions` and `list_objects_named` let a refresh fetch only changed objects. The defaults (`None`) re-read the schema.
- **Indexes and partitions (query hints):** `table_layout`. The default reports only the primary key.

### 3.3 Editor

- **Statement splitting:** `traits.syntax` (`SqlSyntax`): quoting, comments, `$$`, `DELIMITER`, block ends (`GO`, `/`, `END;`), `:name` parameters. Splitting is a safety check: each statement is classified on its own. Add splitter cases to `crates/connector-core/src/sql/splitter.rs` tests.
- **Highlighting:** `DIALECTS` in `ui/src/components/sqlAssist.ts` (CodeMirror dialect; missing falls back to `StandardSQL`).
- **Completion:** `DIALECT_FUNCTIONS` in `ui/src/lib/sqlComplete.ts` for common functions. Table and column completion uses the explorer. Non-SQL languages get no SQL completion.
- **Format:** `traits.formatter` (sql-formatter language). `""` turns Format off; for a non-SQL language Format says it works on SQL only.
- **Check before run:** `traits.explain` (a prefix such as `"EXPLAIN "`) lets `Session::check_sql` report unknown tables and columns without running. `None` turns it off.
- **Cost preview:** `traits.cost_preview` shows the editor's Cost button and bar; implement `Session::estimate_cost` (returns a `CostEstimate`: bytes, rows, partitions, notes) without running the statement. `false` (the default) hides it; `QueryEngine::estimate_cost` refuses other kinds with "X gives no cost estimate before a run".
- **Manual commit:** `TX_KINDS` in `ui/src/components/EditorPane.tsx` shows the auto/manual commit toggle and the Commit/Rollback badge. Add the kind only when `Session::transaction_state` reports an open transaction correctly (follow the statements with `ConnState` when the driver can't tell). `begin_if_manual` in `crates/query-engine/src/sessions.rs` assumes DDL is transactional except on MySQL; check yours.
- **Cancel:** honour `ExecOptions.cancel` in `execute` and stop the query on the server, not only the stream. Set `capabilities.cancel` truthfully.

### 3.4 Result grid and editing

- **Grid, sort, filter, charts, value panel:** work on Arrow batches, nothing to do. Map types well in `types.rs`: dates, decimals and binary must not become text. Nested values go in as JSON text columns.
- **Editing rows** (`crates/query-engine/src/edit.rs`): needs a SQL language and a table with a primary key.
  - `fold()`: how unquoted names are stored (upper on Oracle and Snowflake, lower on Postgres). It duplicates `traits.upper_folding`; add your kind if it folds.
  - `apply_style()`: the transaction that wraps the edits. Without an arm, edits commit statement by statement. Oracle uses a PL/SQL block.
  - Non-SQL kinds are refused with a message (`result_source`).

### 3.5 Query plan

- `plan_support()` in `crates/query-engine/src/plan.rs` says whether the kind has Explain and Analyze, and `explain_on` reads the plan. Without an arm the Plan tab returns "X has no query plan DataBrain can read".
- `PLAN_KINDS` in `ui/src/components/PlanTab.tsx` mirrors `plan_support`. Keep them the same.

### 3.6 Export

- File formats (CSV, Parquet, XLSX, JSON…) work on Arrow, nothing to do.
- **SQL INSERT export and load SQL:** `traits.load` (`LoadTraits`, see [§3.9](#39-jobs-and-loads)). `crates/export/src/load.rs` falls back to standard SQL when `load` is `None`.
- **Full export** (all rows, re-run on its own session) only runs read queries. It needs correct classification.

### 3.7 Outputs and Results (DuckDB)

Outputs of any connection are Arrow and can be joined in the Results connection (`results.<name>`). Nothing to do, as long as types map well. Only DuckDB reads outputs directly; don't add that to a new kind.

### 3.8 Lineage and schema compare

- **Lineage:** `crates/lineage` parses with `dialect_for(kind)` (from `traits.parser`). Non-SQL languages are refused ("Lineage traces SQL; X queries aren't supported", `crates/app/src/api/lineage.rs` and `ui/src/components/openTabs.ts`). With `SqlParser::Generic`, check that the database's common statements (CTEs, `CREATE TABLE AS`, `INSERT … SELECT`, `MERGE`) still parse; add a lineage test.
- **Schema compare:** `ui/src/components/CompareTab.tsx` offers SQL connections. `migration_script` in `crates/query-engine/src/schema_diff.rs` writes the target's DDL with `quote_ident`; check `ALTER TABLE` syntax and foreign key forms for the new kind.
- **ER diagram:** from `describe()` foreign keys.

### 3.9 Jobs and loads

- **Query steps:** any kind. Steps run through the same engine as tabs.
- **Load target:** `traits.load`. `None` = not a load target; the job editor (`ui/src/components/job/LoadFields.tsx`) doesn't offer it, and `is_load_target` returns false. Otherwise set:
  - `types` (`TypeNames` for `CREATE TABLE`), row and byte batch limits with a note naming the server limit;
  - literal forms (`bool_words`, `binary_literal`, `text_literal`, `typed_datetime_literals`, `insert_all`);
  - `create_if_missing`, `stage`, `merge`, `probe_limit0`, `delete_all`, `create_suffix`, `explicit_null`, `insert_settings`;
  - `keyed: false` when the database has no row-level UPDATE/MERGE (hides Update, Merge and Sync by key);
  - `transactions: None` when the load can't run in one transaction (each statement commits; the run says so). `ddl_commits: true` when DDL commits implicitly.
- **Names in load problems and DDL hints:** `catalog_word`, `schema_word`, `create_catalog_sql` and `create_schema_sql` in `crates/query-engine/src/load.rs`.
- **Tables a step reads and writes** (job dependencies, AI table rules): `table_refs` in `crates/connector-core/src/tables.rs` (SQL via the parser; non-SQL languages are `opaque` unless you add an analyzer, as MongoDB does with `set_mongo_analyzer`).
- **Text escaping** of `:name` values and loads: `quote_string` in `crates/connector-core/src/sql/params.rs`. Add an arm if the database reads `\` in strings or doesn't take `''`.

### 3.10 AI assistant

- **Prompt:** `traits.dialect_name` names the language. A non-SQL language needs `language_rules` in `crates/ai/src/agent.rs` (how to write a query, with examples).
- **Row caps:** `traits.row_limit` (`cap_rows` in `crates/ai/src/policy.rs`). Non-SQL queries aren't capped by DataBrain; the connector must honour `ExecOptions.max_rows`.
- **Table access rules:** `defaults()` in `crates/ai/src/access.rs` gives the default catalog and schema that complete an unqualified name. Without an arm the rules can't resolve unqualified names, so they match less.
- **PII masking:** `crates/ai/src/pii.rs` works on SQL. A non-SQL language needs its own handling (see MongoDB) or a note that masking applies only to result columns.
- **Knowledge (reading columns for the AI):** `bulk_metadata` and `schema_object_counts`; `explorer_fingerprints` must cover everything the explorer lists (tables, views, routines, sequences, columns, comments), as both the explorer and index runs use it. The defaults describe tables one by one, which is slow on large databases; override them with catalog queries. `BULK` in `ui/src/lib/indexScope.ts` marks kinds whose metadata comes in large batches.

### 3.11 MCP

Nothing kind-specific. MCP uses the same tools and policy. It works once classification, read-only and the AI policy are right.

### 3.12 History, audit and redaction

History, the job log and the AI audit log store SQL through `redact_text`. If the database's SQL can carry secrets in its own syntax, add the form to `crates/connector-core/src/sql/redact.rs` with a test.

### 3.13 Safety: classification, read-only, AI policy

- `classify(sql, kind)` (`crates/connector-core/src/sql/classify.rs`) decides read, write (`Dml`), schema change (`Ddl`), session state or `Unknown`. Test the database's own write forms: procedure calls, `EXPLAIN ANALYZE` of a write, `SELECT … INTO`, writes inside CTEs, statements without a separator.
- `read_only_allows` and `unknown_functions` (`crates/connector-core/src/sql/read_only.rs`) decide what runs on a read-only connection and which function calls need confirmation.
- `server_read_only()` decides whether a read-only connection is enforced by the server or only by DataBrain's classifier. Implement it in `connect()` when the config says `read_only` (a read-only transaction, session or file), and test that a write fails on the server.
- `connector-core/src/invariants.rs` runs splitting, classification and quoting properties over `ConnectorKind::ALL`, and `fuzz/` fuzzes them. Run them for the new kind.

## 4. Lists kept by hand

These compile without the new kind and silently give it the fallback. Check each one.

| Place | What it decides | Fallback for a missing kind |
|---|---|---|
| `ConnectorKind::server_read_only` (types.rs) | Server-enforced read-only | Not server-enforced |
| `GeneralSection.tsx` Read-only help list | Help text | "No read-only session" text |
| `plan_support` (plan.rs) + `PLAN_KINDS` (PlanTab.tsx) | Plan tab | No plan |
| `apply_style`, `fold` (edit.rs) | Grid edit transaction, name folding | Statement by statement, names as typed |
| `TX_KINDS` (EditorPane.tsx) | Manual commit toggle | Auto-commit only |
| `begin_if_manual` (sessions.rs) | DDL inside manual commit | DDL transactional |
| `catalog_word`, `schema_word`, `create_catalog_sql`, `create_schema_sql` (load.rs) | Load messages and DDL hints | "Catalog", "Schema", `CREATE SCHEMA` |
| `defaults` (ai/access.rs) | AI table rules for unqualified names | No defaults |
| `quote_string` (sql/params.rs) | String literal escaping | `''` doubling |
| `migration_script` (schema_diff.rs) | Schema compare DDL | Standard DDL |
| `language_rules` (ai/agent.rs) | AI prompt for non-SQL | Exhaustive on `QueryLanguage` |
| `dbeaver_kind`, `auth_for`, `config_from_url` (conn_io.rs) | Import | Not imported |
| `KIND_ORDER`, `AUTH_LABELS`, `CLI_HELP`, `CLI_LABEL`, `TOKEN_LABEL` (parts.tsx) | Dialog order and labels | Last; generic labels |
| `DIALECTS` (sqlAssist.ts) | Highlighting | Standard SQL |
| `DIALECT_FUNCTIONS` (sqlComplete.ts) | Function completion | Common functions only |
| `ENGINE_GROUPS` (catalog.ts) | Explorer folders | Only non-empty folders |
| `BULK` (indexScope.ts) | Index batch sizing | Normal batches |

Search for more before you finish: `rg "ConnectorKind::" crates --glob '!crates/connectors/**'` and `rg '"(postgres|mysql|clickhouse)"' ui/src`.

## 5. Support matrix

What each connector supports today, from the code above. Add a column for the new type. ✓ = supported, – = not supported (hidden or refused with a message), and a note where it is partial.

| Feature | SQLite | Postgres | MySQL | SQL Server | Oracle | Snowflake | Databricks | BigQuery | DuckDB | ClickHouse | MongoDB | Neo4j |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| Query language | SQL | SQL | SQL | SQL | SQL | SQL | SQL | SQL | SQL | SQL | Shell commands | Cypher |
| Name levels | 2 | 2 | 2 | 3 | 2 | 3 | 3 | 3 | 3 | 2 | 2 | 2 |
| SSH tunnel | – | ✓ | ✓ | ✓ | ✓ | – | – | – | – | ✓ | ✓ | ✓ |
| Read-only enforced by the server | ✓ | ✓ | ✓ | – | – | – | – | – | ✓ | ✓ | – | – |
| Manual commit | ✓ | ✓ | ✓ | ✓ | – | – | – | – | – | – | – | – |
| Check SQL before run (EXPLAIN) | ✓ | ✓ | ✓ | – | – | ✓ | – | – | ✓ | ✓ | – | ✓ |
| Plan tab | Explain | Explain + Analyze | Explain + Analyze | Explain | Explain | Explain | Explain | – | Explain + Analyze | – | – | – |
| Cost preview before a run | – | – | – | – | – | EXPLAIN (partitions, bytes) | EXPLAIN COST (table statistics) | dry run (bytes) | – | – | – | – |
| Format | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | generic SQL | – | – |
| Grid edits | ✓ in a transaction | ✓ in a transaction | ✓ in a transaction | ✓ in a transaction | ✓ in a PL/SQL block | ✓ in a transaction | per statement¹ | per statement¹ | ✓ in a transaction | per statement¹ | – | – |
| Lineage | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | – | – |
| Schema compare | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | – | – |
| Job load target | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ | Insert / Replace only | – | – |
| Load in one transaction | ✓ | ✓ | ✓ unless DDL | ✓ | – | – | – | – | ✓ | – | – | – |

Sources: `ui/src/lib/sourceTraits.json` (language, levels, explain, formatter, load), each connector's `info().capabilities.ssh`, `server_read_only`, `TX_KINDS`, `plan_support`, `apply_style`. Grid edits also need a primary key that `describe` reports.

¹ Only where `describe` reports a primary key. The Databricks and BigQuery metadata queries report none, and ClickHouse reports its sorting key; edits on these three have not been checked against a live server.

## 6. Tests

- **Unit tests** in the crate: type mapping, config and URL building, error mapping (with a TLS or auth error case), catalog query building.
- **Live tests** in `session.rs`, `#[ignore = "live: needs DATABRAIN_<X>_HOST (cargo test -- --ignored)"]`, reading `DATABRAIN_<X>_*` variables. Cover connect, execute with types, list schemas and objects, describe, cancel, read-only refusing a write, and a load if it is a load target. List the variables in the README's Test section. If a Docker image exists, add a service container to the `live` job in `.github/workflows/ci.yml` and the crate to its `cargo test -p …` line.
- **Shared tests:** add the kind's cases to the classifier and splitter tests (`crates/connector-core/src/sql/`), `quote_ident`/`split_path` tests (`crates/connector-core/src/lib.rs`), `preview_queries` (traits.rs), `insert_statements` (`crates/export/src/load.rs`), and an AI policy test with both the allowed and the refused case (`crates/ai/src/policy.rs`).
- **Checks** before committing: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets` (no warnings), `cargo +1.89 check --workspace --all-targets` (the MSRV), `cargo test --workspace`, and `cd ui && npm run lint && npx tsc -b && npx vitest run`.

## 7. Docs

- `README.md`: the list of databases and the live test variables.
- `docs/FEATURES.md`: a paragraph for the connector under **Connections** (auth, what it supports, what it doesn't and why), and the "not load targets" line if it isn't one.
- This file: the new column in the [support matrix](#5-support-matrix).
- `CHANGELOG.md`: a line under **Unreleased → Added**.

## 8. Notes for AI agents

When you add or change a connector:

1. Read this file, then [CONNECTOR_CORE.md](CONNECTOR_CORE.md) (what each `Session` default and each SQL stage means), `crates/connector-core/src/traits.rs` and the connector closest to the new one.
2. Make a plan that goes through every part of [section 3](#3-feature-by-feature) and every row of [section 4](#4-lists-kept-by-hand), and decide for each: supported, not supported (and how that shows), or partial. Show that plan to the user before writing code, with the features that won't be supported.
3. Prefer a `SourceTraits` field over a new `match` on `ConnectorKind`.
4. Never mark something a read to make a feature work. When unsure, the classifier returns `Unknown`.
5. Don't guess driver behaviour. If you can't run the live tests, say which ones you couldn't run.
6. Regenerate `sourceTraits.json` and the bindings, run the checks in [§6](#6-tests), and update the docs in [§7](#7-docs) in the same change.
7. In the final report, list the features that are not supported and how the user sees that.
