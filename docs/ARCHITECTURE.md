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
| `databrain` | `crates/app/src/main.rs` | The Tauri app: native window with the React UI, all Rust crates in-process |
| `databrain-mcp` | `crates/app/src/bin/databrain-mcp.rs` | MCP server over stdio for external agents (section 14). Opens the same workspace and secret store |
| `databrain-jobs` | `crates/app/src/bin/databrain-jobs.rs` | `tick` runs due jobs and exits (started every minute by launchd, a systemd user timer or Task Scheduler when background jobs are on); `run <job>` and `list` for the terminal |
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

The workspace sets `rust-version = "1.87"` and `edition = "2024"` in `Cargo.toml`. `unsafe_code` is denied in every crate; the one exception is the Windows console attach in `databrain-jobs`.

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
    async fn execute(&self, sql: &str, opts: ExecOptions) -> Result<QueryStream>;
    async fn list_schemas(&self) -> Result<Vec<SchemaInfo>>;
    async fn list_objects(&self, schema: &str) -> Result<Vec<DbObject>>;
    async fn describe(&self, schema: &str, name: &str) -> Result<ObjectDetail>;
    // With defaults: in_transaction, list_catalogs, list_catalog_schemas,
    // explorer_fingerprints, object_versions, list_objects_named, object_ddl,
    // search_objects, search_schema, search_routines, package_members,
    // bulk_metadata, schema_fingerprints, schema_object_counts, check_sql,
    // table_layout, schema_columns
}
```

- `ExecOptions` carries the batch size, a `CancellationToken` and `max_rows` (the row cap plus one, to detect truncation). Connectors stop producing and cancel on the server when the token fires.
- `QueryStream` yields `StreamEvent`s: `Schema`, `Batch`, `Notice`, a "downloading" marker for warehouses that hand results out in chunks, and a summary.
- Credentials are fetched from the `CredentialSource` right before connecting. Connectors don't keep them.
- The optional methods let large catalogs stay fast: fingerprints tell which cached schemas changed, `object_versions` lets a schema refresh table by table, and the `search_*` methods filter on the server so completion never lists a whole schema.

### 5.2 Source traits: what a database type is like

Everything the rest of the app needs to know about a database type is declared once, in `crates/connector-core/src/traits.rs`, as a `SourceTraits` constant per `ConnectorKind`. It is served to the UI with each connector in `ConnectorInfo.traits`. Shared code reads these fields instead of matching on the kind:

| Field | Used by |
|---|---|
| `language` (`sql`, `cql`, `cypher`, `mongo_json`), `parser` | statement splitter and read/write classifier (`sql.rs`, `language.rs`), lineage, Format and SQL completion (SQL only) |
| `quote`, `upper_folding`, `levels`, `whole_path_quotes`, `always_qualify_catalog`, `catalog_noun`, `table_folder`, `view_folder` | identifier quoting and qualified names (Rust `quote_ident`, UI `quoteIdent`/`qualifiedName`), explorer levels and wording, completion |
| `row_limit`, `preview` | AI row caps, "Select top 100 rows" |
| `explain` | EXPLAIN checks (`Session::check_sql`, lineage "Check with database"); `None` = no check |
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
- **SSH** (`crates/ssh-tunnel`): local forwarding on `127.0.0.1:<random>` through `direct-tcpip` channels. The server key's SHA-256 fingerprint is pinned on first use, and a changed key fails the connection.

---

## 7. Query execution flow

```
UI (api.ts: run_query) ─▶ app command ─▶ QueryEngine::run(RunRequest)
   ├─ plan: split statements and classify them (connector-core sql.rs, per dialect)
   ├─ bind :name parameters
   ├─ read-only connection: only reads and harmless USE/SET
   ├─ safety_reasons: write without WHERE, writes on a prod connection
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

---

## 8. Results and outputs

**Result store** (`crates/result-store`). Results stay in memory as Arrow batches. Views (column filters, quick filter, multi-column sort), find, column stats and selection summaries run on Arrow compute kernels (comparison, `ilike`, `cast`, `take`). `chart.rs` groups and aggregates chart data in Rust. `wide.rs` switches columns past 2 GiB to `LargeUtf8`/`LargeBinary`. Pages go to the UI as JSON arrays of display strings, at most 5,000 rows per request, which is enough for a virtualized window.

**Outputs** (`crates/query-engine/src/outputs.rs`, `OutputRegistry`):

- Every result set gets a handle `rN` (the counter persists), an optional name, and provenance: SQL, connection, row limit, whether it was truncated.
- A rerun of a named output keeps up to 5 versions (`name`, `name__1`, ...).
- The outputs of each tab's latest run are never evicted. Older unpinned outputs are freed least recently used first above a 1 GB budget; their metadata stays.
- Pinned outputs are written as Parquet under `<app data>/outputs/` and recorded in the workspace `outputs` table; they load lazily after a restart. Each tab's latest outputs are also saved on quit (up to 64 MB each) so a reopened tab shows its last result.

**SQL over outputs.** The DuckDB connector attaches an in-memory `results` catalog (`ATTACH ':memory:' AS results`). An output is loaded into it when SQL references `results.<handle or name>`, and reloaded when the name points at new data. The "Results (DuckDB)" connection is created on first use; it joins outputs from different databases, runs result diffs (generated `ANTI JOIN` / `IS DISTINCT FROM` SQL, or `EXCEPT ALL` without keys) and backs job steps that combine connections.

**AI result analysis.** `crates/ai/src/resultsql.rs` copies one capped result into an in-memory SQLite table `result` for the `query_result` tool, so the model can aggregate rows locally. `query_outputs` runs DuckDB SQL over `results.<name>` instead.

---

## 9. Export and loads

`crates/export` writes CSV, TSV, JSON, NDJSON, Markdown, SQL INSERT (per dialect), Parquet (`parquet` with zstd) and XLSX (`rust_xlsxwriter` in constant-memory mode). Text formats use the grid's display formatting. Scope is the current view, all fetched rows, or a full export: the query runs again on its own session and every row is streamed to the file in the background, with `export-progress` events and Stop.

`crates/export/src/load.rs` builds `CREATE TABLE` from Arrow types and batched `INSERT` statements from the `load` part of `SourceTraits`. `crates/query-engine/src/load.rs` runs load steps: insert, delete then insert, replace, and update or merge by key through a staging table (`MERGE` where the engine has it, otherwise `UPDATE ... FROM` plus `INSERT ... WHERE NOT EXISTS`), in one transaction where the engine allows it, plus dry runs.

---

## 10. Workspace store

`crates/workspace` is one SQLite file, `<app data>/workspace.db`, opened with `rusqlite`. `MIGRATIONS` in `lib.rs` is applied from `PRAGMA user_version`, one transaction per step (13 versions today). The file uses `foreign_keys` and `secure_delete`.

| Area | Tables |
|---|---|
| Core | `connections`, `folders`, `saved_queries`, `query_history`, `tabs`, `settings`, `maintenance` |
| Explorer cache | `meta_catalogs`, `meta_schemas`, `meta_objects`, `meta_columns`, `meta_conn_state` |
| AI | `ai_providers`, `ai_sessions`, `ai_messages`, `ai_audit` |
| Knowledge | `kn_objects`, `kn_notes`, `kn_state`, `kn_schema_state`, FTS5 `kn_fts` |
| Notebooks, jobs, outputs | `notebooks`, `jobs`, `job_runs` (with run logs and leases), `outputs` |

`connections.config_json` holds settings, and `has_secret` says whether the secret store has a password for it. Passwords inside connection strings are moved to the secret store, and passwords and keys are redacted from kept SQL, job logs, AI transcripts and the audit log.

---

## 11. Lineage

`crates/lineage` parses SQL with `sqlparser` in the connection's dialect and traces columns through CTEs, subqueries, joins, set operations and multi-statement scripts (`CREATE TABLE AS`, `INSERT ... SELECT`, `UPDATE ... FROM`, `MERGE`, ...). Table columns come from the explorer cache or a describe. With "Check with database", the app also reads the DDL of views the SQL reads (`CatalogTable::view_sql`), in rounds so views of views follow, and the crate traces a view's definition into the view's columns; that SQL gets no editor spans. Names it can't place become warnings instead of guesses. The UI lays out the diagram and its exports itself (`ui/src/lib/lineageLayout.ts`, `lineageExport.ts`).

---

## 12. Jobs

A job is a graph of steps stored in the `jobs` table. Each step runs a query, loads its upstream rows into a connection, or saves them to a file through DuckDB `COPY`. `crates/app/src/jobs.rs` runs the graph: independent branches run at the same time, a failure skips the steps after it, and each run keeps a log of up to 5,000 lines. Step results are outputs named after the step (`results.<step>`).

Steps whose SQL the AI assistant or an MCP client wrote carry `JobNode::ai_sql` (set by `create_job` and `update_job`; a user save keeps it only while the SQL is unchanged, and may clear it but never set it). Jobs saved before the flag existed are marked once at workspace open (maintenance `ai_jobs_marked_v1`): every step of a job whose name an approved `create_job` or `update_job` audit entry records. Such steps, and every step of a run the assistant starts, run with `Origin::Ai`, so the DuckDB sandbox applies on the schedule and by hand too, not only when the assistant started the run. Load steps pass the origin in `LoadSpec`, so AI-written before/after SQL on a DuckDB target is sandboxed as well.

Parameter sets (`param_sets` table, `crates/workspace/src/params.rs`) are named `:name` values shared by tabs, notebooks and jobs. A job keeps `params` and `param_set` in its spec; `jobs::effective_params` merges the set's values, the job's, then a run's `RunParams` (UI "Run with…", `databrain-jobs run --set/--param`, a resume replaying the resumed run's), records them on the run (`job_runs.params_json`) and passes them in every step's `RunRequest.params`; load before/after SQL is substituted the same way.

The in-app scheduler runs due jobs while the app is open. With background jobs on, `crates/app/src/background.rs` installs an OS timer that starts `databrain-jobs tick` every minute. A lease in the workspace makes sure a job runs in only one process.

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
```

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
| `index_tables` | Add tables the index is missing (structure only) | approval |
| `get_sample_rows` | A few rows of a table | `share_sample_values` or `share_result_rows` |
| `run_query` | Run SQL on the current or another connection; reads capped at 1,000 rows | `review_sql` (13.4) |
| `query_result` | SQLite SQL over one stored result (`result` table), up to 200 rows back | results of this conversation, or `share_result_rows` of the source connection |
| `list_outputs`, `query_outputs` | List outputs; DuckDB SQL over `results.<name>` | the source connection's policy |
| `result_summary` | Columns, row count, column stats; rows (up to `max_rows_to_model`) only with `share_result_rows` | |
| `list_knowledge_notes`, `add_knowledge_note`, `update_knowledge_note` | Notes and glossary | saved as proposed unless `auto_approve_notes` |
| `list_connections` | Connections the assistant may use, plus Results (DuckDB) | |
| `list_jobs`, `get_job`, `create_job`, `update_job`, `run_job` | Read, create, change and run jobs | approval with a summary of what changes |
| `save_query` | Save to saved queries | approval |
| `get_editor`, `write_editor` | Read the editor; propose SQL as a diff (insert, replace selection, replace all, new tab) | the user accepts or rejects the diff |

Only tables allowed by the connection's table access rules reach the model, and columns listed in `pii_columns` are masked (`pii.rs` follows aliases, expressions and `SELECT *`).

### 13.4 Policy

`AiPolicy` in `crates/workspace/src/lib.rs` is stored per connection:

| Field | Default | Meaning |
|---|---|---|
| `ai_enabled` | true | AI may use the connection |
| `allowed_providers` | empty (any) | provider ids allowed |
| `share_metadata`, `share_sample_values`, `share_result_rows` | true, false, false | what may be sent to the model |
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

The knowledge index lives in the workspace (`kn_objects` with columns, keys and a content hash; `kn_notes`; the FTS5 table `kn_fts` with title weighted 5x). `knowledge.rs`:

- **Indexing** reads metadata through `Session::bulk_metadata`. It compares `schema_fingerprints()` with the stored ones and skips unchanged schemas; "Rebuild" ignores them. Objects whose content hash is unchanged are not rewritten. `index_schemas` limits the scope.
- **Retrieval** runs BM25 over `kn_fts`, adds tables named by matching notes, then follows foreign keys one hop from the top tables, and renders the result as compact DDL within a character budget. Only approved notes are used. There are no embeddings.
- **Notes** target one or more existing tables or columns, or none (glossary). Targets are checked against the index, the explorer cache and the live connection. Notes are exported and imported as `.databrain-notes.json`.

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
- When the policy says `Ask`, the server asks the user through the client with `elicitation/create` (the question and the SQL). Only an explicit accept with `approve = true` runs it. A call that carries `_meta.progressToken` gets a `notifications/progress` saying the question is waiting.
- The wait is 2 minutes (`DATABRAIN_MCP_ELICIT_SECS`, 5 s to 1 h). A timeout ends the call with a message naming the fixes (auto-read, or `mcp_client_approves`) and marks the peer `unanswered`: later `Ask` decisions in that session are refused at once instead of waiting, since a client can declare elicitation and still not show prompts in its current mode.
- A client without elicitation support gets a refusal, unless the connection sets `mcp_client_approves`; then the call runs and is audited as "approved by MCP client".
- `Confirm` decisions are never approved over MCP.
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
