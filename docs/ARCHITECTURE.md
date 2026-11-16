# DataBrain architecture

DataBrain is a desktop SQL client written in Rust on Tauri 2, with a React UI and a built-in AI assistant. This document describes how the code is built and why. For what the app does from a user's point of view, see [FEATURES.md](FEATURES.md); for builds and releases, see [RELEASING.md](RELEASING.md).

---

## 1. Design rules

- **One data format.** Every connector returns Arrow `RecordBatch` streams. The grid, outputs, DuckDB joins, export, charts, job loads and the AI's result tools work the same for every engine.
- **Facts about a database type live in one place.** Quoting, name levels, statement syntax, row limits and load rules are a `SourceTraits` constant per engine (section 5.2). Shared code reads those fields instead of matching on the engine.
- **The AI goes through the same engine as the user.** AI tools call the query engine, the result store and the editor bridge. Every call passes the connection's AI policy first (section 13.4), and the AI never talks to a database directly.
- **Secrets stay out of the workspace file.** Passwords, keys and tokens are in the OS keychain or a local encrypted vault (section 6). The SQLite workspace holds only a flag that a secret exists.
- **Thin Tauri layer.** The engine crates have no Tauri dependency. `crates/app` turns Tauri commands into calls on plain async functions (`crates/app/src/api`), which tests call without a webview.

---

## 2. Processes

| Process | Source | Role |
|---|---|---|
| `databrain` | `crates/app/src/main.rs` | The Tauri app: native window with the React UI, all Rust crates in-process. Also the MCP server and the jobs runner: `databrain mcp` and `databrain jobs …` (`crates/app/src/cli/`), so the engine ships once, not three times |
| `databrain-mcp` | `crates/app/src/bin/databrain-mcp.rs` | Launcher (std only, a few hundred KB): runs `databrain mcp`, the MCP server over stdio for external agents (section 14), which opens the same workspace and secret store. MCP clients are set up with this path |
| `databrain-jobs` | `crates/app/src/bin/databrain-jobs.rs` | Launcher: runs `databrain jobs …`. `tick` runs due jobs and exits (started every minute by launchd, a systemd user timer or Task Scheduler when background jobs are on); `run <job>` and `list` for the terminal |
| `databrain-oracle-agent` | `crates/connectors/oracle/agent` (Go) | Thin Oracle driver, one per Oracle session, spoken to over stdin/stdout (section 5.3) |
| `kiro-cli acp` | installed by the user | Started per turn when the AI provider is Kiro (section 13.7) |

---

## 3. Technology

| Concern | Choice | Notes |
|---|---|---|
| App shell | Tauri 2 (2.11) | Plugins: dialog, opener, clipboard-manager, updater |
| Frontend | React 18, TypeScript, Vite 7, Tailwind 4 | Own UI primitives in `ui/src/components/ui.tsx`, icons from `lucide-react` |
| SQL editor | CodeMirror 6 (`@codemirror/lang-sql`) | Shared setup in `ui/src/components/sqlEditorSetup.ts` |
| Result grid | Glide Data Grid | Canvas, virtualized |
| UI state | zustand | `ui/src/store.ts` re-exports the slices in `ui/src/store/` |
| SQL formatting | `sql-formatter` (UI) | Language per engine from `SourceTraits.formatter` |
| Async runtime | tokio | Cancellation with `tokio_util::sync::CancellationToken` |
| Data format | Apache Arrow (`arrow` 59) | DuckDB's Arrow 58 batches are converted over Arrow IPC |
| Result views | Arrow compute kernels | Filter, sort, find, stats in `crates/result-store` |
| SQL over results | DuckDB (`results.<name>`), in-memory SQLite (AI `query_result`) | Section 8 |
| Workspace store | SQLite through `rusqlite` (bundled) | `PRAGMA user_version` migrations |
| Knowledge search | SQLite FTS5 (BM25) | No embeddings |
| SQL parsing | `sqlparser` 0.63 | Statement classification, table access checks, lineage |
| Secrets | `keyring` 3 or an AES-256-GCM vault file | `crates/auth/src/vault.rs` |
| JWT signing | `aws-lc-rs` (RS256) | `crates/auth/src/jwt.rs` |
| HTTP | `reqwest` | Cloud warehouses, ClickHouse, AI providers, OAuth |
| LLM streaming | Hand-written SSE decoder (`crates/ai/src/sse.rs`) | No vendor SDKs |
| MCP | Hand-written JSON-RPC 2.0 (`crates/ai/src/mcp.rs`) | No MCP SDK |
| SSH | `russh` | `crates/ssh-tunnel` |
| Rust to TypeScript types | `ts-rs` | Generated into `ui/src/lib/bindings` |
| Logging | `tracing`, `tracing-appender` | Daily rolling file in app data |

The workspace sets `rust-version = "1.89"` and `edition = "2024"` in `Cargo.toml`. `unsafe_code` is denied in every crate; the one exception is the Windows console attach for `databrain jobs` (`cli/jobs.rs` and the `databrain-jobs` launcher).

---

## 4. Crates

```
crates/
  app/              Tauri host: commands, events, job runner, scheduler, updates;
                    binaries databrain, databrain-mcp, databrain-jobs
  query-engine/     sessions, runs, cancel, safety prompts, outputs, loads, plans,
                    grid edits, schema compare, connection tests
  connector-core/   Connector/Session traits, SourceTraits, statement splitting
                    and classification, Arrow helpers, external tables
  connectors/       sqlite, postgres (and Redshift), mysql, mssql, oracle,
                    cloud (Snowflake, Databricks, BigQuery), duckdb, mongodb,
                    neo4j, clickhouse
  duckdb-dynload/   DuckDB C API loaded at runtime (builds without bundled DuckDB)
  result-store/     in-memory Arrow results, views, find, stats, charts
  export/           file writers and the SQL used by load steps
  workspace/        SQLite store: connections, history, notebooks, jobs, AI,
                    knowledge, explorer cache, outputs
  lineage/          column lineage of SQL scripts
  ai/               providers, agent, tools, policy, table access, knowledge,
                    MCP server, Kiro, inline completion
  auth/             credential sources, OAuth, JWT, cloud CLI reuse, secret stores
  bench/            timings of the critical paths (docs/PERFORMANCE.md); not in the app
  ssh-tunnel/       SSH local forwarding with host-key pinning
ui/                 React app
```

The AI code is one crate with modules, not separate crates:

| Module (`crates/ai/src`) | Content |
|---|---|
| `providers.rs`, `sse.rs`, `types.rs` | `LlmProvider` trait, adapters, SSE decoding, message and event types |
| `agent.rs` | Agent loop, prompts, history, retrieval per turn |
| `tools.rs` | Tool specs, `ToolContext` dispatch, approval gate, audit |
| `policy.rs`, `access.rs`, `pii.rs` | SQL review, table access rules, PII masking |
| `knowledge.rs` | Indexing and retrieval |
| `resultsql.rs` | SQL over one stored result in in-memory SQLite |
| `jobs.rs` | Job tools: schemas, build, merge, describe |
| `mcp.rs` | MCP server over stdio |
| `kiro.rs` | Kiro provider over ACP, per-turn MCP endpoint |
| `ghost.rs` | Inline completion in the editor |

---

## 5. Connector layer

### 5.1 Traits

`crates/connector-core/src/lib.rs`, abbreviated:

```rust
#[async_trait]
pub trait Connector: Send + Sync {
    fn info(&self) -> ConnectorInfo;                       // form fields, auth methods, traits
    fn kind(&self) -> ConnectorKind { ... }
    fn validate(&self, cfg: &ConnectionConfig) -> Result<(), String> { Ok(()) }
    fn auth_context(&self, cfg: &ConnectionConfig) -> AuthContext { ... }  // OAuth endpoints, CLI hints
    async fn connect(&self, cfg: &ConnectionConfig, creds: Arc<dyn CredentialSource>)
        -> Result<Box<dyn Session>>;
}

#[async_trait]
pub trait Session: Send + Sync {
    fn kind(&self) -> ConnectorKind;
    async fn server_version(&self) -> Result<String>;
    async fn ping(&self) -> Result<()>;
    async fn execute(&self, stmt: Statement<'_>, opts: ExecOptions) -> Result<QueryStream>;
    async fn list_schemas(&self) -> Result<Vec<SchemaInfo>>;
    async fn list_objects(&self, schema: &str) -> Result<Vec<DbObject>>;
    async fn describe(&self, schema: &str, name: &str) -> Result<ObjectDetail>;
    // With defaults: transaction_state, in_transaction, list_catalogs, list_catalog_schemas,
    // explorer_fingerprints, object_versions, list_objects_named, object_ddl,
    // search_objects, search_schema, search_routines, package_members,
    // bulk_metadata, schema_object_counts, check_sql,
    // table_layout, schema_columns
}
```

- `ExecOptions` carries the batch size, a `CancellationToken` and `max_rows` (the row cap plus one, to detect truncation). Connectors stop producing and cancel on the server when the token fires.
- `QueryStream` yields `StreamEvent`s: `Schema`, `Batch`, `Notice`, a "downloading" marker for warehouses that hand results out in chunks, and a summary.
- Credentials are fetched from the `CredentialSource` right before connecting. Connectors don't keep them.
- The optional methods let large catalogs stay fast: fingerprints tell which cached schemas changed, `object_versions` lets a schema refresh table by table, and the `search_*` methods filter on the server so completion never lists a whole schema.
- Each trait method and its default, the value and stream contract and the SQL pipeline (split, parameters, classify, read-only, redact): [CONNECTOR_CORE.md](CONNECTOR_CORE.md).

### 5.2 Source traits: what a database type is like

Everything the rest of the app needs to know about a database type is declared once, in `crates/connector-core/src/traits.rs`, as a `SourceTraits` constant per `ConnectorKind`. It is served to the UI with each connector in `ConnectorInfo.traits`. Shared code reads these fields instead of matching on the kind:

| Field | Used by |
|---|---|
| `language` (`sql`, `cypher`, `mongo_json`), `parser` | statement splitter and read/write classifier (`sql.rs`, `language.rs`), lineage, Format and SQL completion (SQL only) |
| `quote`, `upper_folding`, `levels`, `whole_path_quotes`, `always_qualify_catalog`, `catalog_noun`, `table_folder`, `view_folder` | identifier quoting and qualified names (Rust `quote_ident`, UI `quoteIdent`/`qualifiedName`), explorer levels and wording, completion |
| `row_limit`, `preview` | AI row caps, "Select top 100 rows" |
| `explain` | EXPLAIN checks (`Session::check_sql`, lineage "Check with database"); `None` = no check |
| `cost_preview` | the editor's cost preview (`Session::estimate_cost`: BigQuery dry run, Snowflake and Databricks EXPLAIN) |
| `syntax` (quotes, comments, `$$`, `DELIMITER`, block ends like `/` or `GO`) | statement splitting and `:name` parameters |
| `load` (`None` = not a load target): types, batch limits, literals, staging, merge style, transactions | job load steps and SQL INSERT export (`crates/export/src/load.rs`) |
| `cloud`, `local`, `network`, `ssl_setting`, `dialect_name`, `name`, `formatter` | SSH rules, the quick network check, the connection form, AI prompts, messages |

The UI keeps a copy in `ui/src/lib/sourceTraits.json` for its tests and pure helpers; the app replaces it with the backend's at start. The Rust test `traits::tests::ui_copy_is_up_to_date` fails when the copy is stale (`UPDATE_SOURCE_TRAITS=1 cargo test -p databrain-connector-core traits` rewrites it).

### 5.3 Engines

`ConnectorKind` has 12 variants: `sqlite`, `postgres`, `mysql`, `mssql`, `oracle`, `snowflake`, `databricks`, `bigquery`, `duckdb`, `mongodb`, `neo4j`, `clickhouse`. Each connector crate is behind a Cargo feature of `crates/app`.

| Engine | Crate and protocol | Cancel |
|---|---|---|
| PostgreSQL, Amazon Redshift | `connectors/postgres`: `tokio-postgres`. The statement is prepared for column types, then rows come over the simple (text) protocol, so every type displays as `psql` shows it. Redshift is a flavor of this connector (`redshift.rs`) with its own catalog queries and IAM sign-in through the AWS CLI | `CancelToken` |
| MySQL / MariaDB | `connectors/mysql`: `mysql_async` 0.36, text protocol | `KILL QUERY` from a second connection |
| SQL Server | `connectors/mssql`: `tiberius` 0.12 with rustls (TLS 1.2/1.3 on every OS); SQL login, NTLM, Entra ID | drops the connection; the next statement reconnects |
| Oracle | `connectors/oracle`: thin driver by default, Instant Client optional (below) | `agent.cancel` / `break_execution` |
| Snowflake | `connectors/cloud/snowflake.rs`: the driver REST protocol (login request, query request, result chunks) | abort request |
| Databricks | `connectors/cloud/databricks.rs`: SQL Statement Execution API 2.0, chunks and external links | statement cancel |
| BigQuery | `connectors/cloud/bigquery.rs`: Jobs API, run inside a BigQuery session | job cancel |
| SQLite | `connectors/sqlite`: `rusqlite` (bundled) | interrupt |
| DuckDB | `connectors/duckdb`: `duckdb` crate, bundled or loaded from the system (section 18). Attached files become views in the `files` schema; Excel reading in `xlsx.rs` | interrupt |
| MongoDB | `connectors/mongodb`: official `mongodb` driver; `shell.rs` parses mongosh-style calls and JSON commands. Documents become rows, nested values JSON text | |
| Neo4j | `connectors/neo4j`: `neo4rs` (Bolt), Cypher | |
| ClickHouse | `connectors/clickhouse`: HTTP interface over `reqwest`, one `session_id` per tab, `TabSeparatedWithNamesAndTypes` streamed line by line | `KILL QUERY ... ASYNC` |

Notes that apply across engines:

- Native types map to Arrow. Types Arrow can't hold become `Utf8`, and the database's type name is kept in field metadata (the grid shows it on hover). Exact decimals (Postgres `numeric`, MySQL `DECIMAL`) stay text so no precision is lost; filters and sorts treat them as numbers, and they are cast back to `DECIMAL` when DuckDB reads them as outputs.
- The cloud connectors call the vendors' HTTP APIs directly. No ADBC or native driver is bundled.
- **Oracle drivers.** The default is a thin driver: `databrain-oracle-agent`, a small Go program using go-ora (`crates/connectors/oracle/agent`). `agent.rs` downloads it on first use into `<app data>/oracle-agent/`, checks it against the SHA-256 sums compiled in from `agent/SHA256SUMS`, and starts one per session. A file downloaded by hand is accepted only if it matches the same sums. The "Oracle Instant Client" driver uses the `oracle` crate (ODPI-C) on blocking threads, for features only the thick client has. `client.rs` installs Instant Client's Basic package after checking the SHA-256 Oracle publishes, records a SHA-256 manifest of the folder's libraries, and checks the libraries against it before they are loaded into the process.

### 5.4 Adding a database type

The full checklist, feature by feature, with the current support matrix, is in [ADDING_A_CONNECTOR.md](ADDING_A_CONNECTOR.md). In short:

1. Add the variant to `ConnectorKind` (`types.rs`: enum, `ALL`, `as_str`) and its `SourceTraits` in `traits.rs`, starting from `traits::STANDARD` and overriding what differs. A non-SQL source sets `language` and `load: None`: lineage, Format, SQL completion and "load into" turn off for it on their own. A new query language also needs a `classify` arm in `language.rs` (what reads and what writes).
2. Write the connector crate under `crates/connectors/<name>`: `impl Connector` (`info()` with the form fields and auth methods, `validate()` for required settings, `connect()`) and `impl Session` (`execute` returning Arrow batches, `list_schemas`, `list_objects`, `describe`; `list_catalogs` for three-level sources; the rest have defaults). Results are Arrow, so the grid, outputs, DuckDB joins over `results.<name>`, export, charts and the AI's result tools work unchanged; nested values go in as JSON text columns.
3. Register it in the app: add it to `registry_with_outputs` in `crates/app/src/api/mod.rs` behind a new Cargo feature in `crates/app/Cargo.toml`, and add the crate to the workspace members.
4. Run `UPDATE_SOURCE_TRAITS=1 cargo test -p databrain-connector-core traits`, then `cargo test --workspace`. The test run also regenerates the UI's `ConnectorKind` type in `ui/src/lib/bindings`; commit it.

Connector-specific UI (a driver panel, sign-in hints) is optional. Without it, the generic connection form is built from `info().fields`.

---

## 6. Authentication and secrets

`crates/auth` serves both database connections and AI providers.

- **`CredentialSource`** (`crates/auth/src/lib.rs`): `get()` returns a valid credential and refreshes it if needed. Defaults cover `invalidate()`, `sign_in()`, `sign_out()`, `status()` and `identity()`. The query engine keeps one source per connection, so token caches survive reconnects.
- **OAuth** (`oauth.rs`, `loopback.rs`): authorization code with PKCE S256 through a single-use loopback listener on `127.0.0.1`, the device code flow, client credentials, and refresh with a margin before expiry. Used for Snowflake OAuth, Databricks U2M, Google, Entra ID and OpenRouter. Snowflake's `externalbrowser` SAML SSO goes through the Snowflake login protocol. Browser OAuth for Google, Snowflake and Entra ID needs the user's own OAuth client ID; Databricks uses the `databricks-cli` public client.
- **Key-pair JWT** (`jwt.rs`): RS256 signed with `aws-lc-rs`, for Snowflake key pairs and Google service accounts. Encrypted PKCS#8 keys are supported. The `rsa` crate is avoided because of RUSTSEC-2023-0071.
- **Cloud CLI reuse** (`cloud.rs`): gcloud application default credentials, service account files, `~/.databrickscfg` and `databricks auth token`, the AWS CLI for Redshift IAM, and `az` for SQL Server and Azure OpenAI. DataBrain only reads these files and never changes them.
- **Terminal sign-ins** (`terminal.rs`): runs commands such as `kiro-cli login` in the terminal the user chose, and detects terminals that don't start the command.
- **Secret stores** (`vault.rs`): `KeychainStore` (macOS Keychain, Windows Credential Manager, Secret Service through `keyring`) or `VaultStore` (AES-256-GCM, `vault.json` plus an owner-only `vault.key` in app data). `SwitchableStore` selects one from the `credential_store` setting (keychain by default) and moves secrets when the user switches in Settings.
- **SSH** (`crates/ssh-tunnel`): local forwarding on `127.0.0.1:<random>` through `direct-tcpip` channels. A server with no pinned key is refused before authenticating (`TunnelError::UnknownHostKey`, code `ssh_host_key_unknown`); the app shows the fingerprint and `trust_ssh_host_key` pins it once the user trusts it. A changed key fails the connection. Imports drop pinned fingerprints.

---

## 7. Query execution flow

```
UI (api.ts: run_query) ─▶ app command ─▶ QueryEngine::run(RunRequest)
   ├─ plan: split statements and classify them (connector-core sql.rs, per dialect)
   ├─ bind :name parameters
   ├─ read-only connection: only reads and harmless USE/SET
   ├─ safety_reasons: write without WHERE; on a prod connection anything but reads,
   │    transaction control and harmless USE/SET (admin statements included)
   │    └─ not confirmed → RunResponse::NeedsConfirmation (UI asks, runs again confirmed)
   ├─ one run per tab (or session_key) at a time
   └─ spawn: session.execute → batches → ResultStore + OutputRegistry
returns RunResponse::Started { job_id, statements }
events "job-event": StatementStarted, Progress, Downloading, StatementFinished,
                    StatementFailed, JobFinished, OutputsChanged
UI: fetch_page(result_id, view, offset, limit ≤ 5,000) → rows as display strings
```

- `RunRequest` names the connection, tab, SQL, row limit, origin (`user`, `ai`, `mcp`, `job`), an optional output name and an optional `session_key`. Notebook cells use `session_key = nb:{notebook}`, so all cells share one database session and temp tables carry across cells.
- Sessions are per tab (`sessions.rs`). They support manual commit with Commit and Rollback, are closed after an idle timeout, and are reopened on demand. Opening one is bounded by the connect timeout (the connection's `connect_timeout_secs` option, else the setting; idle closing likewise reads `idle_disconnect_minutes` per connection, `0` = never; `SESSION_OPTIONS` are left out of the cache identity). It covers the tunnel, the credential read and the handshake, not browser sign-in or local files. `Connector::prepare` does one-time local setup first, outside that limit (the Oracle thin driver's download). AI and MCP use their own sessions; DuckDB sessions for AI and MCP run sandboxed (`enable_external_access = false`, `lock_configuration = true`) unless the connection allows external access.
- `run_and_wait` is the same run for the AI, MCP and job steps. Its outcome comes on a channel of its own, so a lagging event subscriber can't lose the end of a run.
- `connect.rs` resolves credentials, opens the SSH tunnel and applies the connect timeout. `probe.rs` does a DNS and TCP check first so a test tells "unknown host", "refused" and "no answer" apart.
- Every run is written to `query_history` with its origin.
- Sessions, the run loop, cancellation, transactions, outputs and which safety check lives where: [QUERY_ENGINE.md](QUERY_ENGINE.md).

---

## 8. Results and outputs

**Result store** (`crates/result-store`). Results stay in memory as Arrow batches. Views (column filters, quick filter, multi-column sort), find, column stats and selection summaries run on Arrow compute kernels (comparison, `ilike`, `cast`, `take`). `chart.rs` groups and aggregates chart data in Rust. `wide.rs` switches columns past 2 GiB to `LargeUtf8`/`LargeBinary`. Pages go to the UI as JSON arrays of display strings, at most 5,000 rows per request, which is enough for a virtualized window.

**Outputs** (`crates/query-engine/src/outputs/`: `registry.rs` with `OutputRegistry`, `duckdb.rs`; comparing in `compare.rs`):

- Every result set gets a handle `rN` (the counter persists), an optional name, and provenance: SQL, connection, row limit, whether it was truncated.
- A rerun of a named output keeps up to 5 versions (`name`, `name__1`, ...).
- The outputs of each tab's latest run, and results the user kept in the tab ("Keep": shown next to later runs' results), are never evicted. Older unpinned outputs are freed least recently used first above the results budget (a quarter of the computer's memory, 512 MB to 16 GB, or the size chosen in Settings → Queries); their metadata stays.
- Job steps' outputs (run key `job:<job>:<step>`) have a budget of their own, a quarter of the results budget (`OutputRegistry::job_budget`), taken out of the tabs' share (`tabs_budget`, the other three quarters), so the setting stays the ceiling. They are freed only to make room for each other, never for a tab's results, and a tab's results never make room for them. Where results spill, a step's result keeps at most half of what that budget has left after what the other steps hold and reserved, and the rest goes to disk: a big scheduled run doesn't push out what the user is looking at, and isn't cut for memory. Where the disk refuses (low, or a failed write) or results don't spill, it stays in memory up to the tabs' cap, as before. A step loading now reserves its share (`reserve_job_memory`, released when its output is registered), so steps running at once stay within the budget together. Only outputs the job budget can't free count against it: pinned ones, those of a job running in this process (`job_running`), and those read in the last minute; the latest outputs of jobs that aren't running are freed least recently used first (memory, and their files when the disk is low; `free_disk` frees the needing result's own kind first) when room is needed, and a step reading a freed one fails saying so (Resume runs its step again). When a run ends, the earlier versions of its steps' outputs (`results.<step>__1`…) are freed (`OutputRegistry::release_earlier`), but not the one still holding the step's name (its rerun failed) nor one read in the last minute; the latest stays for the steps that read it and for "View output".
- Pinned outputs are written as Parquet under `<app data>/outputs/` and recorded in the workspace `outputs` table; they load lazily after a restart. Each tab's latest outputs are also saved on quit (up to 64 MB each) so a reopened tab shows its last result.

**SQL over outputs.** The DuckDB connector attaches an in-memory `results` catalog (`ATTACH ':memory:' AS results`). An output is loaded into it when SQL references `results.<handle or name>`, and reloaded when the name points at new data. The "Results (DuckDB)" connection is created on first use; it joins outputs from different databases, runs result diffs (generated `ANTI JOIN` / `IS DISTINCT FROM` SQL, or `EXCEPT ALL` without keys) and backs job steps that combine connections.

**AI result analysis.** `crates/ai/src/resultsql.rs` copies one capped result into an in-memory SQLite table `result` for the `query_result` tool, so the model can aggregate rows locally. `query_outputs` runs DuckDB SQL over `results.<name>` instead.

---

## 9. Export and loads

`crates/export` writes CSV, TSV, JSON, NDJSON, Markdown, SQL INSERT (per dialect), Parquet (`parquet` with zstd) and XLSX (`rust_xlsxwriter` in constant-memory mode). Text formats use the grid's display formatting. Scope is the current view, all fetched rows, or a full export: the query runs again on its own session and every row is streamed to the file in the background, with `export-progress` events and Stop.

`crates/export/src/load.rs` builds `CREATE TABLE` from Arrow types and batched `INSERT` statements from the `load` part of `SourceTraits`. `crates/query-engine/src/load.rs` runs load steps: insert, delete then insert, replace, and update or merge by key through a staging table (`MERGE` where the engine has it, otherwise `UPDATE ... FROM` plus `INSERT ... WHERE NOT EXISTS`), in one transaction where the engine allows it, plus dry runs.

`crates/query-engine/src/testdata.rs` makes up rows for a table ("Generate test data…"): each column's database type picks a generator, foreign key columns take keys read from the parent tables on the metadata session, and the first primary key column counts up from the table's `MAX`. It writes nothing: `api::generate_test_data` stores the rows as an output, and the existing `load_result` / `dry_run_result_load` load them. With "Generate values with AI", `QueryEngine::test_data_ai_ask` describes the columns the AI may write (structure only: types, limits, enum values, unique constraints from the DDL and `table_layout`, and the CHECK constraints on those columns, never the raw DDL; never keys, foreign keys, UUIDs or bytes), `databrain_ai::testdata::sample` asks the model for JSON rows in batches (same per-connection AI policy as the assistant: AI on, schema metadata shared, table rules, allowed providers; CHECK constraints on PII columns are left out), and `generate_test_data_with` puts each value in its column only when it fits, making up the rest from a separate random stream so the seed still repeats the other columns. `api::generate_test_data` registers its request id for `ai_cancel` before reading the table, so Stop works throughout. Column patterns live in `testdata_pattern.rs`: `Pattern::parse` by the column's generator, wrapped as `Gen::Patterned` so made-up values follow it and AI values are checked against it; `default_pattern` gives name-based defaults, and `api::suggest_test_data_patterns` asks the AI (`databrain_ai::testdata::suggest_patterns`) and keeps those `test_data_valid_patterns` accepts.

---

## 10. Workspace store

`crates/workspace` is one SQLite file, `<app data>/workspace.db`, opened with `rusqlite`. `MIGRATIONS` in `lib.rs` is applied from `PRAGMA user_version`, one transaction per step (13 versions today). The file uses `foreign_keys` and `secure_delete`.

| Area | Tables |
|---|---|
| Core | `connections`, `folders`, `saved_queries`, `query_history`, `tabs`, `settings`, `maintenance` |
| Explorer cache | `meta_catalogs`, `meta_schemas`, `meta_objects`, `meta_columns`, `meta_conn_state` |
| AI | `ai_providers`, `ai_sessions`, `ai_messages`, `ai_audit` |
| Knowledge | `kn_objects`, `kn_notes`, `kn_state`, `kn_schema_state`, FTS5 `kn_fts`; learned from history: `kn_joins`, `kn_join_seen`, `kn_table_seen`, `kn_learn_state` |
| Notebooks, jobs, outputs | `notebooks`, `jobs`, `job_runs` (with run logs and leases), `outputs` |

How the explorer cache is filled, checked against the server and read by other features: [EXPLORER.md](EXPLORER.md).

`connections.config_json` holds settings, and `has_secret` says whether the secret store has a password for it. Passwords inside connection strings are moved to the secret store, and passwords and keys are redacted from kept SQL, job logs, AI transcripts and the audit log.

---

## 11. Lineage

`crates/lineage` parses SQL with `sqlparser` in the connection's dialect and traces columns through CTEs, subqueries, joins, set operations and multi-statement scripts (`CREATE TABLE AS`, `INSERT ... SELECT`, `UPDATE ... FROM`, `MERGE`, ...). Table columns come from the explorer cache or a describe. With "Check with database", the app also reads the DDL of views the SQL reads (`CatalogTable::view_sql`), in rounds so views of views follow, and the crate traces a view's definition into the view's columns; that SQL gets no editor spans. Names it can't place become warnings instead of guesses. Table and view nodes keep every known column (for `*` and name resolution) and mark the ones the SQL uses (`Column::used`): the columns it names, then, walking edges back from what statements return or write, the columns that feed them, plus join and filter inputs; the diagram lists only those. The UI lays out the diagram and its exports itself (`ui/src/lib/lineageLayout.ts`, `lineageExport.ts`).

`join_pairs` uses the same builder to list the equi-join keys between database tables (`ON`, `USING`, `WHERE` links, `UPDATE … FROM`, `DELETE … USING`, `MERGE … ON`), tracing each side through CTEs and subqueries along `Direct` edges only; computed, ambiguous or unknown columns, `OR` and non-equi conditions and tables the script creates are left out. `fingerprint` hashes a query's shape (literals as `?`, FNV-1a) to tell a repeated query from a different one. Both feed learning from history (13.5).

---

## 12. Jobs

A job is a graph of steps stored in the `jobs` table. Each step runs a query, loads its upstream rows into a connection, or saves them to a file through DuckDB `COPY`. `crates/app/src/jobs.rs` runs the graph: independent branches run at the same time, a failure skips the steps after it, and each run keeps a log of up to 5,000 lines. Step results are outputs named after the step (`results.<step>`).

Run limits (`RunLimits` on the job, `StepLimits` on a step, `crates/workspace/src/jobs/limits.rs`): `execute` starts a ready step only while fewer than `max_parallel` steps run and fewer than `max_per_connection` run on its connection (a query step's connection, a load's target; the Results DuckDB is not counted per connection); the others are recorded as `waiting`. `run_step` runs each try in `one_try`, which cancels a child token when the step's time limit passes and fails with kind `timeout`. A failure of kind `connection` is tried again after `retry_delay` (30 s × 4ⁿ, at most 30 min) only when `retry_refused` finds nothing that could write twice: every statement classified `Read` with no unknown function (`reads_only`), or a load that reported `LoadEvent::Failed { kept_nothing: true }` (one transaction, rolled back before any commit was sent, no before/after SQL that may commit) or failed before writing. The run's time limit cancels the run token; after any cancel, steps get `STOP_GRACE` (30 s) to end before they are aborted, so a statement the server won't let go of can't keep the run and its lease alive.

Steps whose SQL the AI assistant or an MCP client wrote carry `JobNode::ai_sql` (set by `create_job` and `update_job` on steps whose effect they change — `JobNode::same_effect` compares `JobNode::same_sql` (the action, SQL, connections, before/after SQL; trimmed, an empty one as none) and a load's table, mode, keys (names exactly: `orders` and `"Orders"` differ on Postgres), "create", and an export's folder, file and format; a load or export without SQL also when the steps it runs after change. A user save keeps it while `JobNode::same_sql` holds, marks a copy of an AI step ("Duplicate") as the AI's, may clear it, never set it. Saves carry the version they were based on (`updated_at`) and are refused as `conflict` on an older one; the editor rebases the user's edits on the newer version (`rebaseJob`). A rename (`renames_of`) follows a step of the job to a valid step name, once and not onto another renamed step; the steps whose SQL it rewrites become the AI's. Names the user's tabs, notebooks or other jobs use are refused (`names_used_elsewhere`). A step that reads rows the AI's SQL selected (`results.<name>` of an AI output, on Results or any DuckDB connection, or a load/export's default upstream rows) runs with `Origin::Ai` too. Jobs saved before the flag existed are marked once at workspace open (maintenance `ai_jobs_marked_v1`): every step of a job whose name an approved `create_job` or `update_job` audit entry records. Such steps, every step of a run the assistant starts, and every step of a job that runs after such a run succeeds ("run after", carried as `JobRun::ai` through the queue of starts that had to wait, `job_after_pending`, which keeps a user's start and an AI one apart) run with `Origin::Ai`, so the DuckDB sandbox applies on the schedule and by hand too, not only when the assistant started the run. Load steps pass the step's origin in `LoadSpec`, so their before/after SQL on a DuckDB target is sandboxed as well when the step is the AI's or the run is under the AI rules. `run_job`'s approval reviews the jobs that would run after it (`crate::jobs::follower_problems` per visible follower, step by step) and lists what they change (`step_writes`: loads, exports, and queries the classifier doesn't call a plain read); a job started after the assistant's run has nobody to ask, so the runner (`ai_rules_now`, `Tracer::unattended_ai`) refuses its steps that need the user's approval (`review_step`'s confirm items and `copy_confirms`) rather than dropping them, and its other steps run. A workspace a development build upgraded to an earlier draft of v22 is brought to the final one at open (`repair_v22`).

Loads (`crates/query-engine/src/load.rs`) write rows as batched INSERTs, with a faster path unless the step is "INSERTs only" (`LoadSpec::insert_only`): on Postgres, `Session::bulk_load` streams the rows as `COPY … FROM STDIN` text (`databrain_export::load::PgCopy`) after the target's column types are read (behind a savepoint, so a failed lookup leaves the transaction usable for the INSERTs; a failure of the savepoint itself stops the load with its own error) and each column is checked to land as its INSERT literal would (`pg_copy_fits`, `copy_fits`); a table with rules or row-level security, or rows holding an identity-always or generated column, go by INSERTs, and so does Redshift (`ConnectionConfig::is_redshift` or the session's detected flavor), which has neither COPY from the client nor SAVEPOINT. `bulk_load` takes the load's cancel token: Stop sends the server a cancel for the COPY's start (its lock wait), each chunk and its end. The rows always pass through DataBrain: a load never runs its upstream query again on the target (`INSERT … SELECT`), which would read the table after the load's own DELETE/DROP and before SQL, and load rows other than the step's.

The mark follows what the assistant chose, not only its text:
- **Copies:** a user save keeps the mark on a new step whose SQL and connections equal one of the assistant's steps (`api::save_job`: "Duplicate" keeps it the assistant's).
- **Links:** a load or export without SQL takes its first upstream's rows, so `mark_ai_sql` treats an AI change of its links (which steps, in which order) like a change of its SQL, and `changes()` lists every link change in the approval.
- **Rows:** at run time, a step on Results whose SQL (or default `select * from results.<upstream>`) reads an output made with `Origin::Ai` runs with `Origin::Ai` too (`run_node`): its write is re-checked with `ai_rules_now`, and its own output is the AI's, so this follows the rows down the job.
- **Names:** `databrain_ai::jobs::output_name_user` (also behind the editor's name check) refuses an AI step name that a query tab, notebook or other job already uses, so `results.<name>` the user reads can't become the assistant's rows.
- **Versions:** the editor's saves carry the job's `updated_at`; `api::save_job` refuses one made on an older version (kind `conflict`) and the editor puts its edits on top of the saved version (`rebaseJob`: a step whose text the user didn't touch keeps the saved text and mark). `update_job` reads the job again after the approval and refuses when it changed meanwhile. The workspace makes each save's `updated_at` larger than the last.

Parameter sets (`param_sets` table, `crates/workspace/src/params.rs`) are named `:name` values shared by tabs, notebooks and jobs. A job keeps `params` and `param_set` in its spec; `jobs::effective_params` merges the set's values, the job's, then a run's `RunParams` (UI "Run with…", `databrain-jobs run --set/--param`, a resume replaying the resumed run's), records them on the run (`job_runs.params_json`) and passes them in every step's `RunRequest.params`; load before/after SQL is substituted the same way.

The in-app scheduler runs due jobs while the app is open. With background jobs on, `crates/app/src/background.rs` installs an OS timer that starts `databrain-jobs tick` every minute. A lease in the workspace makes sure a job runs in only one process.

Schedules (`crates/workspace/src/jobs/schedule.rs`, `JobSchedule` in the job's spec, every new field with a serde default so older jobs load unchanged): every N minutes, times of day on weekdays, days of the month (`-1` = last day; a day a month lacks is skipped), or a 5-field cron expression (parsed there, no crate: lists, ranges, steps, month and day names, `@daily` and the like; as in cron, day-of-month AND day-of-week when either starts with `*`, OR when both are set; a cron no month can run is refused). `time_zone` is empty (the computer's), an IANA name (`chrono-tz`, already in the build through arrow), `UTC`, or a fixed offset. Wall-clock times resolve in that zone: a time skipped by a daylight-saving change runs at the first valid time after it, a repeated one at its first occurrence only; intervals count elapsed time, aligned to the zone's standard-time offset all year (`standard_offset_ms`, the smaller of its January and July offsets), so runs stay exactly the interval apart when clocks change. A cron whose days can be more than the 8 years `next_after` looks ahead apart (Feb 29 on a Sunday) is refused (`Cron::longest_wait`, over the 400-year calendar cycle). `Job::schedule_base` is never later than now. `JobSchedule::due(base, now)` applies `missed`: `once` (one run, counted from now), `skip` (a run only when a time falls in the last 10 minutes, one look, otherwise just counted from now), `each` (one run per missed time, the last 24, each marking `last_scheduled_at` at its own time so the next pass finds the next one; counted in a window growing back from now, so a 1-minute schedule missed for months stays cheap on every tick); `jobs::tick_as` uses it in the app and the background runner, which ticks again after its runs so `each` doesn't wait a minute between them, and waits for the runs its last pass started before it exits (`cli::jobs::background_tick`, at most `TICK_DEADLINE`, 55 minutes: then the runs still going are stopped, with the reason as their error). It reports the runs this process started (`JobsState::take_started`, by run id; never the app's), notifying each failed or stopped one (`tick_report`). A run the runner itself stops (lease lost, lease not renewed, the deadline) records why (`JobsState::stop_for`). A start that can't be recorded (`mark_job_scheduled` fails) isn't made. A scheduled start that fails gives back its slot and lease (`jobs::give_back`) only when it may work later and only its own: the slot by compare-and-set (`give_back_job_slot`: still our mark, our lease); not while a run of the job started in the process meanwhile holds them (`let_go`, also on the no-run paths), not when another process runs it ("busy"), and a job that can't start (no steps) keeps its slot used. A "run after" start that is busy or meets a workspace error is queued. A save with the schedule on refuses a bad time, zone or cron expression.

`after_jobs` lists jobs whose successful run as a whole (all steps, or a resume) starts this one, with trigger `after:<name>`; the start happens in the finished run's task before the job leaves `running`, so `databrain-jobs tick`/`run` (`jobs::wait_all`) wait for those runs too. A follower that is running (here or in the other process) when it should start is queued in `job_after_pending` and started by the next `tick_as` in either process once free (taken with one delete, so only one process starts it). `save_job` drops unknown ids, the job itself and repeats, and refuses a loop naming its jobs, reading the other jobs and writing in one `BEGIN IMMEDIATE` transaction (two processes can't each save half of a loop); `delete_job` takes the job out of other lists in one transaction too. A failed unattended run (schedule, background, after) whose failed steps use a connection with browser or device-code sign-in (`AuthMethod::is_interactive`) gets a log line saying so; the schedule panel warns about it ahead.

---

## 13. AI assistant

### 13.1 Providers

```rust
#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn kind(&self) -> ProviderKind;
    async fn list_models(&self) -> Result<Vec<ModelInfo>>;
    async fn chat_stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatEvent>>>;
    fn as_kiro(&self) -> Option<&KiroProvider> { None }       // Kiro runs whole turns
    async fn budget(&self) -> Result<Option<Budget>> { Ok(None) }
}
```

`ChatEvent` is `TextDelta`, `ToolCall`, `Usage` or `Done`. Three adapters cover every provider:

| Adapter | `ProviderKind` | Wire API | Auth |
|---|---|---|---|
| `OpenAi` | `openai`, `azure_openai`, `openrouter`, `ollama`, `lm_studio`, `openai_compatible` | `/chat/completions` with streaming usage; Azure deployments with an `api-version` | API key; Entra ID token from `az` for Azure; OpenRouter browser sign-in (PKCE, returns a user key); none for local servers |
| `Anthropic` | `anthropic` | Messages API | API key |
| `Gemini` | `gemini` | `streamGenerateContent?alt=sse` | API key or Google ADC token |
| `KiroProvider` | `kiro` | `kiro-cli acp` (section 13.7) | `kiro-cli login` or a `ksk_` API key |

`budget()` reports Kiro plan credits, OpenRouter credits and key limits, and DeepSeek's balance (an OpenAI-compatible base URL on `api.deepseek.com`). Each provider has a default model and an optional fast model for inline completion. API keys are in the secret store.

### 13.2 Agent loop

`Agent::run` in `crates/ai/src/agent.rs`:

```
check ai_enabled and allowed_providers
load or create the session; keep about the last 30 messages
add @mentioned outputs and extra connections to the user turn
knowledge::retrieve → compact DDL of up to 8 tables + matching notes
system prompt (dialect, rules) + tool specs
loop until no tool calls or max_steps (12, set by the app):
    provider.chat_stream → TextDelta / ToolCall / Usage to the UI ("ai-event")
    each ToolCall → ToolContext::call → policy gate → result (cut at 16,000 chars)
    past 200,000 characters of conversation, the oldest tool results are dropped from what is sent
```

- A connection's own settings hold wherever it is reached from: a tool's `connection` argument, the other connections chosen for a question, an output's source and a job's steps all check `ai_enabled`, the MCP share and `allowed_providers` (`ToolContext::provider_allowed`) of that connection, not only of the chat's.
- Jobs the assistant wrote are reviewed twice: when it creates, changes or runs them (`jobs::review`, with the user's approval), and by the job runner each time such a step runs (`jobs::review_step`), so a connection later tagged production, set read-only or closed to AI writes stops the step.

- Modes: `Chat`, `Generate`, `Edit`, `FixError`, `Explain`, `AnalyzeResult`, `Optimize`. A question about connection settings runs without tools.
- Approvals: the app emits an `approval_request` on `ai-event` and waits up to 10 minutes for `ai_respond`. The user can edit the SQL before approving; edited SQL is reviewed again.
- Each run has a `CancellationToken`. Stop cancels the model stream and any query the run started.
- Every gated tool call is written to `ai_audit` with its decision (`allowed`, `approved`, `denied`, `blocked`, `error`, `approved by MCP client`).
- Several conversations run at once, each with its own run, approvals and auto-approve state.

### 13.3 Tools

Defined once in `specs()` in `crates/ai/src/tools.rs`, with hand-written JSON Schemas. The same list goes to the providers, to MCP (without the editor tools) and to Kiro.

| Tool | Purpose | Gate |
|---|---|---|
| `search_schema`, `describe_table`, `list_tables` | Knowledge index lookups, optionally on another connection | table access rules |
| `find_join_path` | Shortest chains of joins between two tables over declared foreign keys and learned joins | `share_metadata`; tables the AI may not read are never on a path |
| `index_tables` | Add tables the index is missing (structure only) | approval |
| `get_sample_rows` | A few rows of a table | `share_sample_values` or `share_result_rows` |
| `run_query` | Run SQL on the current or another connection; reads capped at 1,000 rows | `review_sql` (13.4) |
| `query_result` | SQLite SQL over one stored result (`result` table), up to 200 rows back | results of this conversation, or `share_result_rows` of the source connection |
| `list_outputs`, `query_outputs` | List outputs; DuckDB SQL over `results.<name>` | the source connection's policy |
| `result_summary` | Columns, row count, column stats; rows (up to `max_rows_to_model`) only with `share_result_rows` | |
| `list_knowledge_notes`, `add_knowledge_note`, `update_knowledge_note` | Notes and glossary | saved as proposed unless `auto_approve_notes` |
| `add_knowledge_join` | A join written in a document or said by the user (tables and columns checked against the index, with its source); used by `find_join_path` and `describe_table` without being seen in queries | saved as proposed unless `auto_approve_notes`; a join the user rejected is not recorded |
| `list_connections` | Connections the assistant may use, plus Results (DuckDB) | |
| `list_jobs`, `get_job`, `create_job`, `update_job`, `run_job` | Read, create, change and run jobs (over MCP only `list_jobs` and `get_job`) | approval with a summary of what changes |
| `save_query` | Save to saved queries | approval |
| `get_editor`, `write_editor` | Read the editor; propose SQL as a diff (insert, replace selection, replace all, new tab) | the user accepts or rejects the diff |

Only tables allowed by the connection's table access rules reach the model, and columns listed in `pii_columns` are masked (`pii.rs` follows aliases, expressions and `SELECT *`, and views: a view's definition, kept in the explorer cache's `meta_views` with the view's version, is expanded by the lineage so a rule on `users.email` masks `v_users.contact`; the AI's query tools read the definitions of the views a result reads before masking it, again each time for databases that don't version views; a view whose definition isn't known, or a column a view gives that its definition doesn't explain, is masked).

### 13.4 Policy

`AiPolicy` in `crates/workspace/src/lib.rs` is stored per connection:

| Field | Default | Meaning |
|---|---|---|
| `ai_enabled` | true | AI may use the connection |
| `allowed_providers` | empty (any) | provider ids allowed; also for another connection's chat reaching this one |
| `share_metadata`, `share_sample_values`, `share_result_rows` | true, false, false | what may be sent to the model (`share_metadata` off: table names only, in the prompt and in `describe_table`) |
| `run_query` | `ask` | `never`, `ask`, or `auto_read` (reads run without asking) |
| `allow_write` | false | AI may propose writes (always with approval) |
| `max_rows_to_model` | 50 | rows sent to the model |
| `pii_columns` | empty | masked columns |
| `index_schemas`, `index_batch` | all, 25 | knowledge index scope and batch size |
| `mcp_enabled`, `mcp_client_approves` | false, false | shared with MCP; accept the MCP client's own approval |
| `auto_approve_notes` | false | AI notes saved without review |
| `read_allow`, `read_deny`, `write_allow`, `write_deny` | empty | table access as SQL LIKE patterns |
| `external_access` | false | DuckDB AI sessions may use files, network, extensions |

`policy::review_sql` splits and classifies the SQL (`Read`, `Session`, `Transaction`, `Dml`, `Ddl`, `Other`, `Unknown`) and returns a `Decision`:

- `Deny` when AI is off, `run_query` is `never`, or the SQL writes (or can't be classified) on a read-only connection, without `allow_write`, or on a production connection.
- `Ask` for allowed writes (with a warning when a write has no `WHERE`) and for session or transaction statements.
- `Allow` for reads with `auto_read`, otherwise `Ask`.
- `Confirm` when the engine has no server-side read-only mode and a "read" calls functions that are not known built-ins, on a connection where the AI may not write. Only the user in DataBrain can approve a `Confirm`.

Then `access::check_sql` finds every table the SQL reads and writes (joins, subqueries, CTEs, `INSERT ... SELECT` sources, DML and DDL targets; MongoDB collections including `$lookup` and `$out`) and denies any table outside the rules. While rules are set, SQL whose tables can't be determined, and `USE` or `SET search_path`, are refused. `cap_rows` wraps reads with the engine's row limit syntax. Auto-approve in the panel approves a conversation's `Ask` requests without asking; the connection rules still apply.

### 13.5 Knowledge

Server facts live once, in the explorer cache (`meta_*`); what the AI may use is the user's scope over them (`ai_scope` → `ai_tables` → the `ai_objects` view, `crates/workspace/src/ai_scope.rs`; see [EXPLORER.md](EXPLORER.md#what-the-ai-may-use)). AI code reads table facts only through `crates/ai/src/catalog.rs` (`AiTables`), which adds the access rules; a test fails if it reads them another way. `kn_notes` and the FTS5 table `kn_fts` (title weighted 5x) stay. `knowledge.rs`:

- **Reading columns** (an index run) reads metadata through `Session::bulk_metadata` into the explorer cache, and skips schemas whose `explorer_fingerprints()` value is unchanged since their last read; "Read all again" ignores them. Indexing a schema allows it for the AI.
- **Retrieval** runs BM25 over `kn_fts`, adds tables named by matching notes, then follows foreign keys and learned joins one hop from the top tables, and renders the result as compact DDL within a character budget. Only approved notes are used. There are no embeddings.
- **Notes** target one or more existing tables or columns, or none (glossary). Targets you type are checked against the explorer cache and the live connection; the AI's are checked against what it may use. Notes are exported and imported as `.databrain-notes.json`.
- **Learned from history** (`learn.rs`, `AiPolicy.learn_from_history`, on by default). The app checks every minute (`ai_api::learn_loop`) for connections with history after their cursor and runs passes of 500 entries. Each successful entry gives join keys (`databrain_lineage::join_pairs`, or MongoDB `$lookup` through the connector's analyzer) and the tables it reads; table names are resolved strictly (a bare name in several schemas only in the connection's default one, unknown names dropped). Sightings are stored per join (or table), query fingerprint and UTC day, so counts are distinct queries and days and a scheduled job counts once a day. Runs by the assistant itself (`ai`, `mcp` origin) are kept apart and never count; SQL the user runs from the editor counts whoever wrote it. A join reaches the AI when confirmed, or seen in 2 queries or on 2 days, and not when rejected, a declared FK, stale (a column gone from the index) or touching a table the AI may not read (`usable_joins`); then it is rendered as `JOIN (cols) = table(cols)  -- learned …` lines, used for one-hop expansion after foreign keys, and by `find_join_path`. Table usage scales BM25 scores by `1 + 0.2·ln(1 + queries)`. After an index run the connection's kept history is read again (idempotent). Knowledge → "Learned from query history" lists joins with why each is or isn't given to the AI, and confirms, rejects, shows the source queries, makes a note or forgets.

### 13.6 Inline completion

`ghost.rs` makes one call to the provider's fast model (temperature 0, at most 160 tokens, no tools) with the SQL around the cursor and a few relevant tables from the index, under the connection's AI policy. The UI side is `ui/src/components/ghostExt.ts`. Kiro is not used for it.

### 13.7 Kiro

Kiro has no public model API, so `kiro.rs` drives the local CLI. Each turn:

1. `McpHttp::start` opens a temporary MCP endpoint on `127.0.0.1` with a random port and a random bearer token. It is a small hand-written HTTP server: it refuses requests with an `Origin` header or a wrong `Host` header, compares the token in constant time, and stops when the turn ends. Its tools go through the same `ToolContext` and policy as the built-in agent.
2. DataBrain starts `kiro-cli acp --agent databrain-sql` (Agent Client Protocol, JSON-RPC over stdio). DataBrain writes the agent config `~/.kiro/agents/databrain-sql.json`, which limits Kiro's tools to `@databrain`. DataBrain offers Kiro no file system or terminal access.
3. It resumes the conversation's Kiro session with `session/load` (or starts one with `session/new`), sends `session/prompt`, and streams Kiro's updates to the UI. Stop sends `session/cancel`.

Auth is the `kiro-cli login` browser session (run in the user's terminal and detected with `kiro-cli whoami`), or a `ksk_` API key from the secret store passed to the child only as `KIRO_API_KEY`.

---

## 14. MCP server

`databrain-mcp` serves `McpServer` from `crates/ai/src/mcp.rs`: hand-written JSON-RPC 2.0, one message per line on stdin and stdout, protocol version `2025-06-18` (the client's version is echoed when it sends one). Methods: `initialize`, `ping`, `tools/list`, `tools/call`; notifications are ignored.

- Only connections with `mcp_enabled` (and AI on) are visible. Every tool takes a required `connection` argument, and tools carry MCP annotations (`readOnlyHint`, `destructiveHint`, `idempotentHint`).
- Requests are handled concurrently, so a tool call waiting for the user doesn't block the client's other messages, including the answer. `notifications/cancelled` stops the call it names: a question it waits on ends, a running query is cancelled on the server, and no response is sent for it.
- When the policy says `Ask` and the connection sets `mcp_client_approves`, the call runs on the client's own tool approval, without a question, and is audited as "approved by MCP client". This holds with any `run_query` setting, not only auto-read.
- Otherwise, when the policy says `Ask`, the server asks the user through the client with `elicitation/create` (the question and the SQL). Only an explicit accept with `approve = true` runs it. A call that carries `_meta.progressToken` gets a `notifications/progress` saying the question is waiting.
- The wait is 2 minutes (`DATABRAIN_MCP_ELICIT_SECS`, 5 s to 1 h). A timeout ends the call with a guide the agent passes on to the user (Edit connection → AI → "Accept the MCP client's approval") and marks the peer `unanswered`: later `Ask` decisions in that session are refused at once instead of waiting, since a client can declare elicitation and still not show prompts in its current mode.
- A client without elicitation support gets a refusal with the same guide.
- `Confirm` decisions are never approved over MCP.
- `create_job`, `update_job` and `run_job` are not offered over MCP (`tools::APP_ONLY_TOOLS`) and are refused if called: jobs are created, changed and run in DataBrain's assistant.
- Settings → MCP shows the client config (`ai_api::mcp_config`), which points at the `databrain-mcp` binary next to the app.

---

## 15. Frontend

```
ui/src/
  App.tsx, main.tsx
  components/            top-level views: Sidebar, EditorPane, SqlEditor, ResultsPanel,
                         AiPanel, JobTab, LineageTab, ErTab, PlanTab, DdlTab, CompareTab,
                         DashboardTab, ChartView, CommandPalette, dialogs, settings
    ai/                  chat, conversation tabs, sessions, knowledge, pickers
    results/             result view, toolbar, column menu, edit bars, export, find
    sidebar/             connections tree, catalog and object nodes, history, saved,
                         notebooks and jobs panels
    notebook/            notebook view, cells, cell editor, cell AI
    job/                 job canvas panels, step panel, run log, load/export fields
    connection/          connection form sections (general, auth, SSH, AI)
    ui.tsx               own primitives: Modal, Popover, MenuItem, ComboInput,
                         ListPicker, Toasts, ConfirmDialog, ...
    sqlEditorSetup.ts    CodeMirror setup shared by query tabs and notebook cells
    ghostExt.ts, paramsExt.ts, queryHintsExt.ts, sqlFold.ts, sqlAssist.ts
  store.ts → store/      zustand slices: app, ui, tabs, runs, connections, explorer,
                         library, params
  aiStore.ts, jobsStore.ts, exportsStore.ts
  lib/
    api.ts               the only place that calls invoke; typed commands, event helpers
    bindings/            TypeScript types generated from Rust by ts-rs
    sourceTraits.json    copy of the backend's SourceTraits
    ...                  pure helpers: completion, formatting, lineage layout, charts
```

- **Backend calls.** `ui/src/lib/api.ts` wraps `invoke` and turns errors into `EngineError { kind, message, code, position }`. Events arrive through `listen`: `job-event`, `job-run`, `job-run-log`, `ai-event`, `auth-event`, `export-progress`, `load-progress`, `knowledge-event`, `oracle-agent`, `oracle-install`, `update-event`.
- **Types.** Rust types that cross the boundary derive `ts_rs::TS` with `#[ts(export)]`. `.cargo/config.toml` sets `TS_RS_EXPORT_DIR` to `ui/src/lib/bindings`, so `cargo test` writes them; CI fails if they differ from what is committed.
- **No UI libraries for content.** Markdown (`Markdown.tsx`), charts (`ChartView.tsx`, SVG) and the lineage, ER and job graphs are drawn by the app's own code.
- **AI in the UI.** The AI can prepare changes but not apply them on its own: editor edits appear as diffs in the assistant panel, and queries appear as approval cards with the SQL, connection and classification. Inline AI (⌘I) sends its proposal to the panel as a diff.

---

## 16. Security summary

- Secrets are in the OS keychain or the encrypted vault, never in `workspace.db`, logs, exports or prompts. Kept SQL, job logs, AI transcripts and audit entries are redacted.
- Browser sign-in uses PKCE S256 and a single-use loopback listener bound to `127.0.0.1`.
- SSH host keys are pinned. The Oracle helper and Instant Client libraries are checked against SHA-256 sums before they run or load.
- AI SQL goes through statement classification, the connection's policy, table access rules, a row cap, read-only rules, and a DuckDB sandbox for AI sessions. Permissions are enforced by the policy code, not by the model.
- MCP is off per connection by default, stdio only, and can't approve `Confirm` decisions. Kiro's MCP endpoint is per turn, localhost only, and token protected.
- The Tauri window has a strict CSP (`script-src 'self'`, `connect-src` limited to IPC) and a small capability list (`crates/app/capabilities/main.json`).
- No telemetry. Provider requests go from the app straight to the provider.

---

## 17. Errors, logging, testing

- **Errors.** Connectors return `ConnectorError`; the engine returns `EngineError { kind, message, code, position }`, which the UI receives as is. `position` underlines the error in the editor.
- **Logging.** `tracing` with a daily rolling file in app data (`crates/app/src/logging.rs`). `databrain-mcp` also logs to stderr.
- **Rust tests.** `cargo test --workspace` runs unit tests in every crate, the app's API tests (`crates/app/src/api/tests.rs`), the query engine tests, and `crates/ai/tests/agent_e2e.rs`, which drives the agent against a mock OpenAI-compatible server, the MCP server over in-memory pipes, and the Kiro provider against a fake `kiro-cli` (`crates/ai/tests/fixtures/fake-kiro-cli`). Tests that need a database server are `#[ignore]` and read `DATABRAIN_*` variables; a missing variable fails them rather than skipping.
- **UI tests.** `npm run lint` (ESLint with the React hooks rules, `--max-warnings 0`). `npm test` runs two Vitest projects from `ui/vite.config.ts`: `unit` (`*.test.ts` in Node) and `dom` (`*.test.tsx` in jsdom with Testing Library). `npm run e2e` runs the Playwright smoke tests in `ui/e2e` in Chromium against a mocked Tauri backend (`ui/e2e/tauri-mock.js` answers commands from test data and lets tests push events): the explorer, the job canvas and the AI panel.
- **CI** (`.github/workflows/ci.yml`):
  - `test` on macOS, Linux and Windows: UI lint (Linux), Vitest, UI build, `cargo clippy --workspace --all-targets`, `cargo test --workspace`, and the bindings check (Linux).
  - `deny`: `cargo deny check` with `deny.toml` (advisories, licenses, sources).
  - `e2e`: the Playwright smoke tests.
  - `live`: the ignored tests of the Postgres, MySQL, MongoDB and ClickHouse connectors and the app, against GitHub Actions service containers.
- Snowflake, Databricks, BigQuery, SQL Server, Oracle, Neo4j and Redshift have live tests that run only with credentials set; CI doesn't run them.

---

## 18. Build and packaging

- **DuckDB.** The default build compiles DuckDB in (`duckdb` crate with the `bundled` feature; app feature `duckdb`). The extensions DataBrain uses (Excel, Delta, Iceberg, Avro, httpfs, ICU, AWS, Azure) are downloaded at build time into `crates/app/resources/duckdb-extensions/` and shipped in the bundle, so nothing is downloaded at runtime. The "no-duckdb" build (`npm run app:build:no-duckdb`, app feature `system-duckdb`) uses `crates/duckdb-dynload`, which resolves the DuckDB C API from the installed `libduckdb` with `libloading` at startup. Without the library the app still starts and reports the engine as missing.
- **Oracle helper.** `scripts/build-oracle-agent.mjs` builds the Go helper when Go is installed; releases publish the binaries whose SHA-256 sums are compiled into the app.
- **Updates.** `crates/app/src/updates.rs` checks GitHub releases. Builds signed with an updater key install updates in place through `tauri-plugin-updater` and `latest.json`; other builds link to the download.
- **Releases.** `.github/workflows/release.yml` builds macOS (Apple Silicon, with and without DuckDB, and Intel), Windows and Linux bundles into a draft GitHub release. See [RELEASING.md](RELEASING.md).
