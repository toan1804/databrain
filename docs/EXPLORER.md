# Explorer metadata: fetching and caching

The explorer (the connections tree: catalogs, schemas, tables, columns) shows what it cached last time at once, then asks the server what changed. This page says what is fetched when, where it is kept, how a stale entry is found, and how it relates to the AI knowledge index, which is a separate store.

Main files:

| Layer | File |
|---|---|
| UI store | `ui/src/store/explorerSlice.ts` (`openExplorer`, `loadSchemas`, `loadCatalogSchemas`, `loadObjects`, `loadColumns`, `exploreChosen`) |
| Commands | `crates/app/src/api/explorer.rs` (listing, `cached_*`, `refresh_schema`, `revalidate_explorer`, `describe`, completion) |
| Write queue | `crates/app/src/meta_writer.rs` (`MetaWriter`) |
| Cache tables | `crates/workspace/src/meta_cache.rs` |
| Engine calls | `crates/query-engine/src/metadata.rs`, the `Session` trait in `crates/connector-core/src/lib.rs` |

## Two layers of cache

1. **Workspace tables** (SQLite, `workspace.db`), kept across restarts:
   - `meta_catalogs`: catalogs of three-level engines, with `schemas_at` when that catalog's schema list is complete.
   - `meta_schemas`: schema list (`pos`; `pos = 1000000000` marks a schema seen only through a search hit), plus `listed_at` (a complete table listing exists) and `listed_fp` (the schema fingerprint it was listed at).
   - `meta_objects`: tables, views, routines, sequences per schema, with `version` (from `object_versions`) where the engine has one.
   - `meta_columns`: `columns_json` per table, `fks_json` (its foreign keys; NULL when not known, e.g. rows cached before v19 or written by `meta_put_columns`) and a `stale` flag. No indexes or DDL.
   - `meta_conn_state`: `checked_at`, `catalogs_at`, `schemas_at` and `identity` (which server the connection pointed at).
2. **Zustand store** (`explorerSlice.ts`), for the session: `catalogs`, `catalogSchemas`, `schemas`, `objects` (`conn|schema`), `columns` (`conn|schema|table`) and `explorer[conn]` (`checkedAt`, `checking`, `offline`, `fingerprints`, `deferred`). Once a key is in the store, `loadObjects` and `loadColumns` return it without asking the backend again.

Writes after a live listing or describe go through `MetaWriter` (`state.meta.push` / `push_blocking`): one queue, run in order, so the UI gets the server's answer before the disk write. Code that reads the cache to compare against it calls `state.meta.flush()` first (`refresh_schema_at`, `revalidate_explorer`, `clear_explorer_cache`, lineage).

## What happens when

```
expand connection ──► openExplorer
   1. cached_explorer           (SQLite only)  → tree shows at once
   2. list_catalogs / list_schemas (live)      → replaces top level; meta_put_* queued
        └─ fails and a cache exists → explorer[conn].offline = message, stop
   3. background:
        revalidate_explorer(open schemas)
            explorer_fingerprints (1 query) vs meta_schemas.listed_fp
            changed schemas → refresh_schema_at (≤ 40; rest "deferred")
        loadCatalogSchemas (default catalog + open ones, ≤ 4)
        prefetch up to 8 uncached schemas, one at a time
        exploreChosen (every schema picked in "Choose schemas")

expand schema ──► loadObjects
   in store → done
   cached_objects hit → show; if engine has no fingerprints or schema was
                        deferred → refresh_schema in background
   miss → list_objects (live); queued: object_versions + meta_put_listing

expand table ──► loadColumns
   in store → done
   cached_columns hit → show; if stale and online → describe in background
   miss → describe (live); meta_put_columns queued (clears stale)
```

- **Connect.** There is no separate connect step for the explorer: the first engine call opens the connection's metadata session (`with_meta`, tab key `__meta__` in `crates/query-engine/src/sessions.rs`). A failed call with a connection error reconnects once.
- **Refresh on the connection** (`openExplorer(conn, true)`): `api.reconnect` first (closes the metadata session and idle tab sessions, so rights granted since login show), then no cache read, a live top level and `revalidate_explorer(force = true)`, which ignores the 60 s `RECHECK_MS` guard.
- **Refresh on a schema** (`loadObjects(conn, schema, true)`): `refresh_schema`, always against the server. In-memory columns of that schema are dropped unless the result is `unchanged`.
- **Search and completion hits** (`search_objects`, `complete_tables`, `complete_routines`) are added to `meta_objects` (`meta_add_objects`) but never make a schema "listed".
- **DuckDB `results.main`** (query outputs) is never cached (`cacheable` in `api/explorer.rs`); `runsSlice.ts` reloads it when outputs change.
- **Offline.** If step 2 fails and a cache exists, the tree stays and `ConnectionNode.tsx` shows the offline badge. While `offline` is set, prefetch, `exploreChosen` and stale-column re-describes are skipped. It is cleared by the next successful `openExplorer`, and by `markOnline` when the server answers anything else: a statement finishes on that connection (`runsSlice.ts`), or a live listing, refresh or describe comes back. `markOnline` then re-runs `openExplorer` to check what changed meanwhile. Without a cache the error goes to the user.

## Detecting stale entries

- **Fingerprints** (`Session::explorer_fingerprints`, default = `schema_fingerprints`): one catalog query, one opaque value per schema. Equal to `listed_fp` means the cached listing is right and nothing is read.
- **Object versions** (`Session::object_versions`): one value per object (`kind:name`). `refresh_schema_at` compares them to `meta_objects.version`, fetches only new or changed objects with `list_objects_named` (500 names per call), deletes gone ones and marks the changed tables' columns stale (`meta_apply_changes`). Above 30 % changed and more than 20 objects (`FULL_RELIST_SHARE`) one full listing is cheaper.
- **No versions:** a changed schema is listed again (`meta_put_listing`) and every table's columns become stale, unless the fingerprint is unchanged.
- **`stale` on columns** means "show, but describe again when online". It is set only by the listing paths above (`columns_changed`), and cleared by any describe that writes the cache (`api::describe`, grid edits in `query-engine/src/edit.rs`).
- **Deferred schemas:** changed schemas over the 40-per-run cap, schemas that failed to refresh, and, on engines without fingerprints, every listed schema that was not open. The UI refreshes a deferred schema when it is next loaded from the cache.

### Per engine

| Engine | Explorer fingerprint | Object versions | A changed schema is |
|---|---|---|---|
| PostgreSQL | own: relations, columns, constraints, comments, sequences, routines (`xmin`) | yes | refreshed per object |
| MySQL / MariaDB | index fingerprint + routines | yes | refreshed per object |
| SQL Server | own: `modify_date` + `MS_Description` | yes | refreshed per object |
| Oracle | own: `last_ddl_time` | yes | refreshed per object |
| Snowflake | index fingerprint (tables and columns) | no | listed again |
| Databricks | index fingerprint (Unity Catalog tables; none on Hive metastore) | no | listed again |
| DuckDB | index fingerprint (columns) | no | listed again |
| SQLite | `PRAGMA schema_version` | no | listed again |
| Redshift, BigQuery, ClickHouse, MongoDB, Neo4j | none | no | listed again when open at connect, otherwise when expanded |

## Invalidation

- **Schema or catalog gone** from a complete list: `meta_put_schemas` / `meta_put_catalogs` drop its objects and columns.
- **Table gone** from a listing: its columns are dropped (`columns_changed`).
- **Connection now points elsewhere** (kind, host, port, database, file, auth, options except session ones, SSH hop): `save_connection` → `meta_check_identity` → `meta_clear` and `kn_clear` (the AI index's objects; notes and learned joins stay). The connection form also drops the connection's keys from the UI store (`useConnectionForm.tsx` `persist`).
- **Connection deleted:** `delete_connection` removes both the explorer cache and the knowledge index.
- `clear_explorer_cache` exists as a command but the UI does not call it.

## Other readers of the explorer cache

| Feature | Reads |
|---|---|
| Editor completion (`ui/src/components/sqlAssist.ts`) | the store first; `complete_tables_local` (`kn_complete`: knowledge index UNION `meta_objects`); `complete_columns_local` (`kn_column_names` → `meta_table`: the newer of the two); `complete_routines_local` (`meta_objects`); then the server |
| Lineage (`crates/app/src/api/lineage.rs`) | `meta_get` for table names, `kn_column_names` for columns; with "Check with database" it first describes referenced tables whose cached columns are `stale`, then up to 20 unknown tables (writing the cache) |
| Grid edits (`crates/query-engine/src/edit.rs`) | `meta_object` and non-stale `meta_columns`, else a describe that it writes back |
| Test data (`crates/query-engine/src/testdata.rs`) | `cached_test_data_plan` → `meta_table` (columns and foreign keys), shown until the live `test_data_plan` answers |
| AI note targets, `resolve_table` (`crates/ai/src/knowledge.rs`) | `meta_resolve_path`, `meta_get` before the server |
| ER diagram (`ErTab.tsx` → `schema_tables`) | nothing: always live (`Session::schema_columns`), not cached |

## The AI knowledge index

The knowledge index (`kn_objects` with `columns_json` and `fks_json`, `kn_schema_state` with fingerprints, FTS5 `kn_fts`; `crates/workspace/src/knowledge.rs`, `crates/ai/src/knowledge.rs`) is a separate store with its own fetch path:

- It is filled only when the user (or the AI, with approval) indexes: `index_connection` reads `list_schemas`, `schema_fingerprints` and `bulk_metadata` on its own session (`index_tab`: `__index__` for server databases, the metadata session for local files); `index_tables` describes single tables. Neither writes the explorer cache, and explorer listings and describes never write the index.
- Both keep foreign keys (the explorer cache since v19, from `describe` and grid edits).
- Its staleness is per schema (`kn_schema_state` fingerprint, checked at the next index run); there is no per-table `stale` flag, and nothing re-checks it between runs.
- Readers of one table go through `Workspace::meta_table`: the entry read from the server last (`meta_columns.seen_at` vs `kn_objects.indexed_at`) wins, foreign keys it doesn't know come from the other, and `stale` comes from the explorer cache (the index can't tell). `kn_column_names` (completion, lineage, the AI's joins) and `cached_test_data_plan` use it. `kn_complete` still merges both lists of names.
- A connection identity change clears both.
