# DataBrain

A modern desktop SQL client written in Rust (Tauri 2 + React). See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the full design and roadmap.

## What works today

- **Connections:**
  - Databases: SQLite, PostgreSQL, MySQL/MariaDB, SQL Server, Oracle, Snowflake, Databricks, BigQuery, and DuckDB for local files.
  - Sign-in: passwords/tokens/keys go in the OS keychain. Browser sign-in (OAuth PKCE, Snowflake SSO), device code, service principals, service accounts, and CLI logins (gcloud, Databricks CLI, az).
  - SSH tunnels with host-key pinning.
  - Test fails fast: a DNS + TCP check tells "unknown host", "connection refused" and "no answer" apart within seconds, the whole attempt gives up after the connect timeout (Settings → Queries → Connecting, default 20 s; browser sign-in is not limited), and Stop cancels it.
  - Folders with drag and drop.
- **Explorer:** three-level engines (Databricks, Snowflake, BigQuery, DuckDB) show catalog → schema → tables, catalogs first: a catalog's schemas are listed when it opens.
  - Cached locally (app-data SQLite): catalogs, schemas, tables and columns you have seen show at once on the next start, before connecting, and stay browsable offline ("Offline · cached 2 h ago"; completion uses the same cache).
  - On connect, one fingerprint query per connection tells which cached schemas changed (Postgres, MySQL/MariaDB, SQL Server, Oracle, Snowflake, Databricks, DuckDB, SQLite). Unchanged schemas are not read. Changed ones are refreshed table by table on Postgres, MySQL, SQL Server and Oracle (only new and changed tables are fetched; columns of changed tables are re-read when opened), and listed again elsewhere. A schema's Refresh button works the same way.
  - In the background, the default catalog's schemas and the tables of the default and recently open schemas are listed, so clicking them doesn't wait.
  - Editing a connection's server, database, user or options clears its cache. Objects are grouped into Tables, Views, Materialized views, Functions, Procedures and Sequences (where the engine has them). Right-click a connection → "Choose catalogs…" (Databricks; databases on Snowflake/DuckDB, projects on BigQuery) or "Choose schemas…" (other engines) to list only some of the top-level items. Large schemas show 200 objects at a time with a filter box, so 10k-table catalogs stay responsive. Find tables with ⌘P: it searches every connected database, or one connection that you choose, with `schema.name` matching. Each match can be shown in the tree, inserted into the editor, or opened with a select of the top 100 rows. Right-click an object → "Show DDL" opens a read-only tab at once that loads the DDL (Copy, Open in editor, Reload): complete table DDL with keys, foreign keys, checks, indexes, partitioning and comments on Postgres and SQL Server; the database's own DDL on MySQL (`SHOW CREATE`), Oracle (DBMS_METADATA, packages with their body), Snowflake, Databricks, BigQuery, DuckDB and SQLite; functions and procedures on Postgres, MySQL, SQL Server and Oracle.
- **Local files:** query CSV/TSV, Parquet, JSON/NDJSON, Excel, Delta Lake and Iceberg with DuckDB. Use "Query a local file…" in the sidebar or palette, or attach files to a DuckDB connection (they appear as views in `files`). Excel: workbooks with several sheets ask for the first sheet or all sheets (connections have a "Read every sheet" option, one view per sheet); the whole used range is read, including columns under blank header cells (`column_D`) and rows after empty rows.
- **Editor:** CodeMirror 6 with dialect highlighting and schema autocomplete, run statement/selection/script, cancel, and error underlines. Parameters: write `:data_date` and a box for it appears above the editor; text is quoted for you (`2026-09-09` → `'2026-09-09'`), numbers (`2000`), `NULL`/`TRUE`/`FALSE` and already-quoted literals are kept, and the `{}` toggle in a box inserts an expression as typed (`current_date - 1`); hover a box to see what goes into the query. Notebooks get the same bar at the top, shared by all cells. Values are saved per tab; `::` casts, strings, comments and `col:path` are not parameters. Right-click (or ⇧⌥F) formats the selection, or the whole script when nothing is selected, in the connection's dialect (also in notebook cells). Autocomplete looks tables and columns up on demand, filtered by what you type (tables and columns you have already seen in the explorer, ⌘P or completion are cached locally across restarts and answer at once, along with the knowledge index; the server is asked in the background), so it never loads a whole schema or catalog and stays fast on 10k-table catalogs. Picking a table or view in FROM/JOIN also writes a short alias (`public.customer_orders co`; unique in the statement, skipped when an alias is already there). Functions and packages complete in expressions, procedures where they can be called (`BEGIN`, `CALL`, `EXEC`), and `pkg.` lists an Oracle package's functions and procedures; all inserted schema-qualified, ready to call (`app.fn_total(`).
  - Query tips for slow queries (all engines): when a statement runs longer than the slow-query threshold (Settings → Queries, default 60 s; 0 turns tips off), DataBrain reads the indexes, partitions and cluster keys of its tables on a separate session while it keeps running, and lists ways to make it cheaper above the result (refreshed when it ends; faster queries get no tips) (no partition filter, a function or cast on a partition/indexed column, `LIKE '%…'` on an indexed column, a filter no index covers on a large table). Click a tip to select the spot in the editor, or "Improve with AI" to have the assistant rewrite that statement (it gets only the statement, its tips and its tables' indexes/partitions/cluster keys, and the rewrite is shown as a diff to accept); the ran statement is underlined with the reason on hover, and table names show their layout. Autocomplete lists key columns first (`partition key`, `cluster key`, `primary key`, `indexed`).
- **Lineage:** column lineage of SQL, drawn as a diagram in a read-only tab.
  - Open it with "Lineage" in the editor toolbar, or right-click → Show lineage (also in notebook cells). It traces the selection, or the whole script when nothing is selected. Refresh re-reads the editor.
  - The diagram starts at table level, one line per link between tables. Click a table to list its columns ("All columns" lists every table's), click a column to trace it: its path is highlighted and the tables on it show the columns it uses.
  - The right panel shows the traced column as a top-down flow: the column, an arrow with its expression (and the joins/filters where several inputs meet), then its input columns side by side, down to the database tables. Below it, the column written in database columns (`total = sum(sales.orders.amount * sales.orders.rate)`). The panel can be resized, ↗ opens the flow in a tab of its own, and clicking an expression or join selects it in the SQL.
  - Lay it out left to right or top to bottom and drag tables around (view only; Reset layout puts them back). Export to draw.io, Mermaid (or copy it), Graphviz DOT, SVG, PNG or JSON, as laid out on screen (Mermaid and DOT keep the direction, not moved positions).
  - "Check with database" (on with a connection): uncached tables are described, and each read query is checked with `EXPLAIN`, never run, on Postgres, MySQL, DuckDB, SQLite and Snowflake. Errors show in the header with their position; writes and DDL are skipped; offline, the diagram still works from the explorer cache. `:name` parameters use the values entered.
  - **Covered:** SELECT with joins (inner, left, right, full, cross, semi/anti, `ON`, `USING`), `WHERE`/`HAVING`/`QUALIFY`, `GROUP BY` and aggregates, window functions, `CASE` and other expressions, subqueries in `FROM` (including `LATERAL`), in the select list and in `WHERE`/`EXISTS`/`IN`, CTEs, `UNION`/`INTERSECT`/`EXCEPT` (by position), `VALUES`, `UNNEST`, `SELECT *` and `alias.*` (from the explorer cache or a describe), quoted identifiers per dialect. Scripts: `CREATE TABLE … AS`, `CREATE VIEW … AS` and `INSERT … SELECT` link statements, so a table written by one statement is the one a later statement reads. Each step is marked copied, computed, aggregated, filter or join. Unknown columns, ambiguous names and columns that can't be placed are listed under "N not traced", never guessed.
  - **Not covered (yet):**
    - `WITH RECURSIVE` is only traced when the CTE has a column list (`t(n)`); without one, the recursive step's columns are not resolved. Recursive CTEs written without `RECURSIVE` (SQL Server, Oracle) are not recognised, and Oracle `CONNECT BY` is ignored.
    - `UPDATE`, `DELETE`, `MERGE`, DDL and procedural blocks (`BEGIN … END`, `DECLARE`): only their tables are shown. Statements the parser rejects (some PL/SQL and vendor syntax) are shown the same way and marked partial.
    - `PIVOT`/`UNPIVOT`, table functions, `JSON_TABLE` and similar sources appear as a box without column lineage. A subquery in the select list is linked through its first column only.
    - Database views are not expanded: a view is a box like a table.
    - `SELECT *` on a table whose columns are neither cached nor reachable shows one `table.*` line. An unqualified column with two such tables in scope is not assigned.
    - No `EXPLAIN` check on SQL Server, Oracle, Databricks and BigQuery. The check only reports errors; it doesn't correct the lineage. A statement that reads a table created earlier in the same script may be reported as failing.
    - One editor, selection or notebook cell at a time: no lineage across saved queries, notebooks or the workspace, and no lineage recorded by the server (Unity Catalog, Snowflake access history, BigQuery).
    - Expressions are shown as the parser prints them (normalised spacing); the database-columns formula stops expanding past about 240 characters.
    - Lineage tabs, moved tables and expanded tables are not kept after a restart.
- **Notebooks:** SQL and Markdown cells on a connection.
  - Run a cell, run all, or run from here.
  - Output stays under each cell, and all cells share one session.
  - Per-cell connection override and AI actions. Notebooks are saved and can be put in folders.
- **Jobs:** SQL steps that run in order, by hand or on a schedule (sidebar → Jobs).
  - A job opens in a tab as a canvas of steps. Drag steps around; drag a step's ● handle onto another step so it runs after it (or onto empty space to add one there). Click a link and press Delete (or right-click it) to remove it. Selecting a step highlights everything upstream and downstream of it.
  - "Create job" at the top of a notebook makes a job with one step per SQL cell, top to bottom, each running after the cell above, on the cell's connection (or the notebook's). Steps are named after the cells' output names; as those names belong to the notebook, a step gets the next free name (`revenue_2`) and `results.revenue` in later steps is renamed to match. Markdown and empty cells are skipped; `:name` parameters are not filled in.
  - Each step has a name, a connection and SQL, edited in the side panel (resizable) with the usual completion and formatting. "Copy from a query tab" (or right-click the canvas → From a query tab) takes a tab's SQL and connection.
  - A step's result is an output named after the step, and the step owns that name: no other step (in any job) can have it, and query tabs, notebook cells and renames can't produce `results.<step>` (the step-name box says what already uses a name). A name is free again when the step is renamed or deleted. Steps on Results (DuckDB) read upstream outputs as `results.<step>`, so they can combine several connections; renaming a step renames it in the SQL of the steps that read it, and a step that reads `results.x` without running after `x` gets a "Link" warning.
  - Right-click a step: Run this step, Run from here (it and everything downstream), Add downstream step (DuckDB, reading this one), Load into another connection…, Save to a local file…, View output, Duplicate, Delete.
  - Load steps select rows with DuckDB (by default `select * from results.<upstream>`) and write them into a table of any connection: Append, Truncate (delete its rows first) or Replace (drop and create it from the rows' column types), as batched `INSERT`s in that database's dialect.
  - Save-to-file steps select rows the same way and DuckDB writes them (`COPY … TO`) as CSV (with a header), Parquet (zstd) or JSON lines into a folder you choose (Downloads by default; a missing folder fails the step before it runs). Leave the file name empty for a new name per run (`{step}_{date}_{time}`), or give one, with `{step}`, `{job}`, `{date}`, `{time}` filled in; a fixed name is overwritten on each run. "Show file" reveals the last file in Finder/Explorer.
  - Steps that don't depend on each other run at the same time; when a step fails, the steps after it are skipped and the others still run. Each step shows its status, rows and time, and the job panel lists recent runs with each step's outcome (the last 200 are kept). Stop cancels the running statements.
  - Schedule: every N minutes, or at a time of day on chosen weekdays (local time). Schedules run while DataBrain is open; a run missed while it was closed runs once at the next start, and a failed scheduled run shows a notification.
  - Runs don't ask for confirmation, also on production connections (the load panel warns about those); read-only connections are refused. `:name` parameters are not filled in for job steps.
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
  lineage/         column lineage of SQL scripts (sqlparser)
  ai/              providers, agent + tools, policy, knowledge index, MCP server
  app/             Tauri host (commands + events) and the databrain-mcp binary
ui/                React + TypeScript + Vite + Tailwind
```

App data (workspace DB) is stored in the OS app-data directory, for example `~/Library/Application Support/dev.databrain.app/` on macOS.
