//! Outputs (`results.<name>`): list, load, rename, pin, drop, diff, chart.

use super::*;

// ------------------------------------------------------------ outputs

pub use databrain_query_engine::OutputInfo;

pub fn list_outputs(state: &AppState) -> Vec<OutputInfo> {
    state.engine.outputs().list()
}

/// Resolve `r12`, a name, `name__1`, `results.x` or a result id.
pub fn get_output(state: &AppState, reference: &str) -> Result<OutputInfo> {
    state.engine.outputs().resolve(reference).ok_or_else(|| EngineError::new("not_found", format!("output {reference} not found")))
}

/// Make sure the data is in memory (loads pinned snapshots) and return it.
pub async fn load_output(state: &AppState, reference: String) -> Result<OutputInfo> {
    let reg = state.engine.outputs().clone();
    let o = blocking(move || reg.ensure_loaded(&reference)).await?;
    state.engine.outputs_changed();
    Ok(o)
}

pub fn rename_output(state: &AppState, handle: &str, name: Option<String>) -> Result<OutputInfo> {
    let name = name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
    let o = state.engine.outputs().set_name(handle, name.as_deref())?;
    state.engine.outputs_changed();
    Ok(o)
}

/// Drop an output (data, saved snapshot and handle).
pub async fn drop_output(state: &AppState, reference: String) -> Result<OutputInfo> {
    let reg = state.engine.outputs().clone();
    let o = blocking(move || reg.remove(&reference)).await?;
    state.engine.outputs_changed();
    Ok(o)
}

/// Drop every output that is neither pinned nor a tab's latest result.
pub async fn drop_unpinned_outputs(state: &AppState) -> Result<usize> {
    let reg = state.engine.outputs().clone();
    let n = blocking(move || Ok(reg.remove_unpinned())).await?;
    state.engine.outputs_changed();
    Ok(n)
}

pub async fn pin_output(state: &AppState, handle: String, pinned: bool) -> Result<OutputInfo> {
    let reg = state.engine.outputs().clone();
    let o = blocking(move || reg.set_pinned(&handle, pinned)).await?;
    state.engine.outputs_changed();
    Ok(o)
}

/// Keep a result in its tab's results when the tab runs again, or let go.
pub async fn keep_output(state: &AppState, handle: String, kept: bool) -> Result<OutputInfo> {
    let o = state.engine.outputs().set_kept(&handle, kept)?;
    state.engine.outputs_changed();
    Ok(o)
}

/// DuckDB SQL comparing two outputs (rows added / removed / changed).
/// `mapping`: extra (before column, after column) pairs chosen by the user;
/// equal names and names equal ignoring case are matched automatically.
/// `exact`: `mapping` is the complete list of pairs (no automatic matching).
/// `options`: how values compare (types normalized by default, see
/// [`CompareOptions`](databrain_query_engine::outputs::CompareOptions)).
pub fn output_diff_sql(
    state: &AppState,
    before: &str,
    after: &str,
    keys: Vec<String>,
    mapping: Vec<(String, String)>,
    exact: bool,
    options: &databrain_query_engine::outputs::CompareOptions,
) -> Result<String> {
    let b = get_output(state, before)?;
    let a = get_output(state, after)?;
    databrain_query_engine::outputs::diff_sql_opts(&b, &a, &keys, &mapping, !exact, options)
}

/// The column pairs a compare would use, with their types and how each is
/// compared (for the compare dialog).
pub fn output_compare_pairs(
    state: &AppState,
    before: &str,
    after: &str,
    mapping: Vec<(String, String)>,
    exact: bool,
    options: &databrain_query_engine::outputs::CompareOptions,
) -> Result<Vec<databrain_query_engine::outputs::ComparePair>> {
    let b = get_output(state, before)?;
    let a = get_output(state, after)?;
    databrain_query_engine::outputs::compare_pairs(&b, &a, &mapping, !exact, options)
}

/// What a compare tab compares (the dialog's choices).
#[derive(Debug, Clone, Deserialize, ts_rs::TS)]
#[ts(export)]
pub struct CompareSpec {
    pub before: String,
    pub after: String,
    #[serde(default)]
    pub keys: Vec<String>,
    /// (before column, after column) pairs.
    #[serde(default)]
    pub mapping: Vec<(String, String)>,
    /// `mapping` is every pair (no automatic matching).
    #[serde(default)]
    pub exact: bool,
    #[serde(default)]
    pub options: databrain_query_engine::outputs::CompareOptions,
}

/// A compare's counts and its rows (a result to page through).
#[derive(Debug, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct CompareRun {
    pub summary: databrain_query_engine::outputs::CompareSummary,
    /// `_change` (`added`, `removed`, `changed`), the after output's columns,
    /// `<col> (before)` and `_changed` for changed rows.
    pub result: databrain_result_store::ResultInfo,
    /// The diff SQL (DuckDB, on the Results connection).
    pub sql: String,
    pub pairs: Vec<databrain_query_engine::outputs::ComparePair>,
    /// Key columns (after-side names).
    pub keys: Vec<String>,
    pub before: OutputInfo,
    pub after: OutputInfo,
}

/// Rows of differences kept for the compare view (counts are always exact).
pub const COMPARE_ROW_LIMIT: usize = 100_000;

/// Compare two outputs: the counts (one summary query) and the differing
/// rows, both run on the Results connection. `tab_id` groups the runs.
pub async fn compare_outputs(state: &AppState, spec: CompareSpec, tab_id: &str) -> Result<CompareRun> {
    use databrain_query_engine::outputs as o;
    let before = load_output(state, spec.before.clone()).await?;
    let after = load_output(state, spec.after.clone()).await?;
    if before.handle == after.handle {
        return Err(EngineError::new("invalid", "choose two different outputs"));
    }
    let auto = !spec.exact;
    let pairs = o::compare_pairs(&before, &after, &spec.mapping, auto, &spec.options)?;
    let keys = o::compare_keys(&before, &after, &spec.keys, &spec.mapping, auto, &spec.options)?;
    let (summary_sql, columns) = o::compare_summary_sql(&before, &after, &spec.keys, &spec.mapping, auto, &spec.options)?;
    let sql = o::diff_sql_opts(&before, &after, &spec.keys, &spec.mapping, auto, &spec.options)?;
    let conn = results_connection(state)?;
    let run = |sql: String, limit: Option<usize>| {
        let st = state.engine.clone();
        let hub = state.hub.clone();
        let req = RunRequest {
            connection_id: conn.clone(),
            tab_id: format!("compare:{tab_id}"),
            sql,
            base_offset: 0,
            row_limit: limit,
            confirmed: true,
            origin: Default::default(),
            session_key: None,
            output_name: None,
            params: Default::default(),
            output_masks: None,
        };
        async move {
            let out = st.run_and_wait(&hub, req, None).await?;
            let first = out.into_iter().next().ok_or_else(|| EngineError::new("internal", "the compare ran no statement"))?;
            if let Some(e) = first.error {
                return Err(e);
            }
            first.result.ok_or_else(|| EngineError::new("internal", "the compare returned no rows"))
        }
    };
    let s = run(summary_sql, Some(1)).await?;
    let row = {
        let rs = state.engine.results().get(&s.id).map_err(|e| EngineError::new("internal", e.to_string()))?;
        let page = rs.lock().page(&ViewSpec::default(), 0, 1).map_err(|e| EngineError::new("internal", e.to_string()))?;
        page.rows.into_iter().next().unwrap_or_default()
    };
    // The one-row summary is not an output anyone wants in the list.
    if let Some(h) = state.engine.outputs().by_result(&s.id).map(|x| x.handle) {
        let _ = drop_output(state, h).await;
    }
    let summary = o::CompareSummary::from_row(&row, &columns)?;
    let result = run(sql.clone(), Some(COMPARE_ROW_LIMIT)).await?;
    Ok(CompareRun { summary, result, sql, pairs, keys, before, after })
}

/// One side of a compare before it is an output: an output already there,
/// a table read from its connection, or a file read by DuckDB.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CompareSource {
    Output {
        reference: String,
    },
    /// `schema` is the explorer's schema id (`catalog.schema` on three-level engines).
    Table {
        connection_id: String,
        schema: String,
        table: String,
    },
    /// CSV, TSV, Parquet, JSON or Excel (`sheet`, else the first sheet with data).
    File {
        path: String,
        sheet: Option<String>,
    },
}

/// Rows read from a table or file for a compare.
pub const COMPARE_SOURCE_ROW_LIMIT: usize = 1_000_000;

/// The output a compare side stands for: an output as it is; a table or
/// file read now (at most [`COMPARE_SOURCE_ROW_LIMIT`] rows, `truncated`
/// when there were more). `side` (`before`/`after`) and `tab_id` group the
/// runs, so reading again replaces the previous read.
pub async fn compare_load_source(state: &AppState, source: CompareSource, tab_id: &str, side: &str) -> Result<OutputInfo> {
    let (connection_id, sql) = match &source {
        CompareSource::Output { reference } => return load_output(state, reference.clone()).await,
        CompareSource::Table { connection_id, schema, table } => {
            let kind = state.workspace.get_connection(connection_id)?.config.kind;
            if !kind.traits().language.is_sql() {
                return Err(EngineError::new("invalid", format!("comparing tables reads them with SQL; {} isn't a SQL database", kind.traits().name)));
            }
            (connection_id.clone(), databrain_connector_core::traits::preview_query(kind, schema, table, COMPARE_SOURCE_ROW_LIMIT))
        }
        CompareSource::File { path, sheet } => {
            let (path, sheet) = (path.clone(), sheet.clone());
            let scan = super::blocking(move || file_scan(&path, sheet.as_deref())).await?;
            (results_connection(state)?, format!("SELECT * FROM {scan} LIMIT {COMPARE_SOURCE_ROW_LIMIT}"))
        }
    };
    let out = state
        .engine
        .run_and_wait(
            &state.hub,
            RunRequest {
                connection_id,
                tab_id: format!("compare:{tab_id}:{side}"),
                sql,
                base_offset: 0,
                row_limit: Some(COMPARE_SOURCE_ROW_LIMIT),
                confirmed: true,
                origin: Default::default(),
                session_key: None,
                output_name: None,
                params: Default::default(),
                output_masks: None,
            },
            None,
        )
        .await?;
    let first = out.into_iter().next().ok_or_else(|| EngineError::new("internal", "nothing was read"))?;
    if let Some(e) = first.error {
        return Err(e);
    }
    let r = first.result.ok_or_else(|| EngineError::new("internal", "the read returned no rows"))?;
    state.engine.outputs().by_result(&r.id).ok_or_else(|| EngineError::new("internal", "the read made no output"))
}

/// DuckDB expression that reads a data file (an Excel sheet: `sheet`, else
/// the first with data). Only that sheet is read.
#[cfg(feature = "duckdb-engine")]
fn file_scan(path: &str, sheet: Option<&str>) -> Result<String> {
    use databrain_connector_duckdb::{FileFormat, detect_format, excel_sheet_source, file_scan_expr};
    let fmt = detect_format(path)
        .ok_or_else(|| EngineError::new("invalid", "unsupported file type (use .csv/.tsv/.parquet/.json/.ndjson/.xlsx or a Delta/Iceberg folder)"))?;
    if fmt != FileFormat::Excel {
        return Ok(file_scan_expr(path, fmt));
    }
    Ok(excel_sheet_source(path, sheet).map_err(|e| EngineError::new("invalid", e.to_string()))?.sql)
}

#[cfg(not(feature = "duckdb-engine"))]
fn file_scan(path: &str, _sheet: Option<&str>) -> Result<String> {
    Err(EngineError::new("invalid", format!("DuckDB support is not built in ({path})")))
}

/// Key columns to suggest for comparing two outputs: the first matched
/// column (id-like names first), else the first pair of them, whose values
/// are unique and not NULL on both sides. Empty when none is.
pub async fn compare_suggest_keys(state: &AppState, before: &str, after: &str, mapping: Vec<(String, String)>, exact: bool) -> Result<Vec<String>> {
    use databrain_query_engine::outputs as o;
    let b = load_output(state, before.to_string()).await?;
    let a = load_output(state, after.to_string()).await?;
    let pairs = o::column_pairs_with(&b, &a, &mapping, !exact)?;
    let cands = o::key_candidates(&pairs);
    if cands.is_empty() {
        return Ok(vec![]);
    }
    let sql = o::unique_check_sql(&b, &a, &pairs, &cands);
    let conn = results_connection(state)?;
    let out = state
        .engine
        .run_and_wait(
            &state.hub,
            RunRequest {
                connection_id: conn,
                tab_id: "compare:keys".into(),
                sql,
                base_offset: 0,
                row_limit: Some(1),
                confirmed: true,
                origin: Default::default(),
                session_key: None,
                output_name: None,
                params: Default::default(),
                output_masks: None,
            },
            None,
        )
        .await?;
    let first = out.into_iter().next().ok_or_else(|| EngineError::new("internal", "no answer"))?;
    if let Some(e) = first.error {
        return Err(e);
    }
    let r = first.result.ok_or_else(|| EngineError::new("internal", "no answer"))?;
    let row = {
        let rs = state.engine.results().get(&r.id).map_err(|e| EngineError::new("internal", e.to_string()))?;
        let page = rs.lock().page(&ViewSpec::default(), 0, 1).map_err(|e| EngineError::new("internal", e.to_string()))?;
        page.rows.into_iter().next().unwrap_or_default()
    };
    if let Some(h) = state.engine.outputs().by_result(&r.id).map(|x| x.handle) {
        let _ = drop_output(state, h).await;
    }
    Ok(o::pick_key(&cands, &row))
}

/// Id of the local DuckDB connection for `results.*` queries.
pub fn results_connection(state: &AppState) -> Result<String> {
    Ok(state.engine.results_connection()?.id)
}

pub async fn chart_data(
    state: &AppState,
    result_id: String,
    view: ViewSpec,
    spec: databrain_result_store::ChartSpec,
) -> Result<databrain_result_store::ChartData> {
    let rs = state.results().get(&result_id)?;
    blocking(move || Ok(databrain_result_store::with_window(&rs, &view, |g| g.chart(&view, &spec))?)).await
}

// ------------------------------------------------------------ results memory

/// Setting: the memory results may use, in MB (absent or null: automatic,
/// from the computer's memory).
pub const RESULTS_MEMORY_SETTING: &str = "results_memory_mb";

/// Smallest size that can be chosen.
const RESULTS_MEMORY_MIN_MB: u64 = 256;

/// The memory query results may use (Settings → Results).
#[derive(Debug, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct ResultsMemory {
    /// The budget in use, in bytes (one result keeps at most half).
    #[ts(type = "number")]
    pub budget: u64,
    /// What "Automatic" is on this computer, in bytes.
    #[ts(type = "number")]
    pub auto: u64,
    /// The size chosen in MB; None = automatic.
    #[ts(type = "number | null")]
    pub custom_mb: Option<u64>,
    /// The computer's memory in bytes (None when it can't be read).
    #[ts(type = "number | null")]
    pub total: Option<u64>,
    /// Bytes the outputs in memory use now.
    #[ts(type = "number")]
    pub used: u64,
}

fn custom_results_mb(ws: &databrain_workspace::Workspace) -> Option<u64> {
    ws.get_setting(RESULTS_MEMORY_SETTING).ok().flatten().and_then(|v| v.as_u64())
}

/// Memory DuckDB may use for its own work (scans, sorts, joins): what the
/// results budget and the free-memory floor leave, at most DuckDB's default
/// 80% and at least 512 MB; spilling to disk covers the rest.
pub fn duckdb_memory_limit(total: u64, budget: u64, floor: u64) -> u64 {
    total.saturating_sub(budget).saturating_sub(floor).min(total / 10 * 8).max(512 * 1024 * 1024)
}

/// Set the results budget from the setting (or the computer's memory), stop
/// loading a result when the computer runs low on free memory, and give
/// DuckDB the memory left over.
pub fn apply_results_memory(state: &AppState) {
    apply_results_memory_to(&state.engine)
}

/// [`apply_results_memory`] for an engine without the app (the MCP server).
pub fn apply_results_memory_to(engine: &databrain_query_engine::QueryEngine) {
    use databrain_query_engine::outputs::{auto_budget, free_floor};
    let total = crate::sysmem::total();
    let budget = custom_results_mb(engine.workspace()).map_or(auto_budget(total) as u64, |mb| mb * 1024 * 1024);
    let outputs = engine.outputs();
    outputs.set_budget(budget as usize);
    outputs.set_free_memory_probe(Some(Arc::new(crate::sysmem::available)), free_floor(total));
    #[cfg(feature = "duckdb-engine")]
    databrain_connector_duckdb::set_memory_limit(total.map(|t| duckdb_memory_limit(t, budget, free_floor(Some(t)))));
}

/// Results larger than memory continue on disk in `<app dir>/results_spill`
/// (this process's own folder), until the disk's free space drops below 5%
/// of it, between 1 and 4 GB (a fuller disk is common: a tenth of a 1 TB disk
/// would stop results with 90 GB free).
pub fn set_results_spill(engine: &databrain_query_engine::QueryEngine, app_dir: &std::path::Path) {
    let base = app_dir.join("results_spill");
    engine.results().set_spill_root(base);
    let total = crate::sysmem::disk_space(app_dir).map(|(_, t)| t);
    let floor = total.map_or(1 << 30, |t| (t / 20).clamp(1 << 30, 4 << 30));
    let at = app_dir.to_path_buf();
    let free: Arc<dyn Fn() -> Option<u64> + Send + Sync> = Arc::new(move || crate::sysmem::disk_space(&at).map(|(free, _)| free));
    engine.outputs().set_free_disk_probe(Some(free), floor);
}

/// Where DuckDB spills work bigger than its memory: `<app dir>/duckdb_spill`.
pub fn set_duckdb_spill_dir(app_dir: &std::path::Path) {
    #[cfg(feature = "duckdb-engine")]
    databrain_connector_duckdb::set_spill_dir(Some(app_dir.join("duckdb_spill").to_string_lossy().into_owned()));
    #[cfg(not(feature = "duckdb-engine"))]
    let _ = app_dir;
}

pub fn results_memory(state: &AppState) -> ResultsMemory {
    let total = crate::sysmem::total();
    let outputs = state.engine.outputs();
    ResultsMemory {
        budget: outputs.budget() as u64,
        auto: databrain_query_engine::outputs::auto_budget(total) as u64,
        custom_mb: custom_results_mb(&state.workspace),
        total,
        used: outputs.used() as u64,
    }
}

/// Choose the memory results may use (`mb`), or automatic (None). Lowering
/// it frees older results at once.
pub fn set_results_memory(state: &AppState, mb: Option<u64>) -> Result<ResultsMemory> {
    if let Some(mb) = mb {
        let max_mb = crate::sysmem::total().map(|t| t / (1024 * 1024));
        if mb < RESULTS_MEMORY_MIN_MB || max_mb.is_some_and(|m| mb > m) {
            return Err(EngineError::new(
                "invalid",
                match max_mb {
                    Some(m) => format!("Choose between {RESULTS_MEMORY_MIN_MB} MB and this computer's {m} MB"),
                    None => format!("Choose at least {RESULTS_MEMORY_MIN_MB} MB"),
                },
            ));
        }
    }
    state.workspace.set_setting(RESULTS_MEMORY_SETTING, &mb.map_or(serde_json::Value::Null, |m| serde_json::json!(m)))?;
    apply_results_memory(state);
    Ok(results_memory(state))
}
