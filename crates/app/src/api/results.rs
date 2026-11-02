//! Results: pages, find, stats, export and copy, editing rows, related
//! rows, loading a result into a table.

use super::*;

// ------------------------------------------------------------ results

pub fn result_info(state: &AppState, result_id: &str) -> Result<ResultInfo> {
    Ok(state.results().get(result_id)?.lock().info())
}

pub async fn fetch_page(state: &AppState, result_id: String, view: ViewSpec, offset: usize, limit: usize) -> Result<Page> {
    let rs = state.results().get(&result_id)?;
    blocking(move || Ok(rs.lock().page(&view, offset, limit.min(MAX_PAGE))?)).await
}

pub async fn find_in_result(
    state: &AppState,
    result_id: String,
    view: ViewSpec,
    query: String,
    columns: Option<Vec<usize>>,
    limit: usize,
    whole_word: bool,
) -> Result<FindResult> {
    let rs = state.results().get(&result_id)?;
    blocking(move || Ok(rs.lock().find_with(&view, &query, columns.as_deref(), limit.clamp(1, 100_000), whole_word)?)).await
}

/// Count / distinct / sum / min / max / avg of the selected cells.
pub async fn summarize_selection(
    state: &AppState,
    result_id: String,
    view: ViewSpec,
    columns: Vec<usize>,
    offset: usize,
    limit: usize,
) -> Result<databrain_result_store::SelectionSummary> {
    let rs = state.results().get(&result_id)?;
    blocking(move || Ok(rs.lock().summarize(&view, &columns, offset, limit.min(MAX_COPY_ROWS))?)).await
}

pub async fn column_stats(state: &AppState, result_id: String, view: ViewSpec, column: usize) -> Result<ColumnStats> {
    let rs = state.results().get(&result_id)?;
    blocking(move || Ok(rs.lock().column_stats(&view, column)?)).await
}

/// Export the view to a user-chosen file. Returns rows written.
pub async fn export_result(state: &AppState, result_id: String, view: ViewSpec, options: ExportOptions, path: String) -> Result<u64> {
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return Err(invalid("Export path must be absolute"));
    }
    let rs = state.results().get(&result_id)?;
    blocking(move || {
        let (schema, batches) = {
            let mut g = rs.lock();
            (g.schema(), g.view_batches(&view, 10_000)?)
        };
        write_export(&path, schema, &batches, options)
    })
    .await
}

/// UI event channel for the progress of full exports (`{export_id, phase, rows, bytes}`).
pub const EXPORT_EVENT: &str = "export-progress";

/// Run the query that produced a result again, without the row limit, and
/// write all its rows to `path` (grid filters and sorting don't apply).
/// Progress goes to the UI on [`EXPORT_EVENT`].
pub async fn export_full_result(state: &AppState, result_id: &str, options: ExportOptions, path: String, export_id: &str) -> Result<u64> {
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return Err(invalid("Export path must be absolute"));
    }
    let o = state.engine.outputs().by_result(result_id).ok_or_else(|| invalid("The query of this result is no longer known; run it again"))?;
    let ui = state.ui.clone();
    let id = export_id.to_string();
    let progress = move |p: databrain_query_engine::ExportProgress| {
        let mut v = serde_json::to_value(p).unwrap_or_default();
        v["export_id"] = serde_json::Value::String(id.clone());
        ui.emit(EXPORT_EVENT, v);
    };
    state.engine.export_query(export_id, &o.connection_id, Some(o.session()), &o.sql, &path, options, &progress).await
}

fn write_export(
    path: &Path,
    schema: databrain_connector_core::arrow::datatypes::SchemaRef,
    batches: &[databrain_connector_core::arrow::array::RecordBatch],
    options: ExportOptions,
) -> Result<u64> {
    // Written to a new temp file beside it and renamed (see FileWriter), so a
    // failed export never leaves a half-written file under the chosen name.
    export_file(path, schema, batches, options).map_err(|e| EngineError::new("invalid", format!("{}: {e}", path.display())))
}

/// Serialize rows `[offset, offset+limit)` of the view (optionally a subset of
/// columns) for the clipboard.
pub async fn copy_rows(state: &AppState, result_id: String, view: ViewSpec, offset: usize, limit: usize, options: ExportOptions) -> Result<String> {
    let rs = state.results().get(&result_id)?;
    blocking(move || {
        let (schema, batch) = {
            let mut g = rs.lock();
            let (b, _) = g.view_slice(&view, offset, limit.min(MAX_COPY_ROWS))?;
            (g.schema(), b)
        };
        export_to_string(schema, [&batch], options).map_err(|e| EngineError::new("internal", e.to_string()))
    })
    .await
}

pub fn release_result(state: &AppState, result_id: &str) -> bool {
    state.results().remove(result_id)
}

// ------------------------------------------------------------ editing results

fn edit_source(state: &AppState, result_id: &str) -> Result<(databrain_query_engine::OutputInfo, databrain_connector_core::arrow::datatypes::SchemaRef)> {
    let o = state.engine.outputs().by_result(result_id).ok_or_else(|| invalid("This result is no longer available; run the query again"))?;
    let schema = state.results().get(result_id)?.lock().schema();
    Ok((o, schema))
}

/// The table a result can be edited in (or why not).
pub async fn edit_target(state: &AppState, result_id: &str) -> Result<databrain_query_engine::edit::EditTarget> {
    let (o, schema) = edit_source(state, result_id)?;
    state.engine.edit_target(&o, &schema, true).await
}

/// The one table a result's query reads (per-table grid settings); `None`
/// for other queries and results that are gone.
pub fn result_source(state: &AppState, result_id: &str) -> Result<Option<databrain_query_engine::edit::ResultSource>> {
    let Some(o) = state.engine.outputs().by_result(result_id) else { return Ok(None) };
    let kind = state.workspace.get_connection(&o.connection_id)?.config.kind;
    Ok(databrain_query_engine::edit::result_source(&o, kind))
}

/// The SQL the pending changes run (built here, not taken from the UI).
pub fn edit_preview(
    state: &AppState,
    result_id: &str,
    target: &databrain_query_engine::edit::EditTarget,
    changes: &[databrain_query_engine::edit::RowChange],
) -> Result<Vec<databrain_query_engine::edit::EditStatement>> {
    let (_, schema) = edit_source(state, result_id)?;
    state.engine.edit_statements(target, &schema, changes)
}

/// Save the pending changes on the session the result came from.
pub async fn apply_edits(
    state: &AppState,
    result_id: &str,
    changes: Vec<databrain_query_engine::edit::RowChange>,
) -> Result<databrain_query_engine::edit::ApplyReport> {
    let (o, schema) = edit_source(state, result_id)?;
    // The target again (not the UI's copy), read from the database: the key
    // and columns it allows now.
    let target = state.engine.edit_target(&o, &schema, false).await?;
    let stmts = state.engine.edit_statements(&target, &schema, &changes)?;
    state.engine.apply_edits(&target, o.session(), &stmts).await
}

/// Foreign keys from and to the table a result was read from.
pub async fn result_links(state: &AppState, result_id: &str) -> Result<Vec<databrain_query_engine::related::ResultLink>> {
    let (o, schema) = edit_source(state, result_id)?;
    state.engine.result_links(&o, &schema).await
}

/// The query for the rows a link leads to from the given rows' values.
pub fn related_rows_sql(state: &AppState, result_id: &str, link: &databrain_query_engine::related::ResultLink, rows: &[Vec<Option<String>>]) -> Result<String> {
    let (o, schema) = edit_source(state, result_id)?;
    state.engine.related_rows_sql(&o.connection_id, &schema, link, rows)
}

/// Where and how to load a result's rows ("Load into…" on the grid).
#[derive(Debug, Clone, Deserialize, ts_rs::TS)]
#[ts(export)]
#[ts(optional_fields = nullable)]
pub struct LoadResultArgs {
    pub target_connection_id: String,
    pub table: String,
    #[serde(default)]
    pub mode: databrain_workspace::LoadMode,
    #[serde(default)]
    pub key_columns: Vec<String>,
    #[serde(default = "default_true")]
    pub create_table: bool,
}

fn default_true() -> bool {
    true
}

impl LoadResultArgs {
    fn spec(&self) -> databrain_query_engine::LoadSpec {
        databrain_query_engine::LoadSpec {
            table: self.table.trim().to_string(),
            mode: self.mode,
            key_columns: self.key_columns.clone(),
            create_table: self.create_table,
            ..Default::default()
        }
    }
}

/// UI event channel of loads from the grid (`{load_id, done, total}`).
pub const LOAD_EVENT: &str = "load-progress";

static LOADS: std::sync::LazyLock<parking_lot::Mutex<std::collections::HashMap<String, databrain_connector_core::CancellationToken>>> =
    std::sync::LazyLock::new(Default::default);

struct LoadProgress {
    ui: Arc<dyn crate::ai_api::UiBridge>,
    id: String,
}

impl databrain_query_engine::LoadObserver for LoadProgress {
    fn event(&self, e: databrain_query_engine::LoadEvent) {
        if let databrain_query_engine::LoadEvent::Progress { done, total, .. } = e {
            self.ui.emit(LOAD_EVENT, serde_json::json!({"load_id": self.id, "done": done, "total": total}));
        }
    }
}

/// Copy the rows of a result (what was fetched) into a table of a
/// connection, with the job load code: append / truncate / replace /
/// update / merge, creating a missing table.
pub async fn load_result(state: &AppState, result_id: &str, args: LoadResultArgs, load_id: &str) -> Result<databrain_query_engine::LoadReport> {
    let o = state.engine.outputs().by_result(result_id).ok_or_else(|| invalid("This result is no longer available; run the query again"))?;
    let target = state.workspace.get_connection(&args.target_connection_id)?;
    if target.config.read_only {
        return Err(invalid(format!("Connection \"{}\" is read-only", target.name)));
    }
    let cancel = databrain_connector_core::CancellationToken::new();
    LOADS.lock().insert(load_id.to_string(), cancel.clone());
    let tab = format!("__load__:{load_id}");
    let obs = LoadProgress { ui: state.ui.clone(), id: load_id.to_string() };
    let r = state.engine.write_output(&o.handle, &args.target_connection_id, &tab, &args.spec(), &cancel, &obs).await;
    LOADS.lock().remove(load_id);
    state.engine.close_tab(&tab);
    r
}

/// Stop a running [`load_result`].
pub fn cancel_load(load_id: &str) -> bool {
    LOADS.lock().get(load_id).map(|c| c.cancel()).is_some()
}

/// Check a load of a result without keeping any change (its first 1,000 rows).
pub async fn dry_run_result_load(state: &AppState, result_id: &str, args: LoadResultArgs) -> Result<databrain_query_engine::LoadDryRun> {
    let rows = state.engine.result_rows(result_id)?;
    let mut left = 1000usize;
    let mut sample = Vec::new();
    for b in rows.batches {
        if left == 0 {
            break;
        }
        let n = b.num_rows().min(left);
        sample.push(b.slice(0, n));
        left -= n;
    }
    let rows = databrain_query_engine::LoadRows { schema: rows.schema, batches: sample };
    let tab = format!("__loadcheck__:{}", uuid::Uuid::new_v4());
    let r = state.engine.dry_run_load(Some(&rows), None, &args.target_connection_id, &tab, &args.spec()).await;
    state.engine.close_tab(&tab);
    Ok(r)
}

/// Which parts of a load target path exist (catalog, schema, table), as
/// "Load into…" and job load steps show it while the path is typed.
pub async fn check_load_target(state: &AppState, connection_id: &str, table: &str) -> Result<databrain_query_engine::LoadTarget> {
    state.engine.check_load_target(connection_id, table).await
}
