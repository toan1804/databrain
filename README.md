# DataBrain

A modern desktop SQL client written in Rust (Tauri 2 + React). See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the full design and roadmap.

## What works today

- **Connections:**
  - Databases: SQLite, PostgreSQL, MySQL/MariaDB, SQL Server, Oracle, Snowflake, Databricks, BigQuery, and DuckDB for local files.
  - Sign-in: passwords/tokens/keys go in the OS keychain. Browser sign-in (OAuth PKCE, Snowflake SSO), device code, service principals, service accounts, and CLI logins (gcloud, Databricks CLI, az).
  - SSH tunnels with host-key pinning.
  - Folders with drag and drop.
- **Explorer:** three-level engines (Databricks, Snowflake, BigQuery, DuckDB) show catalog → schema → tables. Objects are grouped into Tables, Views, Materialized views, Functions, Procedures and Sequences (where the engine has them). Right-click a connection → "Choose catalogs…" (Databricks; databases on Snowflake/DuckDB, projects on BigQuery) or "Choose schemas…" (other engines) to list only some of the top-level items. Large schemas show 200 objects at a time with a filter box, so 10k-table catalogs stay responsive. Find tables with ⌘P: it searches every connected database, or one connection that you choose, with `schema.name` matching. Each match can be shown in the tree, inserted into the editor, or opened with a select of the top 100 rows. Right-click an object → "Show DDL" opens its DDL in a new tab (or "Copy DDL"): complete table DDL with keys, foreign keys, checks, indexes, partitioning and comments on Postgres and SQL Server; the database's own DDL on MySQL (`SHOW CREATE`), Oracle (DBMS_METADATA, packages with their body), Snowflake, Databricks, BigQuery, DuckDB and SQLite; functions and procedures on Postgres, MySQL, SQL Server and Oracle.
- **Local files:** query CSV/TSV, Parquet, JSON/NDJSON, Excel, Delta Lake and Iceberg with DuckDB. Use "Query a local file…" in the sidebar or palette, or attach files to a DuckDB connection (they appear as views in `files`). Excel: workbooks with several sheets ask for the first sheet or all sheets (connections have a "Read every sheet" option, one view per sheet); the whole used range is read, including columns under blank header cells (`column_D`) and rows after empty rows.
- **Editor:** CodeMirror 6 with dialect highlighting and schema autocomplete, run statement/selection/script, cancel, and error underlines. Autocomplete looks tables and columns up on demand, filtered by what you type (tables and columns you have already seen in the explorer, ⌘P or completion are cached locally across restarts and answer at once, along with the knowledge index; the server is asked in the background), so it never loads a whole schema or catalog and stays fast on 10k-table catalogs. Picking a table or view in FROM/JOIN also writes a short alias (`public.customer_orders co`; unique in the statement, skipped when an alias is already there). Functions and packages complete in expressions, procedures where they can be called (`BEGIN`, `CALL`, `EXEC`), and `pkg.` lists an Oracle package's functions and procedures; all inserted schema-qualified, ready to call (`app.fn_total(`).
  - Query tips for slow queries (all engines): when a statement runs longer than the slow-query threshold (Settings → Queries, default 60 s; 0 turns tips off), DataBrain reads the indexes, partitions and cluster keys of its tables on a separate session while it keeps running, and lists ways to make it cheaper above the result (refreshed when it ends; faster queries get no tips) (no partition filter, a function or cast on a partition/indexed column, `LIKE '%…'` on an indexed column, a filter no index covers on a large table). Click a tip to select the spot in the editor; the ran statement is underlined with the reason on hover, and table names show their layout. Autocomplete lists key columns first (`partition key`, `cluster key`, `primary key`, `indexed`).
- **Notebooks:** SQL and Markdown cells on a connection.
  - Run a cell, run all, or run from here.
  - Output stays under each cell, and all cells share one session.
  - Per-cell connection override and AI actions. Notebooks are saved and can be put in folders.
- **Outputs:** every result gets a handle (`r12`), and you can also give it a name (`revenue`).
  - Query outputs together with DuckDB as `results.<name>`, including joins across databases (Postgres × Snowflake × CSV…).
  - A rerun keeps the previous version as `revenue__1`. Compare versions (rows added, removed or changed) and chart any output.
  - @mention outputs to the AI.
  - Pin outputs to keep them across restarts (saved as Parquet in the app-data `outputs/` folder).
  - History links to each output. Capped outputs warn when they are queried.
  - Notebook cells can name their output; later cells read it, and cells whose inputs changed are marked stale.
- **Results:** virtualized grid, sort, filters, find, column stats, copy. Export to CSV, TSV, JSON, NDJSON, Markdown, SQL INSERT, Parquet or XLSX.
- **AI mode** (⌘L panel, ⌘I inline edit):
  - Providers: Kiro (browser sign-in or `ksk_` API key, through `kiro-cli`), OpenAI, Anthropic, Gemini, Azure OpenAI, OpenRouter (browser sign-in), Ollama, LM Studio, or any OpenAI-compatible server.
  - The assistant uses indexed schema metadata plus your notes/glossary as knowledge. Indexing is incremental: one cheap catalog query per run tells which schemas changed (Postgres, Oracle, MySQL/MariaDB, SQL Server, Snowflake, Databricks Unity Catalog, DuckDB, SQLite), and only those are re-read; "Rebuild" re-reads everything. Postgres, Oracle, MySQL, SQL Server, Snowflake and Databricks read a whole batch of schemas with a few catalog queries instead of several per schema. BigQuery re-reads every dataset. While exploring it records what it learns (code meanings, business rules, join paths) and updates outdated notes; changes wait for review in Knowledge unless the connection's "Save AI notes directly" is on.
  - A note can be about one or more tables of the connection (`sales.orders`, `sales.orders.status`, or `sales.orders & crm.customers` for join knowledge; `and`, `or` and `,` also separate tables). The field completes table and column names, and only tables that exist are accepted (checked against the knowledge index, then tables already seen in the explorer, ⌘P or autocomplete, then the live connection, which gives up after 10 s if it is busy indexing) — for notes you write, notes the AI writes, and imports.
  - Share notes: Knowledge → Export… writes a `.databrain-notes.json` file; Import… shows new notes, ones you already have, and ones that differ. For each difference, keep yours, use the file's, keep both, edit a merge, or let the AI merge them. It proposes SQL as diffs, runs queries after approval (per-connection policy), and analyzes results.
  - Fix/Explain/Analyze buttons; conversation history; audit log.
- **MCP:** `databrain-mcp` (stdio) exposes opted-in connections to Kiro CLI, Claude Code, Cursor… (Settings → MCP / Kiro shows the config snippet).
- Safety prompts for prod and for writes, read-only connections, saved queries, history, command palette (⌘K), dark/light theme.

Runtime notes:
- Oracle needs no Oracle software by default: the thin driver (a small helper using [go-ora](https://github.com/sijms/go-ora), Oracle 10g+, TLS/wallets, native network encryption via "Driver options") is downloaded on the first Oracle connection (about 5 MB, into the app-data folder `oracle-agent/`, checked against SHA-256 sums built into DataBrain). Choose the "Oracle Instant Client" driver on a connection for thick-only features; DataBrain then checks for Instant Client and can install Oracle's latest Basic package in one click (into `oracle/instantclient_*`; Linux also needs `libaio`).
- Google/Snowflake/Entra browser OAuth needs your own OAuth client ID.
- DuckDB is compiled into the app (with Parquet and JSON). Its Excel, Delta, Iceberg, Avro, httpfs and ICU extensions are downloaded at build time and shipped inside the bundle, so nothing is downloaded at runtime.
- Kiro needs Kiro CLI installed (`curl -fsSL https://cli.kiro.dev/install | bash`). API keys require a Kiro Pro plan or higher. DataBrain writes one agent config, `~/.kiro/agents/databrain-sql.json`, which limits Kiro to DataBrain's tools.

## Requirements

- Rust ≥ 1.87 and Node ≥ 20 (Go ≥ 1.27 only to build the Oracle thin driver helper)
- macOS: Xcode Command Line Tools. Linux: the [Tauri system dependencies](https://v2.tauri.app/start/prerequisites/). Windows: WebView2 + MSVC build tools.

## Run

```sh
cd ui
npm install
npm run app:dev      # Vite dev server + Tauri app with hot reload
npm run app:build    # release bundle (.app/.dmg, .msi, .deb/.AppImage)
```

`npm run dev` alone serves the UI in a browser, but queries need the desktop backend.

`npm run app:build` produces a self-contained app for the build machine (macOS: `target/release/bundle/macos/DataBrain.app` and a `.dmg`; Windows: `.msi`/`.exe`; Linux: `.deb`/`.AppImage`). The bundle includes `databrain-mcp` and the DuckDB extensions; SQLite, DuckDB and (on Linux) OpenSSL are linked statically. Build on each target platform (DuckDB extensions are platform-specific). External tools stay optional: Oracle Instant Client (offered in-app), Kiro CLI, and cloud CLIs for CLI sign-in.

### Oracle thin driver releases

The helper lives in `crates/connectors/oracle/agent` (Go). `node scripts/build-oracle-agent.mjs` builds every platform reproducibly into `target/oracle-agent/`, writes the gzip files to upload to `target/oracle-agent/dist/`, and updates `agent/SHA256SUMS` (compiled into DataBrain). Upload the `.gz` files to `{base}/v{RELEASE}/` (`agent::DEFAULT_BASE_URL`, `agent::RELEASE`; the `oracle_agent_url` setting or `DATABRAIN_ORACLE_AGENT_URL` override the base). `tauri dev` and `tauri build` build the helper for your computer when Go is installed (`target/oracle-agent/`), and DataBrain uses that copy directly, so builds made on a machine with Go never download it.

## Test

```sh
cargo test --workspace          # Rust unit + end-to-end (SQLite) tests
cargo clippy --workspace --all-targets
cd ui && npm test && npm run build
```

`examples/data` has a small sample "shop" dataset to try the app and to drive end-to-end tests (`cargo test -p databrain-app --test example_data`):

- `customers.csv`, `products.csv`: CSV files with quoted commas and quotes, Unicode text and empty values.
- `orders.parquet`, `order_items.parquet`: zstd Parquet with typed columns (TIMESTAMP, DECIMAL).
- `orders_by_year/`: Hive-partitioned Parquet.
- `shop.db`: a SQLite database with the same rows, foreign keys and a view.

Regenerate it with `examples/data/generate.sh` (needs Python 3 and cargo).

Connectors for server databases have live tests that are skipped unless a server is configured. They use the `DATABRAIN_PG_*`, `DATABRAIN_MYSQL_*`, `DATABRAIN_MSSQL_*`, `DATABRAIN_ORACLE_*`, `DATABRAIN_SF_*`, `DATABRAIN_DBX_*`, `DATABRAIN_BQ_PROJECT` and `DATABRAIN_SSH_*` variables. For example:

```sh
DATABRAIN_PG_HOST=localhost DATABRAIN_PG_USER=postgres DATABRAIN_PG_PASSWORD=... \
  cargo test -p databrain-connector-postgres -- --nocapture
DATABRAIN_MYSQL_HOST=127.0.0.1 DATABRAIN_MYSQL_USER=root DATABRAIN_MYSQL_PASSWORD=... \
  cargo test -p databrain-connector-mysql -- --nocapture
```

## Layout

```
crates/
  auth/            keychain secrets, auth methods, OAuth/device/JWT/cloud CLI credentials
  connector-core/  Connector/Session traits, Arrow row builder, SQL splitter + classifier
  connectors/      sqlite, postgres, mysql, mssql, oracle, cloud (snowflake/databricks/bigquery), duckdb
  ssh-tunnel/      russh-based tunnels
  result-store/    in-memory Arrow results: paging, filter, sort, find, stats
  export/          CSV/TSV/JSON/NDJSON/Markdown/SQL/Parquet/XLSX writers
  workspace/       local SQLite: connections, folders, queries, history, tabs, notebooks, AI, knowledge
  query-engine/    sessions, jobs, cancel, row cap, safety, sign-in, SSH
  ai/              providers, agent + tools, policy, knowledge index, MCP server
  app/             Tauri host (commands + events) and the databrain-mcp binary
ui/                React + TypeScript + Vite + Tailwind
```

App data (workspace DB) is stored in the OS app-data directory, for example `~/Library/Application Support/dev.databrain.app/` on macOS.
