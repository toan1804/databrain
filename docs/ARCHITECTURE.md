# DataBrain — Architecture

A modern desktop SQL client written in Rust with a built-in AI assistant.

- Connects to relational databases (PostgreSQL, MySQL, Oracle, SQL Server, SQLite, ...) and cloud warehouses (Databricks, Snowflake, BigQuery, ...).
- Users write, save, and run queries, and explore large results (find, filter, sort, export).
- AI mode connects to cloud providers (OpenAI, Anthropic, Gemini, Bedrock, ...) or local models (Ollama, LM Studio, ...), signed in with an API key or through the browser. The AI knows the database metadata, can write into the editor, run queries, and analyze results.

---

## 1. Goals and non-goals

Goals
- One consistent experience across many database engines.
- A fast result grid that stays smooth with millions of rows. Find, filter, sort, and export all run locally.
- A modern UI: command palette, tabs, dark/light themes, keyboard-first, minimal chrome (similar to TablePlus, Beekeeper Studio, Outerbase, or Cursor-style AI panels, not DBeaver).
- An AI assistant that uses real schema knowledge, acts through explicit tools, and always asks the user before it changes anything.
- Privacy is controlled per connection. Metadata, sample data, and result data are separate permissions, and a local-model option means nothing has to leave the machine.

Non-goals (v1)
- Visual ER designer, data migration tooling, DBA admin panels.
- Multi-user server or team sync (the design allows adding this later).
- Training or fine-tuning models.

---

## 2. Technology choices

| Concern | Choice | Why |
|---|---|---|
| App shell | **Tauri 2** | Rust backend with a web UI; small binary, native webview |
| Frontend | **React + TypeScript + Vite**, Tailwind + shadcn/ui | Modern component ecosystem |
| SQL editor | **Monaco** | Autocomplete, inline diff (for AI edits), ghost text |
| Result grid | **Glide Data Grid** (canvas, virtualized) | Handles millions of cells |
| Async runtime | **tokio** | |
| Data format | **Apache Arrow** (`arrow-rs`) | Shared by drivers, engine, export, and AI result analysis |
| Local query engine | **DataFusion** | Filter/sort/find/stats over results, including queries the AI runs |
| App store | **SQLite** via `sqlx` | Workspace, history, AI sessions, knowledge |
| Full-text search | SQLite **FTS5** | Keyword search over metadata and knowledge |
| Vector search | **sqlite-vec** (alternative: LanceDB) | Semantic retrieval over metadata; stays in the same DB file |
| Local embeddings | **fastembed-rs** (ONNX) | Embeds metadata without sending it anywhere |
| LLM HTTP / streaming | `reqwest` + `eventsource-stream` | Thin in-house provider adapters (no heavy SDK lock-in) |
| OAuth / OIDC | `oauth2` + `openidconnect` crates, `axum` loopback listener, `tauri-plugin-opener` | Browser sign-in (PKCE, device flow) for DB platforms and AI providers |
| Cloud identity | `aws-config`, `azure_identity`, `gcp_auth`, `jsonwebtoken` | Reuse cloud CLI profiles, service principals, key-pair JWT |
| MCP | `rmcp` (official Rust MCP SDK) | Exposes DataBrain tools to external agents (Kiro CLI, Claude Code, ...) |
| Secrets | `keyring` | OS keychain for DB passwords, API keys, OAuth tokens |
| SQL parsing | `sqlparser-rs` | Statement splitting, safety classification of AI-generated SQL |
| SSH / TLS | `russh` / `rustls` | |

---

## 3. High-level architecture

```
┌──────────────────────────────────────────────────────────────────────────────┐
│                           Frontend (React / TS)                              │
│ ┌─────────┐ ┌──────────────┐ ┌─────────────┐ ┌──────────────┐ ┌────────────┐ │
│ │Sidebar  │ │ SQL Editor   │ │ Result Grid │ │  AI Panel    │ │ Cmd+K      │ │
│ │conns,   │ │ Monaco,      │ │ Arrow pages,│ │ chat, tool   │ │ palette,   │ │
│ │schema,  │ │ inline AI    │ │ filter/find,│ │ cards, diff  │ │ settings   │ │
│ │saved q. │ │ edit + diff  │ │ "Analyze"   │ │ approvals    │ │            │ │
│ └─────────┘ └──────────────┘ └─────────────┘ └──────────────┘ └────────────┘ │
└──────────────▲─────────────────────────────────────────────┬─────────────────┘
               │ events / Channels (tokens, progress, Arrow) │ commands (invoke)
┌──────────────┴─────────────────────────────────────────────▼─────────────────┐
│                         app (Tauri host, Rust)                               │
├──────────────────────────────── AI layer ────────────────────────────────────┤
│ ┌────────────────┐ ┌──────────────────┐ ┌──────────────────┐ ┌─────────────┐ │
│ │ ai-agent       │ │ ai-providers     │ │ knowledge        │ │ mcp-server  │ │
│ │ agent loop,    │ │ OpenAI, Anthropic│ │ metadata indexer,│ │ (optional)  │ │
│ │ tools, policy, │ │ Gemini, Bedrock, │ │ notes/glossary,  │ │ exposes     │ │
│ │ context builder│ │ Azure, Kiro*,    │ │ FTS5 + vectors,  │ │ tools over  │ │
│ │ approvals      │ │ OpenRouter, local│ │ retrieval        │ │ stdio/http  │ │
│ └───────┬────────┘ └────────┬─────────┘ └────────┬─────────┘ └─────────────┘ │
│         │   ┌───────────────▼────────────────┐   │                           │
│         │   │ auth (shared: AI providers +   │   │                           │
│         │   │ DB connections): API key/PAT,  │   │                           │
│         │   │ key-pair JWT, OAuth PKCE,      │   │                           │
│         │   │ device code, SAML SSO, CLI     │   │                           │
│         │   │ profiles, token manager        │   │                           │
│         │   └────────────────────────────────┘   │                           │
├─────────┼──────────────────── Data layer ────────┼───────────────────────────┤
│ ┌───────▼───────┐ ┌──────────────┐ ┌──────────┐ ┌▼───────────┐               │
│ │ query-engine  │ │ result-store │ │ export   │ │ workspace  │               │
│ │ jobs, cancel, │ │ Arrow +      │ │ CSV/JSON/│ │ SQLite:    │               │
│ │ split, safety │ │ DataFusion   │ │ Parquet/ │ │ saved q.,  │               │
│ │               │ │ spill        │ │ XLSX/SQL │ │ history, AI│               │
│ └───────┬───────┘ └──────────────┘ └──────────┘ └────────────┘               │
│ ┌───────▼────────────────────────────────────────────────────────────────┐   │
│ │ connector-core: Connector/Session traits, type mapping, pool, SSH,     │   │
│ │ CredentialSource (from auth), metadata cache                           │   │
│ └───────┬────────────────────────────────────────────────────────────────┘   │
│  postgres · mysql · oracle · mssql · sqlite · duckdb · clickhouse ·          │
│  snowflake · databricks · bigquery · adbc                                    │
└──────────────────────────────────────────────────────────────────────────────┘
```

The AI layer never talks to databases directly. Every AI action goes through the same `query-engine`, `result-store`, and editor commands the user drives, and each one passes a policy check first.

---

## 4. Cargo workspace layout

```
databrain/
├─ Cargo.toml
├─ crates/
│  ├─ auth/                  # shared: API key/PAT, key-pair, OAuth PKCE, device code,
│  │                         # SSO, cloud CLI reuse, token manager (DB + AI)
│  ├─ connector-core/        # traits, Arrow type mapping, pool, SSH, metadata cache
│  ├─ connectors/            # postgres, mysql, oracle, mssql, sqlite, duckdb,
│  │                         # clickhouse, snowflake, databricks, bigquery, adbc
│  ├─ query-engine/          # jobs, cancellation, statement splitting, SQL safety classifier
│  ├─ result-store/          # Arrow store, DataFusion views, stats, spill
│  ├─ export/                # file writers
│  ├─ workspace/             # SQLite persistence + migrations
│  ├─ ai-core/               # message/tool types, LlmProvider trait, token accounting
│  ├─ ai-providers/          # provider adapters (feature-flagged)
│  ├─ ai-agent/              # agent loop, tool registry, policy engine, context builder
│  ├─ knowledge/             # metadata indexer, embeddings, hybrid retrieval
│  ├─ mcp-server/            # optional MCP exposure of DataBrain tools
│  └─ app/                   # Tauri binary
└─ ui/                       # React frontend
```

---

## 5. Connector layer

Every connector produces Arrow `RecordBatch` streams, so downstream code (grid, filter, export, AI analysis) works the same for every engine.

```rust
#[async_trait]
pub trait Connector: Send + Sync {
    fn kind(&self) -> ConnectorKind;
    fn dialect(&self) -> Box<dyn sqlparser::dialect::Dialect>;
    fn capabilities(&self) -> Capabilities;   // transactions, cancel, explain, read-only mode...
    fn auth_methods(&self) -> &'static [AuthMethodKind];  // drives the connection form (section 6)
    async fn connect(&self, cfg: &ConnectionConfig, creds: Arc<dyn CredentialSource>)
        -> Result<Box<dyn Session>>;
}

#[async_trait]
pub trait Session: Send + Sync {
    async fn execute(&self, sql: &str, opts: ExecOptions) -> Result<QueryStream>;
    async fn cancel(&self, h: &CancelHandle) -> Result<()>;
    async fn set_read_only(&self, on: bool) -> Result<()>;
    async fn list_catalogs(&self) -> Result<Vec<Catalog>>;
    async fn list_schemas(&self, catalog: Option<&str>) -> Result<Vec<Schema>>;
    async fn list_objects(&self, schema: &SchemaRef) -> Result<Vec<DbObject>>;
    async fn describe(&self, obj: &ObjectRef) -> Result<ObjectDetail>;  // columns, PK/FK, comments, DDL
    async fn explain(&self, sql: &str) -> Result<Plan>;
}
```

| Engine | Crate / protocol | Notes |
|---|---|---|
| PostgreSQL (and Redshift, CockroachDB) | `tokio-postgres` | Streaming `query_raw`, `CancelToken` |
| MySQL / MariaDB | `mysql_async` | Cancel with `KILL QUERY` from a side connection |
| Oracle | `oracle` (ODPI-C) | Sync API on `spawn_blocking`; needs Instant Client |
| SQL Server | `tiberius` | |
| SQLite / DuckDB | `rusqlite` / `duckdb` | DuckDB returns Arrow natively |
| Snowflake | SQL REST API (+ ADBC for browser SSO) | Arrow result chunks; auth in section 6 |
| Databricks | SQL Statement Execution API | `ARROW_STREAM` + `EXTERNAL_LINKS`; auth in section 6 |
| BigQuery | Jobs API + Storage Read API | Arrow |
| Generic | ADBC driver manager | Fallback / fast path |

Each connector is behind a Cargo feature flag. Native types map to Arrow. Types Arrow can't represent fall back to `Utf8`, and the original type name is kept in field metadata.

---

## 6. Authentication (cloud data platforms and AI providers)

A single `auth` crate handles authentication for database connections and AI providers. Every cloud platform supports at least two options: **browser sign-in** and **API key / token**. Service accounts are available for automation.

### 6.1 Auth methods

```rust
pub enum AuthMethod {
    Password        { user: String },                                   // secret in keychain
    ApiToken        { header: TokenPlacement },                         // API key, PAT, programmatic token
    KeyPairJwt      { user: String, key_ref: SecretRef, passphrase: Option<SecretRef> },
    OAuthBrowser    { oidc: OidcConfig, scopes: Vec<String>, redirect: RedirectMode }, // Auth Code + PKCE
    DeviceCode      { oidc: OidcConfig, scopes: Vec<String> },          // RFC 8628, fallback
    ClientCredentials { oidc: OidcConfig, client_id: String },          // service principal / M2M
    ExternalBrowserSso,                                                 // Snowflake SAML "externalbrowser"
    CloudCli        { kind: CloudCli, profile: Option<String> },        // aws / gcloud / az / databricks CLI
    ServiceAccountKey { key_ref: SecretRef },                           // e.g. GCP JSON key
    None,
}

pub struct OidcConfig {
    pub issuer_or_endpoints: IssuerOrEndpoints,  // discovery URL or explicit authorize/token URLs
    pub client_id: String,                       // app default or admin-provided
    pub client_secret: Option<SecretRef>,        // only for confidential clients
}

pub enum RedirectMode { LoopbackRandomPort, LoopbackFixedPort(u16) }
```

### 6.2 Credential source and token manager

Connectors and AI providers never hold static secrets. They ask for a credential when they need one:

```rust
#[async_trait]
pub trait CredentialSource: Send + Sync {
    /// Returns a valid credential, refreshing silently if needed.
    async fn get(&self) -> Result<Credential, AuthError>;
    /// Called by a connector on 401/expired-token errors; forces refresh or re-auth.
    async fn invalidate(&self);
    fn identity(&self) -> Option<Identity>;      // "alice@corp.com", shown in UI
}

pub enum Credential {
    Password { user: String, password: SecretString },
    Bearer   { token: SecretString, expires_at: Option<OffsetDateTime> },
    Jwt      { token: SecretString, expires_at: OffsetDateTime },  // e.g. Snowflake key-pair
    AwsSigV4(aws_credential_types::Credentials),
    Custom(Box<dyn Any + Send + Sync>),
}
```

`TokenManager` behavior:
- Refresh proactively, about 5 minutes before expiry, using the refresh token or by re-signing the JWT.
- Single-flight refresh: one mutex per auth profile, so parallel queries don't trigger a burst of refresh calls.
- On a 401 or expired error, call `invalidate()`, refresh, and retry the request once.
- If a silent refresh isn't possible (refresh token expired or revoked), emit `auth:reauth_required`. The UI shows "Sign in again" and pauses the job, which resumes after sign-in.
- Refresh tokens and long-lived secrets go in the OS keychain. Access tokens stay in memory, with an optional encrypted keychain cache so the user doesn't have to sign in again after every app restart.

### 6.3 Browser sign-in flow (OAuth 2.0 Authorization Code + PKCE)

```
User clicks "Sign in with browser"
  1. auth: generate code_verifier, S256 challenge, state, nonce (OIDC)
  2. auth: start loopback listener on 127.0.0.1:<port>  (random, or the fixed port the platform requires)
  3. open system browser (tauri-plugin-opener) → provider /authorize?...&redirect_uri=http://127.0.0.1:<port>/callback
  4. user signs in (SSO / MFA handled by IdP, not by us)
  5. browser → GET /callback?code&state → validate state → show "You can close this tab" page
  6. exchange code + verifier at /token → access (+ refresh, id_token)
  7. validate id_token (issuer, audience, nonce, signature via JWKS) when OIDC
  8. store refresh token in keychain, identity in auth_profiles, close listener
  Timeout: 5 min. Cancel button in UI aborts the listener.
```

- A loopback redirect with PKCE is the RFC 8252 recommendation for native apps. The listener accepts exactly one valid callback, is bound only to `127.0.0.1`, and rejects mismatched `state`.
- Device code fallback: shows a code plus verification URL and polls the token endpoint. Used when a loopback listener isn't possible (locked-down machines, remote desktop) or when a platform supports only this flow.
- Generic OIDC option: "Custom OAuth / OIDC" in the connection form, filled in with an issuer URL and client ID. This covers Okta, Entra ID, Keycloak, and Ping for Snowflake External OAuth and other enterprise setups.

### 6.4 Per-platform support matrix

| Platform | Browser sign-in | API key / token | Other |
|---|---|---|---|
| **Snowflake** | (a) `externalbrowser` SAML SSO via the IdP. (b) OAuth Auth Code + PKCE using a Snowflake OAuth security integration or External OAuth (Okta/Entra) | Programmatic Access Token (PAT) | Key-pair JWT (recommended for service users), password + MFA |
| **Databricks** | OAuth U2M (Auth Code + PKCE) against the workspace `/oidc/v1/authorize`; Entra ID on Azure Databricks | Personal Access Token | OAuth M2M (service principal client credentials); reuse `~/.databrickscfg` profiles |
| **BigQuery / GCP** | Google OAuth installed-app flow (loopback + PKCE) | (BigQuery doesn't accept plain API keys for data access) | Service account JSON key, reuse `gcloud` ADC |
| **Redshift / Aurora / RDS (IAM)** | AWS IAM Identity Center SSO (browser / device flow) | Access key + secret (not recommended) | Reuse `~/.aws` profiles; IAM auth tokens generated per connection |
| **Azure SQL / Synapse / Fabric** | Entra ID interactive browser | — | Device code, service principal, Azure CLI reuse; token passed to `tiberius` as an AAD token |
| **ClickHouse Cloud, MotherDuck, Neon, Supabase, PlanetScale** | Where the platform offers OAuth | API token / password | |
| **Oracle Autonomous DB** | OCI IAM token (phase 7) | — | Wallet (mTLS) + password |
| **AI: OpenAI, Anthropic** | Not supported for third-party API access (as of this writing; re-verify) | API key | |
| **AI: Gemini / Vertex, Bedrock, Azure OpenAI, OpenRouter** | Google OAuth / AWS SSO / Entra ID / OpenRouter OAuth PKCE | API key | Cloud CLI reuse |
| **AI: Kiro, ChatGPT/Claude subscriptions** | Only if the provider offers a third-party OAuth client and API and its terms allow it (verify) | — | MCP integration (section 15) |

Implementation notes:
- **Snowflake browser SSO:** the public SQL REST API accepts OAuth, key-pair JWT, and PAT, but not a SAML session. `externalbrowser` SSO therefore goes through the driver login protocol. The simplest route is the ADBC Snowflake driver, which supports `externalbrowser` and OAuth. PAT, key-pair, and OAuth can use the REST path directly.
- **Databricks:** the OAuth U2M client ID and redirect port must match an app connection registered in the Databricks account. Ship a DataBrain app registration where possible, and let admins override `client_id` and the port per connection.
- **OAuth client IDs in general:** enterprise IdPs usually need an admin to register DataBrain (or supply a client ID). The connection form has an "Advanced" section for client ID, scopes, issuer, and port. Don't borrow another vendor's client ID.
- **Cloud CLI reuse:** read-only use of existing profiles such as `aws sso login`, `gcloud auth application-default login`, `az login`, and `databricks auth login`. DataBrain never modifies those files. This is often the fastest path for developers.

### 6.5 Connection form UX

- The user picks a platform, then sees tabs for its supported methods (from `Connector::auth_methods()`), with the recommended method first: **Browser**, **Token / API key**, **Key pair**, **Service principal**, **Use CLI profile**.
- Browser tab: a "Sign in with <Platform>" button. After sign-in it shows the signed-in identity, the token expiry, and "Sign out" and "Switch account" buttons.
- Token tab: a masked input with paste support, plus a "Test" button that validates the token and shows who it belongs to (e.g. Databricks `/api/2.0/preview/scim/v2/Me`, Snowflake `SELECT CURRENT_USER()`).
- Key-pair tab: pick a PEM file or paste one; it is stored in the keychain. There is also a "Generate key pair" helper that shows the `ALTER USER ... SET RSA_PUBLIC_KEY` statement to run.
- One auth profile can be shared by several connections, e.g. one Databricks sign-in used for several SQL warehouses.

### 6.6 Security rules

- PKCE S256 always. OIDC flows use `state` and `nonce`, and ID tokens are validated against JWKS.
- The loopback listener is bound to `127.0.0.1`, serves a single use, and times out. It is never exposed on the network.
- Tokens and keys are held as `SecretString` (the `secrecy` crate) and zeroized on drop, never logged, and redacted from error messages and AI prompts.
- The app requests minimal scopes, e.g. `sql` plus `offline_access` for Databricks rather than `all-apis` when possible.
- "Sign out" deletes the keychain entries and calls the provider's revoke endpoint when it has one.

---

## 7. Query execution flow

```
UI or AI tool ─▶ query-engine.submit(sql, origin = User | Ai{session_id})
                 ├─ split statements (sqlparser)
                 ├─ classify: Read | Dml | Ddl | Admin | Unknown
                 ├─ policy: prod? read-only? origin=Ai? → allow / ask / deny
                 ├─ Job{id, CancellationToken}  → returns job_id
                 └─ spawn: session.execute → stream batches → ResultStore
events: job:progress (throttled), job:done {result_id, schema, rows}
UI: fetch_page(result_id, view, offset, limit) → Arrow IPC bytes via Channel
```

- Run modes: statement under the cursor, the selection, or the whole script.
- A default row cap applies. "Fetch all" keeps streaming. Export can re-run the query and stream it straight to a file.
- Every run goes to history, tagged with its origin (user or AI).

---

## 8. Result store and grid

- Results live in memory as Arrow batches and spill to Arrow IPC files in the cache dir above a size limit.
- Each result is registered in DataFusion as table `result`. Sort, column filters, global find (`ILIKE` across columns), SQL-on-result, and column stats (count, distinct, nulls, min/max, top values, histogram) all compile to DataFusion plans.
- The UI requests only visible pages. They are sent as Arrow IPC over a Tauri binary `Channel` and decoded with `apache-arrow` JS.
- The grid supports pin/reorder/resize, copy as TSV/CSV/JSON/Markdown/INSERT, and a cell viewer for JSON, long text, and binary.
- The same DataFusion engine powers AI result analysis (section 12.6), so the AI can compute aggregates locally instead of receiving raw rows.

---

## 9. Export

Streaming writers over `Stream<RecordBatch>`: CSV/TSV (`arrow-csv`), JSON/NDJSON (`arrow-json`), Parquet (`parquet`), XLSX (`rust_xlsxwriter`), SQL INSERT (dialect-aware), Markdown.
Scope: current view (with filters and sort), full stored result, or re-execute and stream to a file. Exports run as background jobs with progress and cancel.

---

## 10. Workspace persistence (SQLite)

```sql
-- core
connections(id, name, kind, config_json, secret_ref, color, env_tag, folder_id, ai_policy_json, created_at)
folders(id, parent_id, name, kind)
saved_queries(id, name, sql, connection_id, folder_id, tags, description, params_json,
              use_as_ai_example BOOL, created_at, updated_at)
query_history(id, connection_id, sql, origin, ai_session_id, started_at, duration_ms, rows, status, error)
tabs(id, title, sql, connection_id, cursor, order_idx)
settings(key, value_json)

-- AI
ai_providers(id, kind, display_name, base_url, auth_kind, secret_ref, default_model, options_json)
ai_sessions(id, title, connection_id, provider_id, model, created_at, updated_at)
ai_messages(id, session_id, role, content_json, tool_calls_json, tokens_in, tokens_out, cost_usd, created_at)
ai_tool_audit(id, session_id, tool, args_json, decision, result_summary, created_at)

-- auth (see section 6)
auth_profiles(id, owner_kind, owner_id, method, config_json, secret_ref,
              identity, expires_at, last_refreshed_at)   -- owner: connection | ai_provider

-- knowledge (see section 13)
kn_objects(id, connection_id, catalog, schema, name, kind, ddl, comment, row_estimate,
           content_hash, indexed_at)
kn_columns(id, object_id, name, data_type, nullable, is_pk, fk_ref, comment, sample_values_json)
kn_notes(id, connection_id, scope, target_ref, body, author, created_at)   -- glossary, business rules
kn_chunks(id, source_kind, source_id, text)                                -- retrievable unit
kn_chunks_fts USING fts5(text)                                             -- keyword index
kn_chunks_vec USING vec0(embedding float[384])                             -- sqlite-vec
```

API keys and OAuth tokens are never stored in SQLite. `secret_ref` points to the OS keychain.

---

## 11. AI providers and authentication

### 11.1 Provider abstraction

```rust
#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn id(&self) -> &ProviderId;
    fn capabilities(&self, model: &str) -> ModelCaps;   // tools, vision, json mode, ctx window, FIM
    async fn list_models(&self) -> Result<Vec<ModelInfo>>;
    async fn chat_stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatEvent>>>;
    async fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>> { Err(Unsupported) }
}

pub enum ChatEvent {
    TextDelta(String),
    ToolCallDelta { id: String, name: Option<String>, args_delta: String },
    Usage { input: u32, output: u32 },
    Done { stop_reason: StopReason },
}
```

`ChatRequest` uses one normalized schema for messages, tools (JSON Schema), system prompt, and temperature. Each adapter translates it to the vendor's wire format.

### 11.2 Adapters

| Provider | Adapter | Auth |
|---|---|---|
| OpenAI | Responses / Chat Completions API | API key |
| Anthropic | Messages API | API key |
| Google Gemini | Gemini API / Vertex AI | API key, or Google OAuth (browser) for Vertex |
| AWS Bedrock | Converse API (`aws-sdk-bedrockruntime`) | AWS profile / IAM Identity Center SSO (browser) |
| Azure OpenAI / Foundry | OpenAI-compatible | API key or Entra ID OAuth (browser) |
| OpenRouter | OpenAI-compatible | API key or OAuth PKCE (browser) |
| Kiro | Kiro adapter (see note) | Browser sign-in (see note) |
| Local: Ollama, LM Studio, llama.cpp, vLLM | OpenAI-compatible | None / optional key; `base_url` |
| Any other OpenAI-compatible endpoint | Generic adapter | API key + `base_url` |

One generic OpenAI-compatible adapter covers most providers. Dedicated adapters are only needed for Anthropic, Gemini, and Bedrock.

Note on browser sign-in for subscription products (Kiro, ChatGPT, Claude.ai): whether a third-party app may use these subscriptions depends on each provider publishing an OAuth client flow or API for third-party apps, and on their terms of service. Verify this per provider before building it; don't assume it. The shared `auth` crate implements the standard flows, so any provider that supports them can be plugged in. For tools like Kiro CLI, the MCP server in section 15 is a reliable integration path in the other direction: the external agent calls DataBrain.

### 11.3 Authentication

AI providers use the same `auth` crate as database connections (section 6). An API key is validated with a `list_models` call. Browser sign-in uses OAuth PKCE or the device flow, and existing AWS, gcloud, or Azure CLI credentials can be reused.

### 11.4 Model routing

Each task can use a different model. For example, a large model for chat and agent work, a small or local model for inline completion and titles, and a local embedding model for knowledge. The same settings screen also sets per-session token and cost budgets.

---

## 12. AI agent

### 12.1 Interaction modes

| Mode | Trigger | Behavior |
|---|---|---|
| Chat panel | Side panel | Multi-turn agent with tools, scoped to the active connection and tab |
| Inline edit | `Cmd+K` in editor | "Rewrite / optimize / convert to CTE"; result shown as an inline diff; accept or reject |
| Text-to-SQL | Prompt bar above the editor | Generates SQL into the editor (not run automatically by default) |
| Fix error | Button on a failed query | Sends the error, SQL, and relevant schema; proposes a fix as a diff |
| Explain | Context menu | Explains the SQL or EXPLAIN plan in plain language |
| Analyze result | Button on the result grid | Summary, anomalies, and suggested follow-up queries or charts |
| Ghost completion (optional) | Typing | FIM completion from a fast or local model, schema-aware |

### 12.2 Agent loop

```
user msg ─▶ ContextBuilder ─▶ provider.chat_stream ─▶ stream tokens to UI
                ▲                      │
                │                 tool_call?
                │                      ▼
                │              PolicyEngine.check(tool, args, connection policy)
                │                 ├─ Allow  ─▶ execute tool
                │                 ├─ Ask    ─▶ UI approval card (edit/approve/deny)
                │                 └─ Deny   ─▶ tool error returned to model
                │                      ▼
                └──────── tool result (summarized, size-capped) appended
loop until: final answer | max_steps (default 12) | budget exceeded | user stop
```

- Each agent run is a cancellable tokio task. Stopping it also cancels any running DB job it started.
- Tool results are size-capped. Large results are summarized, never dumped whole (see 11.6).
- Every tool call is written to `ai_tool_audit`.

### 12.3 Tools

| Tool | Purpose | Default policy |
|---|---|---|
| `search_schema(query)` | Hybrid search over the knowledge base: tables, columns, notes | Allow |
| `describe_table(ref)` | Columns, types, keys, comments, DDL, related tables | Allow |
| `get_sample_rows(ref, n)` | Few rows to understand values | Per connection "allow sample data" |
| `get_editor(tab?)` / `get_selection()` | Read current SQL | Allow |
| `write_editor(tab, mode, sql)` | `mode` = insert at cursor / replace selection / replace all / new tab; shown as a diff | Ask (setting: auto-apply) |
| `run_query(sql, limit)` | Execute through query-engine | Read: Ask, or Allow if the user enables it. DML/DDL: always Ask. Prod: read-only |
| `explain_query(sql)` | EXPLAIN plan | Allow |
| `result_summary(result_id)` | Schema, row count, column stats | Allow |
| `query_result(result_id, sql)` | DataFusion SQL over a stored result (local) | Allow |
| `get_result_rows(result_id, n)` | Raw rows sent to the model | Per connection "allow result data" |
| `save_query(name, sql, folder)` | Save to saved queries | Ask |
| `add_knowledge_note(target, text)` | Store a business rule or glossary term | Ask |
| `render_chart(result_id, spec)` | Vega-Lite spec rendered in the panel (phase 2) | Allow |

Tools are defined once in Rust (`#[derive(JsonSchema)]` args via `schemars`) and exported both to LLM providers and to the MCP server.

### 12.4 SQL safety for AI-generated statements

1. Parse with `sqlparser` using the connection's dialect. If parsing fails, classify as `Unknown` and require approval.
2. Classify each statement: Read (`SELECT`, `WITH` without DML, `SHOW`, `DESCRIBE`, `EXPLAIN`), DML, DDL, Admin/other.
3. Rules: multi-statement input is split and checked statement by statement. Prod connections allow Read only. `run_query` on Read adds a `LIMIT` if the query has none (dialect-aware wrapping). DML without `WHERE` gets an extra warning.
4. Where supported, AI-originated reads also run in a read-only session or transaction (`SET TRANSACTION READ ONLY`, Snowflake/Databricks warehouse role). This is defense in depth, because a parser-level check alone is not enough.
5. A query timeout and a bytes-scanned warning apply to warehouses. Databricks and Snowflake cost money, so the approval card shows an estimate when the engine provides one.

### 12.5 Context builder

It assembles each request within the model's context budget, in priority order:

1. System prompt: role, the SQL dialect with its quirks, tool usage rules, and safety rules.
2. Active context: connection kind and version, default catalog and schema, current editor SQL and selection, the last error, and the current result summary.
3. Retrieved schema: the top-k relevant tables from knowledge retrieval, rendered as compact DDL with comments, keys, and a few sample values when allowed.
4. Business knowledge: matching notes and glossary entries.
5. Few-shot examples: similar saved queries marked `use_as_ai_example`, plus successful history queries.
6. Conversation history, compacted (older turns summarized) when it gets close to the limit.

For huge warehouses with thousands of tables, only a small retrieved slice is included. The model can pull more with `search_schema` and `describe_table`.

### 12.6 Result analysis without leaking data

The flow when the user clicks "Analyze result":
1. `result_summary`: schema, row count, and per-column stats computed locally in DataFusion.
2. The model decides which aggregates it needs and calls `query_result` (e.g. `SELECT region, sum(revenue) FROM result GROUP BY 1`). These queries run locally and only small outputs go back to the model.
3. Raw rows are sent only if the connection allows result data. Columns tagged as PII are masked.
4. Output: a narrative summary, anomalies, suggested follow-up SQL (insertable into the editor), and optional chart specs.

---

## 13. Knowledge base (metadata as AI knowledge)

### 13.1 Sources

| Source | Content |
|---|---|
| Schema introspection | Catalogs, schemas, tables, views, columns, types, PK/FK, indexes, comments, view definitions, row estimates |
| Sampling (opt-in) | Distinct or top values for low-cardinality columns, e.g. `status ∈ {active, churned}` |
| User notes | Glossary ("MRR = ..."), table notes ("use `orders_v2`, not `orders`"), column semantics |
| Saved queries | Queries marked as AI examples |
| History | Successful queries (opt-in), which reflect real join patterns |
| AI-proposed notes | Things the AI learns during chat, saved only after the user approves |

### 13.2 Indexing pipeline

```
on connect / manual refresh / schedule
  └─ introspect (background, rate-limited, per schema)
      └─ diff by content_hash → only changed objects
          └─ build chunks: one per table (DDL + comments + notes + samples)
                           one per glossary/note, one per example query
              └─ embed (local fastembed by default, or provider embeddings)
                  └─ upsert kn_chunks + FTS5 + sqlite-vec
```

- Incremental: unchanged objects are skipped, so re-indexing a 10k-table Snowflake account stays cheap.
- Scope control: include or exclude schemas per connection (e.g. skip `information_schema` or staging schemas).
- Local embeddings are the default, so metadata doesn't leave the machine just to be indexed.

### 13.3 Retrieval

Hybrid scoring: BM25 (FTS5) plus vector similarity, fused with reciprocal rank fusion, then FK-graph expansion that adds tables joined to the top hits. Results are deduplicated and trimmed to the token budget.

### 13.4 Knowledge UI

- A "Knowledge" tab per connection lists indexed objects, shows status, and allows re-index.
- Users can edit table and column descriptions, add glossary entries, and approve or reject notes the AI proposes.
- Knowledge can be exported and imported as JSON or Markdown, so a team can share it through git.

---

## 14. AI privacy and policy model

Per-connection `ai_policy`:

```json
{
  "ai_enabled": true,
  "allowed_providers": ["local-ollama", "anthropic-work"],
  "share_metadata": true,
  "share_sample_values": false,
  "share_result_rows": false,
  "pii_columns": ["users.email", "users.phone"],
  "run_query": "ask",          // ask | auto_read | never
  "allow_write": false,
  "max_rows_to_model": 50
}
```

- Prod connections default to local-only or metadata-only, with `run_query` set to `ask` and writes disabled.
- A "What will be sent" inspector shows the exact payload of the last request.
- Nothing is sent to any provider until the user configures one and enables AI for that connection.

---

## 15. MCP server (optional)

`mcp-server` exposes the same tool registry (with the same policy engine) over MCP, using stdio or local HTTP bound to `127.0.0.1` with a random token. External agents such as Kiro CLI or Claude Code can then list schemas, search knowledge, and run policy-checked queries through the user's DataBrain connections. It is off by default, enabled per connection, and every call is written to the audit log.

---

## 16. Frontend structure

```
ui/src/
├─ app/                 # layout, theme, routing
├─ features/
│  ├─ connections/      # per-kind forms, auth method tabs (browser / token / key pair / CLI), SSH, env tag, AI policy tab
│  ├─ explorer/         # schema tree, object detail
│  ├─ editor/           # Monaco, tabs, inline AI (Cmd+K), diff accept/reject, ghost text
│  ├─ results/          # grid, filter/find, stats, export, "Analyze" button
│  ├─ saved/  history/
│  ├─ ai/
│  │  ├─ panel/         # chat, streaming markdown, tool-call cards, approval cards
│  │  ├─ providers/     # add provider, API key / browser sign-in, model picker
│  │  └─ knowledge/     # knowledge browser, notes, glossary, index status
│  └─ palette/          # Cmd+K command palette (includes "Ask AI")
├─ ipc/                 # tauri-specta generated bindings
└─ store/               # Zustand
```

AI UX principles:
- The AI can prepare changes but not apply them on its own. Editor edits appear as diffs, queries appear as approval cards showing the SQL, target connection, and classification, and the user decides.
- Tool activity is visible and collapsible, like a Cursor or Claude-style agent trace.
- One-click "Insert into editor", "Run", and "Open in new tab" on every SQL block.

---

## 17. Security summary

- The OS keychain holds DB passwords, API keys, PATs, private keys, and refresh tokens. They are never written to SQLite, logs, exports, or prompts.
- Browser sign-in follows section 6.6: PKCE, state and nonce, ID-token validation, and a single-use `127.0.0.1` loopback listener. Tokens refresh automatically and are revoked on sign-out.
- AI SQL goes through parser classification, the policy engine, a read-only session where possible, a row cap, and a timeout.
- Prompt injection: data coming from the database (comments, cell values) is placed in delimited "untrusted data" blocks. Tool permissions are enforced by the policy engine, not by the model's judgment, so injected text can't escalate privileges.
- Tauri capability allowlist and strict CSP. The MCP server is off by default, bound to localhost, and requires a token.
- No telemetry by default. Provider requests go directly from the app to the provider.

---

## 18. Error handling, observability, testing

- `thiserror` in the crates, with one serializable `AppError { code, message, detail, db_code, position }`. DB error positions are underlined in the editor.
- `tracing` with a rolling file log. Prompts and SQL are logged only at debug level.
- Token usage and cost are tracked per message and session and shown in the AI panel.
- Tests:
  - Connector contract tests with `testcontainers`. Cloud tests are gated on credentials.
  - Result-store property tests.
  - SQL classifier tests with a corpus of tricky statements (CTE + DML, `SELECT ... INTO`, dialect specifics).
  - Provider adapters tested against recorded HTTP fixtures.
  - An agent eval set of text-to-SQL tasks on a sample DB, scored by execution-result match, run in CI against a local model.
  - UI: Vitest + Playwright.

---

## 19. Delivery roadmap

| Phase | Scope |
|---|---|
| **0 – Skeleton** | Workspace, Tauri app, SQLite workspace, connection CRUD + keychain; `auth` crate with `CredentialSource` + password/API-token methods |
| **1 – MVP** | Postgres, MySQL, SQLite; editor; run/cancel; Arrow grid; history; saved queries |
| **2 – Explore** | DataFusion filter/sort/find/stats, exports, schema explorer, autocomplete |
| **3 – AI core** | Provider trait + OpenAI-compatible, Anthropic, Ollama adapters; API-key auth; chat panel; text-to-SQL; fix error; `write_editor` diffs; policy engine + SQL classifier |
| **4 – Knowledge** | Metadata indexer, local embeddings, hybrid retrieval, notes/glossary UI, few-shot from saved queries |
| **5 – Agent + analysis** | `run_query` with approvals, result analysis via DataFusion, charts, audit log, budgets |
| **6 – Cloud** | Snowflake (PAT, key-pair, OAuth, `externalbrowser` via ADBC), Databricks (PAT, OAuth U2M/M2M), BigQuery (Google OAuth, service account); token manager with refresh/re-auth; device code; CLI profile reuse; SSH; Gemini, Bedrock, Azure adapters |
| **7 – Enterprise + ecosystem** | Oracle, SQL Server + Entra ID, Redshift IAM, custom OIDC (Okta/Entra/Keycloak), ClickHouse, ADBC; MCP server; ghost completion; knowledge export/import |

### Implementation status

Phases 0–2 are built, plus most of phases 3–7: AI mode, knowledge, agent with approvals, cloud connectors, SQL Server/Oracle, SSH, OAuth/browser sign-in, MCP, and additionally a DuckDB local-file engine and notebooks. Where the code differs from the design above:

- **Editor:** CodeMirror 6 instead of Monaco. It is smaller, bundles cleanly into Tauri, and has a first-class SQL dialect package. Inline AI diffs (phase 3) can use `@codemirror/merge`.
- **Result views:** filtering, sorting, find and stats run on Arrow compute kernels (`arrow-ord`, `arrow-string`) rather than DataFusion. DataFusion is still planned for SQL-on-result and AI analysis (phase 5). Spill-to-disk is not implemented yet; results live in memory, capped by the row limit.
- **Grid transport:** pages are sent as JSON arrays of display strings (≤5,000 rows per request) rather than Arrow IPC. This is simpler and fast enough for visible-window paging. It can move to binary Channels later.
- **Workspace store:** uses `rusqlite` with `PRAGMA user_version` migrations instead of `sqlx`, so the build has a single SQLite driver.
- **PostgreSQL results:** column types come from a prepared statement, and rows are fetched with the simple (text) protocol. Every Postgres type then displays exactly as `psql` shows it, while common types still become typed Arrow columns.
- **Exact decimals** (Postgres `numeric`, MySQL `DECIMAL`) are kept as text so no precision is lost. Filters and sorts still treat them as numbers.
- **Toolchain:** the code targets rustc 1.87, which pins Tauri at 2.11 and `mysql_async` at 0.36. The npm `@tauri-apps/*` packages are pinned to the same minor versions.
- **Cloud connectors** use the vendors' HTTP APIs directly (Snowflake driver login + query protocol, Databricks SQL Statement API 2.0, BigQuery Jobs API with sessions) instead of ADBC, so no native driver is bundled.
- **JWT signing** (Snowflake key pair, Google service accounts) is hand-rolled RS256 on `rsa`, because `jsonwebtoken` needs rustc 1.88.
- **Knowledge retrieval** is SQLite FTS5 (BM25) plus one foreign-key hop, not embeddings. SQL-on-result for AI analysis uses an in-memory SQLite copy of the capped result rather than DataFusion.
- **Inline AI** (⌘I) sends proposals to the assistant panel as a line diff with Accept/Reject rather than an in-editor merge view.
- **MCP server** is a separate stdio binary (`databrain-mcp`), with no network endpoint. Only connections with `mcp_enabled` are visible to it, and approvals that need the UI are refused. Kiro CLI is supported through it (`~/.kiro/settings/mcp.json`).
- **DuckDB** (bundled, `duckdb` 1.10506 / DuckDB 1.5) reads local files via `read_csv`/`read_parquet`/`read_json_auto`/`read_xlsx`/`delta_scan`/`iceberg_scan`. Attached files become views in the `files` schema. The Excel, Delta and Iceberg extensions download on first use. DuckDB's Arrow 58 batches are converted to the workspace's Arrow 59 over IPC.
- **Notebooks** (workspace v3) store cells as JSON. Each SQL cell keeps its own results (run key `nb:{notebook}:{cell}`), and all cells share one database session through `RunRequest.session_key = nb:{notebook}`, so temp tables carry across cells.
- **Kiro provider** has no public model API, so it runs through `kiro-cli acp` (Agent Client Protocol over stdio). Each turn:
  - DataBrain starts a temporary MCP endpoint that exposes its tools. It listens only on 127.0.0.1 with a random port and a per-turn bearer token, rejects browser `Origin` headers and wrong `Host` headers, and stops when the turn ends.
  - Kiro runs with a managed agent (`databrain-sql`) whose tools are limited to `@databrain`. DataBrain refuses Kiro's other permission requests and all fs/terminal requests. The usual DataBrain approvals still apply.
  - Auth is either the `kiro-cli login` browser session (run in a terminal window, then detected with `whoami`), or a `ksk_` API key from the keychain passed to the child as `KIRO_API_KEY`.
  - The Kiro session id is stored per conversation and resumed with `session/load`.
- **Runtime caveats:**
  - Oracle needs Oracle Instant Client installed.
  - Browser OAuth for BigQuery, Snowflake and Entra ID needs your own OAuth client ID. Databricks uses the `databricks-cli` public client.
  - No live Snowflake, Databricks, BigQuery, SQL Server or Oracle servers were available, so those connectors are covered by unit tests and gated live tests only.
