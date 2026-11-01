# DataBrain

A desktop SQL client for many databases, with column lineage, scheduled jobs and an AI assistant. Built with Rust, Tauri 2 and React.

[Download](#download) · [Features](docs/FEATURES.md) · [Architecture](docs/ARCHITECTURE.md) · [Building and releasing](docs/RELEASING.md)

## Download

| Platform | Installer |
|---|---|
| macOS (Apple Silicon) | [DataBrain_0.2.1_aarch64.dmg](https://github.com/toan1804/databrain/releases/download/v0.2.1/DataBrain_0.2.1_aarch64.dmg) |
| macOS (Apple Silicon), without DuckDB | [DataBrain-no-duckdb_0.2.1_aarch64.dmg](https://github.com/toan1804/databrain/releases/download/v0.2.1/DataBrain-no-duckdb_0.2.1_aarch64.dmg) |
| macOS (Intel), Windows, Linux | Built by the Release workflow from the next release on |

All versions are on the [Releases](https://github.com/toan1804/databrain/releases) page. The app checks for updates once a day and never installs anything by itself.

> [!NOTE]
> The macOS app is not signed yet. Move DataBrain.app to Applications, then right-click it → **Open** the first time.

The "without DuckDB" build is about 85 MB instead of 290 MB and uses the DuckDB library installed on the computer (1.5 or newer, e.g. `brew install duckdb`).

## Features

- **Databases:** PostgreSQL, MySQL/MariaDB, SQL Server, Oracle, SQLite, Snowflake, Databricks, BigQuery, DuckDB, MongoDB and Neo4j. Passwords are kept in the OS keychain. Supports browser sign-in, cloud CLI logins and SSH tunnels.
- **Local files:** query CSV, Parquet, JSON, Excel, Delta Lake and Iceberg files with DuckDB.
- **Explorer:** cached schemas that open at once and work offline, fast on catalogs with 10,000+ tables, DDL for every object, schema compare.
- **Editor:** dialect-aware completion and formatting, `:name` parameters, transactions, query plans, tips for slow queries, `.sql` files.
- **Results:** virtualized grid, cell editing, charts and dashboards, export to CSV, JSON, Parquet, XLSX and more.
- **Outputs:** every result gets a name you can query with DuckDB as `results.<name>`, to join data across databases, compare versions and pin results.
- **Lineage:** column lineage of SQL scripts, drawn as a diagram and exportable to draw.io, Mermaid, DOT, SVG or PNG.
- **Notebooks:** SQL and Markdown cells that share one session.
- **Jobs:** steps that query, load into another database or save files, on a schedule, with run logs, progress and resume after a failure.
- **AI assistant:** Kiro, OpenAI, Anthropic, Gemini, Azure OpenAI, OpenRouter, Ollama or LM Studio. It uses your schema and notes, runs queries only as each connection's policy allows, and reads and writes only the tables you permit.
- **MCP server:** `databrain-mcp` gives Kiro CLI, Claude Code, Cursor and other clients access to the connections you choose.

See [docs/FEATURES.md](docs/FEATURES.md) for the full reference.

## Build from source

Requirements:

- Rust 1.87+ and Node 20+ (Go 1.27+ only to build the Oracle driver helper)
- macOS: Xcode Command Line Tools · Linux: [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/) · Windows: WebView2 and MSVC build tools

```sh
cd ui
npm install
npm run app:dev               # app with hot reload
npm run app:build             # release bundle for this platform
npm run app:build:no-duckdb   # bundle that uses the computer's DuckDB
```

## Test

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
cd ui && npm test && npm run build
```

The TypeScript types of what the backend sends are generated from the Rust types: `cargo test` writes them to `ui/src/lib/bindings` (commit them; CI checks they are current).

Tests that need a database server are marked `#[ignore]`, so `cargo test` lists them as ignored. Run them with `--ignored` and the server's `DATABRAIN_*` variables; a missing variable fails the test rather than skipping it:

```sh
DATABRAIN_PG_HOST=localhost DATABRAIN_PG_USER=postgres DATABRAIN_PG_PASSWORD=... \
  cargo test -p databrain-connector-postgres -- --ignored
```

The variables are `DATABRAIN_PG_*`, `DATABRAIN_MYSQL_*`, `DATABRAIN_MSSQL_*`, `DATABRAIN_ORACLE_*`, `DATABRAIN_SF_*`, `DATABRAIN_DBX_*`, `DATABRAIN_BQ_PROJECT`, `DATABRAIN_MONGO_URI`, `DATABRAIN_NEO4J_*` and `DATABRAIN_SSH_*`. CI runs the Postgres, MySQL and MongoDB ones against service containers. A sample dataset for trying the app is in `examples/data`.

## Project layout

```
crates/
  app/             Tauri host and the databrain-mcp and databrain-jobs binaries
  query-engine/    sessions, query jobs, cancel, safety checks
  connectors/      one crate per database type
  connector-core/  connector traits, statement splitting and classification
  workspace/       local SQLite store: connections, history, notebooks, jobs
  ai/              AI providers, agent tools, knowledge index, MCP server
  lineage/         column lineage of SQL
  result-store/    in-memory Arrow results
  export/          file writers
  auth/            keychain and sign-in methods
  ssh-tunnel/      SSH tunnels
  duckdb-dynload/  DuckDB loaded at runtime (builds without bundled DuckDB)
ui/                React, TypeScript, Vite and Tailwind
```

To add a database type, see [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) §5.1–5.2.
