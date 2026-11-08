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
    blocking(move || Ok(rs.lock().chart(&view, &spec)?)).await
}
