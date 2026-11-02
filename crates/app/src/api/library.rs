//! Saved queries, folders, history, notebooks, open tabs and settings.

use super::*;

// ------------------------------------------------------------ saved queries, history, tabs, settings

pub fn list_saved_queries(state: &AppState, search: Option<String>) -> Result<Vec<SavedQuery>> {
    Ok(state.workspace.list_saved_queries(search.as_deref())?)
}

pub fn save_query(state: &AppState, query: SavedQuery) -> Result<SavedQuery> {
    Ok(state.workspace.save_query(query)?)
}

pub fn delete_saved_query(state: &AppState, id: &str) -> Result<()> {
    Ok(state.workspace.delete_saved_query(id)?)
}

pub fn list_folders(state: &AppState, kind: FolderKind) -> Result<Vec<Folder>> {
    Ok(state.workspace.list_folders(kind)?)
}

pub fn save_folder(state: &AppState, folder: Folder) -> Result<Folder> {
    Ok(state.workspace.save_folder(folder)?)
}

pub fn delete_folder(state: &AppState, id: &str) -> Result<()> {
    Ok(state.workspace.delete_folder(id)?)
}

pub fn list_history(state: &AppState, query: HistoryQuery) -> Result<Vec<HistoryEntry>> {
    Ok(state.workspace.list_history(&query)?)
}

pub fn clear_history(state: &AppState, what: Option<databrain_workspace::HistoryClear>) -> Result<usize> {
    let (n, ids) = state.workspace.clear_history_results(what.unwrap_or(databrain_workspace::HistoryClear::All))?;
    drop_history_results(&state.engine, &ids);
    Ok(n)
}

/// Delete one history entry and the result it produced (unless pinned or shown in a tab).
pub fn delete_history_entry(state: &AppState, id: i64) -> Result<()> {
    if let Some(rid) = state.workspace.delete_history_entry(id)? {
        drop_history_results(&state.engine, &[rid]);
    }
    Ok(())
}

/// Results of deleted history entries go too: their outputs (memory and
/// saved snapshots), except pinned ones and those still shown in a tab.
pub fn drop_history_results(engine: &QueryEngine, result_ids: &[String]) -> usize {
    let outs = engine.outputs();
    let mut n = 0;
    for id in result_ids {
        match outs.by_result(id) {
            Some(o) if !o.pinned && !o.active => n += usize::from(outs.remove(&o.handle).is_ok()),
            Some(_) => {}
            // Not an output (any more): just free the stored rows.
            None => n += usize::from(engine.results().remove(id)),
        }
    }
    if n > 0 {
        engine.outputs_changed();
    }
    n
}

pub fn list_notebooks(state: &AppState) -> Result<Vec<databrain_workspace::NotebookSummary>> {
    Ok(state.workspace.list_notebooks()?)
}

pub fn get_notebook(state: &AppState, id: &str) -> Result<databrain_workspace::Notebook> {
    Ok(state.workspace.get_notebook(id)?)
}

pub fn save_notebook(state: &AppState, notebook: databrain_workspace::Notebook) -> Result<databrain_workspace::Notebook> {
    Ok(state.workspace.save_notebook(notebook)?)
}

/// Deleting a notebook also releases its per-cell sessions/results.
pub fn delete_notebook(state: &AppState, id: &str) -> Result<()> {
    if let Ok(nb) = state.workspace.get_notebook(id) {
        for c in nb.cells {
            state.engine.close_tab(&format!("nb:{id}:{}", c.id));
        }
    }
    state.engine.close_tab(&format!("nb:{id}"));
    Ok(state.workspace.delete_notebook(id)?)
}

pub fn load_tabs(state: &AppState) -> Result<Vec<TabState>> {
    Ok(state.workspace.list_tabs()?)
}

pub fn save_tabs(state: &AppState, tabs: Vec<TabState>) -> Result<()> {
    Ok(state.workspace.save_tabs(&tabs)?)
}

pub fn get_settings(state: &AppState) -> Result<serde_json::Map<String, serde_json::Value>> {
    Ok(state.workspace.all_settings()?)
}

pub fn set_setting(state: &AppState, key: &str, value: serde_json::Value) -> Result<()> {
    if key.is_empty() || key.len() > 100 {
        return Err(invalid("invalid setting key"));
    }
    if crate::paths::PROTECTED_SETTINGS.contains(&key) {
        return Err(invalid(format!("the setting {key} is changed in Settings, not directly")));
    }
    Ok(state.workspace.set_setting(key, &value)?)
}
