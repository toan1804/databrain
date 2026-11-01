//! Commands: the explorer (catalogs, schemas, objects, columns), search,
//! completion, DDL, schema compare, local files.

use super::*;

#[tauri::command]
pub(crate) async fn file_scan_sql(path: String, all_sheets: Option<bool>) -> R<String> {
    #[cfg(feature = "duckdb-engine")]
    return api::file_scan_sql(&path, all_sheets.unwrap_or(false));
    #[cfg(not(feature = "duckdb-engine"))]
    Err(databrain_query_engine::EngineError::new("invalid", format!("DuckDB support is not built in ({path})")))
}

#[tauri::command]
pub(crate) async fn excel_sheets(path: String) -> R<Vec<api::ExcelSheet>> {
    #[cfg(feature = "duckdb-engine")]
    return api::excel_sheets(&path);
    #[cfg(not(feature = "duckdb-engine"))]
    Err(databrain_query_engine::EngineError::new("invalid", format!("DuckDB support is not built in ({path})")))
}

#[tauri::command]
pub(crate) async fn list_schemas(state: State<'_, AppState>, id: String) -> R<Vec<SchemaInfo>> {
    api::list_schemas(&state, &id).await
}

#[tauri::command]
pub(crate) async fn list_objects(state: State<'_, AppState>, id: String, schema: String) -> R<Vec<DbObject>> {
    api::list_objects(&state, &id, &schema).await
}

#[tauri::command]
pub(crate) async fn list_catalogs(state: State<'_, AppState>, id: String) -> R<Option<Vec<databrain_connector_core::CatalogInfo>>> {
    api::list_catalogs(&state, &id).await
}

#[tauri::command]
pub(crate) async fn list_catalog_schemas(state: State<'_, AppState>, id: String, catalog: String) -> R<Vec<SchemaInfo>> {
    api::list_catalog_schemas(&state, &id, &catalog).await
}

#[tauri::command]
pub(crate) fn cached_explorer(state: State<'_, AppState>, id: String) -> R<api::CachedExplorer> {
    api::cached_explorer(&state, &id)
}

#[tauri::command]
pub(crate) fn cached_objects(state: State<'_, AppState>, id: String, schema: String) -> R<Option<databrain_workspace::CachedListing>> {
    api::cached_objects(&state, &id, &schema)
}

#[tauri::command]
pub(crate) fn cached_columns(state: State<'_, AppState>, id: String, schema: String, name: String) -> R<Option<databrain_workspace::CachedColumns>> {
    api::cached_columns(&state, &id, &schema, &name)
}

#[tauri::command]
pub(crate) async fn clear_explorer_cache(state: State<'_, AppState>, id: String) -> R<()> {
    api::clear_explorer_cache(&state, &id).await
}

#[tauri::command]
pub(crate) async fn refresh_schema(state: State<'_, AppState>, id: String, schema: String) -> R<api::SchemaRefresh> {
    api::refresh_schema(&state, &id, &schema).await
}

#[tauri::command]
pub(crate) async fn revalidate_explorer(state: State<'_, AppState>, id: String, open: Vec<String>, force: bool) -> R<api::ExplorerRevalidation> {
    api::revalidate_explorer(&state, &id, &open, force).await
}

#[tauri::command]
pub(crate) fn complete_tables_local(state: State<'_, AppState>, id: String, schema: Option<String>, query: String, limit: usize) -> R<Vec<DbObject>> {
    api::complete_tables_local(&state, &id, schema.as_deref(), &query, limit)
}

#[tauri::command]
pub(crate) async fn complete_tables(state: State<'_, AppState>, id: String, schema: Option<String>, query: String, limit: usize) -> R<Vec<DbObject>> {
    api::complete_tables(&state, &id, schema.as_deref(), &query, limit).await
}

#[tauri::command]
pub(crate) fn complete_routines_local(state: State<'_, AppState>, id: String, schema: Option<String>, query: String, limit: usize) -> R<Vec<DbObject>> {
    api::complete_routines_local(&state, &id, schema.as_deref(), &query, limit)
}

#[tauri::command]
pub(crate) async fn complete_routines(state: State<'_, AppState>, id: String, schema: Option<String>, query: String, limit: usize) -> R<Vec<DbObject>> {
    api::complete_routines(&state, &id, schema.as_deref(), &query, limit).await
}

#[tauri::command]
pub(crate) async fn object_ddl(state: State<'_, AppState>, id: String, schema: String, name: String, kind: databrain_connector_core::ObjectKind) -> R<Option<String>> {
    api::object_ddl(&state, &id, &schema, &name, kind).await
}

#[tauri::command]
pub(crate) async fn package_members(state: State<'_, AppState>, id: String, schema: String, package: String) -> R<Vec<DbObject>> {
    api::package_members(&state, &id, &schema, &package).await
}

#[tauri::command]
pub(crate) fn complete_columns_local(state: State<'_, AppState>, id: String, schema: String, name: String) -> R<Option<Vec<String>>> {
    api::complete_columns_local(&state, &id, &schema, &name)
}

#[tauri::command]
pub(crate) async fn search_objects(state: State<'_, AppState>, id: String, query: String, limit: Option<usize>) -> R<Vec<DbObject>> {
    api::search_objects(&state, &id, &query, limit).await
}

#[tauri::command]
pub(crate) async fn describe_object(
    state: State<'_, AppState>,
    id: String,
    schema: String,
    name: String,
) -> R<ObjectDetail> {
    api::describe(&state, &id, &schema, &name).await
}

#[tauri::command]
pub(crate) async fn table_layout(state: State<'_, AppState>, id: String, schema: String, name: String) -> R<databrain_connector_core::TableLayout> {
    api::table_layout(&state, &id, &schema, &name).await
}

#[tauri::command]
pub(crate) async fn schema_diff(state: State<'_, AppState>, source: databrain_query_engine::schema_diff::SchemaSide, target: databrain_query_engine::schema_diff::SchemaSide) -> R<databrain_query_engine::schema_diff::SchemaDiff> {
    api::schema_diff(&state, &source, &target).await
}
