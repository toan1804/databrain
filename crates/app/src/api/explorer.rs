//! The explorer: catalogs, schemas, objects and columns (cached, refreshed
//! incrementally), search, completion, DDL, describe, schema compare, local files.

use super::*;

// ------------------------------------------------------------ explorer

pub async fn list_schemas(state: &AppState, id: &str) -> Result<Vec<SchemaInfo>> {
    let schemas = state.engine.list_schemas(id).await?;
    let (ws, id2, s2) = (state.workspace.clone(), id.to_string(), schemas.clone());
    state.meta.push_blocking(move || drop(ws.meta_put_schemas(&id2, None, &s2)));
    Ok(schemas)
}

/// Catalogs of three-level engines (`None` for two-level ones), listed live.
pub async fn list_catalogs(state: &AppState, id: &str) -> Result<Option<Vec<databrain_connector_core::CatalogInfo>>> {
    let cats = state.engine.list_catalogs(id).await?;
    if let Some(c) = cats.clone() {
        let (ws, id2) = (state.workspace.clone(), id.to_string());
        state.meta.push_blocking(move || drop(ws.meta_put_catalogs(&id2, &c)));
    }
    Ok(cats)
}

/// Schemas of one catalog, listed live.
pub async fn list_catalog_schemas(state: &AppState, id: &str, catalog: &str) -> Result<Vec<SchemaInfo>> {
    let schemas = state.engine.list_catalog_schemas(id, catalog).await?;
    let (ws, id2, cat, s2) = (state.workspace.clone(), id.to_string(), catalog.to_string(), schemas.clone());
    state.meta.push_blocking(move || drop(ws.meta_put_schemas(&id2, Some(&cat), &s2)));
    Ok(schemas)
}

/// DuckDB's `results.main` lists query outputs: always live, never cached.
fn cacheable(state: &AppState, id: &str, schema: &str) -> bool {
    schema != "results.main" || state.engine.workspace().get_connection(id).map(|p| p.config.kind != ConnectorKind::Duckdb).unwrap_or(true)
}

/// How long a queued listing waits for its object versions before it is
/// saved without them (the next refresh is then a full listing). The wait
/// holds later cache writes back, so it is bounded.
const VERSIONS_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// The listing goes to the explorer at once; the object versions (for the
/// next incremental refresh) are read meanwhile, outside the write queue,
/// and everything is saved afterwards, in order with other cache writes.
pub async fn list_objects(state: &AppState, id: &str, schema: &str) -> Result<Vec<DbObject>> {
    let objs = state.engine.list_objects(id, schema).await?;
    if cacheable(state, id, schema) {
        let (engine, id3, sc3) = (state.engine.clone(), id.to_string(), schema.to_string());
        let mut versions = tokio::spawn(async move { engine.object_versions(&id3, &sc3).await.ok().flatten() });
        let (ws, id2, sc, o2) = (state.workspace.clone(), id.to_string(), schema.to_string(), objs.clone());
        state.meta.push(async move {
            let got = match tokio::time::timeout(VERSIONS_WAIT, &mut versions).await {
                Ok(r) => r.ok().flatten(),
                Err(_) => {
                    versions.abort();
                    None
                }
            };
            let _ = tokio::task::spawn_blocking(move || ws.meta_put_listing(&id2, &sc, &o2, got.as_ref(), None)).await;
        });
    }
    Ok(objs)
}

/// Wait for queued explorer cache writes (tests, and before reading the cache to refresh it).
pub async fn flush_explorer_cache(state: &AppState) {
    state.meta.flush().await;
}

// --------------------------------------------- explorer cache (no network)

/// What the explorer shows before (or without) a connection.
#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct CachedExplorer {
    /// Catalogs (three-level engines), when listed before.
    pub catalogs: Option<Vec<databrain_connector_core::CatalogInfo>>,
    /// The complete schema list, when listed before.
    pub schemas: Option<Vec<SchemaInfo>>,
    /// Schema lists of single catalogs (three-level engines).
    #[ts(type = "Record<string, Array<SchemaInfo>>")]
    pub catalog_schemas: std::collections::BTreeMap<String, Vec<SchemaInfo>>,
    pub state: databrain_workspace::ExplorerCacheState,
}

pub fn cached_explorer(state: &AppState, id: &str) -> Result<CachedExplorer> {
    let ws = state.engine.workspace();
    let mut catalog_schemas = std::collections::BTreeMap::new();
    for cat in ws.meta_listed_catalogs(id)? {
        if let Some(list) = ws.meta_schemas(id, Some(&cat))? {
            catalog_schemas.insert(cat, list);
        }
    }
    Ok(CachedExplorer { catalogs: ws.meta_catalogs(id)?, schemas: ws.meta_schemas(id, None)?, catalog_schemas, state: ws.meta_state(id)? })
}

/// The cached listing of a schema (`None` = never listed).
pub fn cached_objects(state: &AppState, id: &str, schema: &str) -> Result<Option<databrain_workspace::CachedListing>> {
    if !cacheable(state, id, schema) {
        return Ok(None);
    }
    Ok(state.engine.workspace().meta_listing(id, schema)?)
}

/// Cached columns of a table (`stale` = describe again when online).
pub fn cached_columns(state: &AppState, id: &str, schema: &str, name: &str) -> Result<Option<databrain_workspace::CachedColumns>> {
    Ok(state.engine.workspace().meta_columns(id, schema, name)?)
}

/// Forget a connection's cached explorer (it is listed again from the server).
/// Queued after pending writes, so none of them puts old rows back.
pub async fn clear_explorer_cache(state: &AppState, id: &str) -> Result<()> {
    state.meta.flush().await;
    Ok(state.engine.workspace().meta_clear(id)?)
}

// --------------------------------------------- incremental refresh

/// Above this share of changed objects one full listing is cheaper than
/// fetching them by name.
const FULL_RELIST_SHARE: f64 = 0.3;

/// How a schema was brought up to date.
#[derive(Debug, Clone, Serialize, PartialEq, ts_rs::TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum RefreshMode {
    /// Nothing changed (versions equal).
    Unchanged,
    /// Only changed/new objects were fetched.
    Incremental,
    /// The whole schema was listed.
    Full,
}

#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct SchemaRefresh {
    pub schema: String,
    pub mode: RefreshMode,
    pub added: usize,
    pub changed: usize,
    pub removed: usize,
    /// The schema's objects now.
    pub objects: Vec<DbObject>,
}

/// Bring one schema's listing up to date, table by table when the engine
/// has object versions and the schema was listed before; otherwise list it.
pub async fn refresh_schema(state: &AppState, id: &str, schema: &str) -> Result<SchemaRefresh> {
    refresh_schema_at(state, id, schema, None).await
}

async fn refresh_schema_at(state: &AppState, id: &str, schema: &str, fingerprint: Option<&str>) -> Result<SchemaRefresh> {
    // Compare against the listings saved so far, not ones still queued.
    state.meta.flush().await;
    let ws = state.engine.workspace();
    let full =
        |objects: Vec<DbObject>, n: usize| SchemaRefresh { schema: schema.to_string(), mode: RefreshMode::Full, added: n, changed: 0, removed: 0, objects };
    if !cacheable(state, id, schema) {
        let objs = state.engine.list_objects(id, schema).await?;
        let n = objs.len();
        return Ok(full(objs, n));
    }
    let old = ws.meta_versions(id, schema)?;
    let listed = ws.meta_listing(id, schema)?;
    let new = state.engine.object_versions(id, schema).await?;
    let (Some(new), Some(listed)) = (new.clone(), listed.filter(|_| !old.is_empty())) else {
        // First listing, or no versions: list everything.
        let objs = state.engine.list_objects(id, schema).await?;
        ws.meta_put_listing(id, schema, &objs, new.as_ref(), fingerprint)?;
        let n = objs.len();
        return Ok(full(objs, n));
    };
    // Objects listed but without a version (added by a search) count as changed.
    let cached_keys: std::collections::HashSet<String> = listed.objects.iter().map(|o| databrain_connector_core::version_key(o.kind, &o.name)).collect();
    let removed: Vec<String> = cached_keys.iter().filter(|k| !new.contains_key(*k)).cloned().collect();
    let mut added = 0;
    let mut touched: Vec<String> = Vec::new();
    for (k, v) in &new {
        let is_new = !cached_keys.contains(k);
        if is_new || old.get(k) != Some(v) {
            added += usize::from(is_new);
            if let Some((_, name)) = k.split_once(':') {
                touched.push(name.to_string());
            }
        }
    }
    touched.sort();
    touched.dedup();
    let changed = touched.len().saturating_sub(added);
    if touched.is_empty() && removed.is_empty() {
        if let Some(fp) = fingerprint {
            ws.meta_set_listed_fp(id, schema, fp)?;
        }
        return Ok(SchemaRefresh { schema: schema.into(), mode: RefreshMode::Unchanged, added: 0, changed: 0, removed: 0, objects: listed.objects });
    }
    if touched.len() as f64 > (new.len().max(1) as f64) * FULL_RELIST_SHARE && touched.len() > 20 {
        let objs = state.engine.list_objects(id, schema).await?;
        ws.meta_put_listing(id, schema, &objs, Some(&new), fingerprint)?;
        return Ok(SchemaRefresh { schema: schema.into(), mode: RefreshMode::Full, added, changed, removed: removed.len(), objects: objs });
    }
    let mut upserts = Vec::new();
    for chunk in touched.chunks(500) {
        upserts.extend(state.engine.list_objects_named(id, schema, chunk).await?);
    }
    // Only rows whose version moved (a name may cover a table and a function).
    upserts.retain(|o| {
        let k = databrain_connector_core::version_key(o.kind, &o.name);
        !cached_keys.contains(&k) || old.get(&k) != new.get(&k)
    });
    ws.meta_apply_changes(id, schema, &upserts, &removed, &new, fingerprint)?;
    let objects = ws.meta_listing(id, schema)?.map(|l| l.objects).unwrap_or_default();
    Ok(SchemaRefresh { schema: schema.into(), mode: RefreshMode::Incremental, added, changed, removed: removed.len(), objects })
}

/// Result of [`revalidate_explorer`].
#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct ExplorerRevalidation {
    /// Schemas whose cached listing was updated (reload them from the cache).
    pub refreshed: Vec<SchemaRefresh>,
    /// Listed schemas confirmed unchanged by their fingerprint.
    pub unchanged: usize,
    /// Listed schemas left for later (not open, or over the per-run cap).
    pub deferred: Vec<String>,
    /// `false` when the engine has no fingerprints (only `open` schemas were refreshed).
    pub fingerprints: bool,
    /// Skipped because it was checked moments ago.
    pub skipped: bool,
}

/// At most this many changed schemas are refreshed per check; the rest when opened.
const REVALIDATE_CAP: usize = 40;

/// A check this recent is not repeated (unless forced).
const RECHECK_MS: i64 = 60_000;

/// After (re)connecting: one fingerprint query tells which cached schemas
/// changed; those are refreshed table by table ([`refresh_schema`]),
/// unchanged ones are not read at all. `open` = schemas the explorer shows
/// expanded (refreshed first; the only ones when there are no fingerprints).
pub async fn revalidate_explorer(state: &AppState, id: &str, open: &[String], force: bool) -> Result<ExplorerRevalidation> {
    state.meta.flush().await;
    let ws = state.engine.workspace();
    let st = ws.meta_state(id)?;
    let mut out = ExplorerRevalidation { refreshed: vec![], unchanged: 0, deferred: vec![], fingerprints: true, skipped: false };
    if !force && st.checked_at.is_some_and(|t| databrain_workspace::now_ms() - t < RECHECK_MS) {
        out.skipped = true;
        return Ok(out);
    }
    let listed = ws.meta_listed_schemas(id)?;
    let fps = state.engine.explorer_fingerprints(id).await?;
    ws.meta_touch_checked(id)?;
    let open: std::collections::HashSet<&str> = open.iter().map(String::as_str).collect();
    // Open schemas first.
    let mut todo: Vec<(String, Option<String>)> = Vec::new();
    for (schema, listed_fp) in listed {
        if !cacheable(state, id, &schema) {
            continue;
        }
        match &fps {
            Some(f) => {
                // A schema with no row may just have no tables on some engines: treat as a change.
                let now = f.get(&schema).cloned().unwrap_or_default();
                if listed_fp.as_deref() == Some(now.as_str()) {
                    out.unchanged += 1;
                } else {
                    todo.push((schema, Some(now)));
                }
            }
            None if open.contains(schema.as_str()) => todo.push((schema, None)),
            None => out.deferred.push(schema),
        }
    }
    out.fingerprints = fps.is_some();
    todo.sort_by_key(|(s, _)| !open.contains(s.as_str()));
    for (i, (schema, fp)) in todo.into_iter().enumerate() {
        if i >= REVALIDATE_CAP {
            out.deferred.push(schema);
            continue;
        }
        match refresh_schema_at(state, id, &schema, fp.as_deref()).await {
            Ok(r) => out.refreshed.push(r),
            // E.g. dropped meanwhile: the schema list refresh removes it.
            Err(_) => out.deferred.push(schema),
        }
    }
    Ok(out)
}

/// Catalog search: tables and views whose name contains `query`
/// (`schema.part` narrows by schema).
pub async fn search_objects(state: &AppState, id: &str, query: &str, limit: Option<usize>) -> Result<Vec<DbObject>> {
    if query.trim().is_empty() {
        return Ok(vec![]);
    }
    let hits = state.engine.search_objects(id, query, limit.unwrap_or(100)).await?;
    save_hits(state, id, &hits);
    Ok(hits)
}

/// Objects found by a search or completion: added to the cache in the background.
fn save_hits(state: &AppState, id: &str, hits: &[DbObject]) {
    if hits.is_empty() {
        return;
    }
    let (ws, id2, h) = (state.workspace.clone(), id.to_string(), hits.to_vec());
    state.meta.push_blocking(move || drop(ws.meta_add_objects(&id2, &h)));
}

/// Completion, local part: tables from the knowledge index (no network, instant).
pub fn complete_tables_local(state: &AppState, id: &str, schema: Option<&str>, query: &str, limit: usize) -> Result<Vec<DbObject>> {
    let rows = state.engine.workspace().kn_complete(id, schema, query, limit.clamp(1, 500))?;
    Ok(rows
        .into_iter()
        .filter_map(|(schema, name, kind, comment)| {
            let kind: databrain_connector_core::ObjectKind =
                serde_json::from_value(serde_json::Value::String(kind)).unwrap_or(databrain_connector_core::ObjectKind::Table);
            kind.is_relation().then_some(DbObject { schema, name, kind, comment, row_estimate: None })
        })
        .collect())
}

/// Completion, remote part: filtered on the server (schema-scoped when given),
/// so big catalogs are never loaded whole.
pub async fn complete_tables(state: &AppState, id: &str, schema: Option<&str>, query: &str, limit: usize) -> Result<Vec<DbObject>> {
    let hits = match schema {
        Some(s) => state.engine.search_schema(id, s, query, limit).await?,
        None if query.trim().is_empty() => vec![],
        None => state.engine.search_objects(id, query, limit).await?,
    };
    save_hits(state, id, &hits);
    Ok(hits)
}

/// Completion of functions/procedures/packages, local part (explorer and earlier lookups).
pub fn complete_routines_local(state: &AppState, id: &str, schema: Option<&str>, query: &str, limit: usize) -> Result<Vec<DbObject>> {
    Ok(state.engine.workspace().meta_complete_routines(id, schema, query, limit.clamp(1, 500))?)
}

/// Completion of functions/procedures/packages, filtered on the server.
pub async fn complete_routines(state: &AppState, id: &str, schema: Option<&str>, query: &str, limit: usize) -> Result<Vec<DbObject>> {
    let hits = state.engine.search_routines(id, schema, query, limit).await?;
    let _ = state.engine.workspace().meta_add_objects(id, &hits);
    Ok(hits)
}

/// DDL of an object for the explorer's "Show DDL".
pub async fn object_ddl(state: &AppState, id: &str, schema: &str, name: &str, kind: databrain_connector_core::ObjectKind) -> Result<Option<String>> {
    state.engine.object_ddl(id, schema, name, kind).await
}

/// Functions/procedures of an Oracle package (`pkg.` completion).
pub async fn package_members(state: &AppState, id: &str, schema: &str, package: &str) -> Result<Vec<DbObject>> {
    state.engine.package_members(id, schema, package).await
}

/// Column names of a table from the knowledge index (no network).
pub fn complete_columns_local(state: &AppState, id: &str, schema: &str, name: &str) -> Result<Option<Vec<String>>> {
    Ok(state.engine.workspace().kn_column_names(id, schema, name)?)
}

pub async fn describe(state: &AppState, id: &str, schema: &str, name: &str) -> Result<ObjectDetail> {
    let d = state.engine.describe(id, schema, name).await?;
    if !d.columns.is_empty() {
        let (ws, id2, s2, n2, cols, fks) =
            (state.workspace.clone(), id.to_string(), schema.to_string(), name.to_string(), d.columns.clone(), d.foreign_keys.clone());
        state.meta.push_blocking(move || drop(ws.meta_put_table(&id2, &s2, &n2, &cols, Some(&fks))));
    }
    Ok(d)
}

/// Every table of a schema with its columns and foreign keys (ER diagram).
pub async fn schema_tables(state: &AppState, id: &str, schema: &str) -> Result<Vec<databrain_connector_core::TableColumns>> {
    let mut tables = state.engine.schema_columns(id, schema).await?;
    tables.sort_by(|a, b| a.table.cmp(&b.table));
    Ok(tables)
}

pub async fn table_layout(state: &AppState, id: &str, schema: &str, name: &str) -> Result<databrain_connector_core::TableLayout> {
    state.engine.table_layout(id, schema, name).await
}

/// Compare two schemas; the script makes `target` like `source` (not run).
pub async fn schema_diff(
    state: &AppState,
    source: &databrain_query_engine::schema_diff::SchemaSide,
    target: &databrain_query_engine::schema_diff::SchemaSide,
) -> Result<databrain_query_engine::schema_diff::SchemaDiff> {
    state.engine.schema_diff(source, target).await
}

/// DuckDB SQL that reads a local file or folder (CSV, Parquet, JSON, Excel,
/// Delta, Iceberg), e.g. `SELECT * FROM read_parquet('/x/a.parquet')`.
/// Excel: the first sheet, or one statement per non-empty sheet with `all_sheets`.
#[cfg(feature = "duckdb-engine")]
pub fn file_scan_sql(path: &str, all_sheets: bool) -> Result<String> {
    file_scan_sql_with(path, all_sheets, true)
}

/// [`file_scan_sql`]; without `preview`, the statements read every row (no
/// `LIMIT`), for an import.
#[cfg(feature = "duckdb-engine")]
pub fn file_scan_sql_with(path: &str, all_sheets: bool, preview: bool) -> Result<String> {
    let limit = if preview { "\nLIMIT 1000;" } else { ";" };
    use databrain_connector_duckdb::{FileFormat, detect_format, excel_sources, file_scan_expr};
    let fmt = detect_format(path)
        .ok_or_else(|| EngineError::new("invalid", "unsupported file type (use .csv/.tsv/.parquet/.json/.ndjson/.xlsx or a Delta/Iceberg folder)"))?;
    if fmt == FileFormat::Excel {
        let srcs = excel_sources(path, all_sheets).map_err(|e| EngineError::new("invalid", e.to_string()))?;
        if srcs.is_empty() {
            return Err(EngineError::new("invalid", "every sheet in this workbook is empty"));
        }
        let many = srcs.len() > 1;
        return Ok(srcs
            .into_iter()
            .map(|s| {
                let mut label = if many { format!("-- Sheet: {}\n", s.sheet) } else { String::new() };
                if let Some(l) = &s.left_out {
                    label.push_str(&format!("-- The table {l}: title rows above it and cells far from it are left out\n"));
                }
                format!("{label}SELECT *\nFROM {}{limit}", s.sql)
            })
            .collect::<Vec<_>>()
            .join("\n\n"));
    }
    Ok(format!("SELECT *\nFROM {}{limit}", file_scan_expr(path, fmt)))
}

#[derive(Debug, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct ExcelSheet {
    pub name: String,
    pub range: Option<String>,
    pub hidden: bool,
}

/// Sheets of an .xlsx file (workbook order) with their used ranges.
#[cfg(feature = "duckdb-engine")]
pub fn excel_sheets(path: &str) -> Result<Vec<ExcelSheet>> {
    Ok(databrain_connector_duckdb::xlsx::sheets(path)
        .map_err(|e| EngineError::new("invalid", e))?
        .into_iter()
        .map(|s| ExcelSheet { name: s.name, range: s.range, hidden: s.hidden })
        .collect())
}
