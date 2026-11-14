//! Commands: the AI assistant, providers, knowledge and the MCP setup.

use super::*;

// ---- sign-in
#[tauri::command]
pub(crate) async fn sign_in(state: State<'_, AppState>, id: String) -> R<databrain_auth::AuthStatus> {
    ai_api::sign_in(&state, &id).await
}

#[tauri::command]
pub(crate) async fn sign_out(state: State<'_, AppState>, id: String) -> R<()> {
    ai_api::sign_out(&state, &id).await
}

#[tauri::command]
pub(crate) async fn auth_status(state: State<'_, AppState>, id: String) -> R<databrain_auth::AuthStatus> {
    ai_api::auth_status(&state, &id)
}

#[tauri::command]
pub(crate) async fn cancel_sign_in(state: State<'_, AppState>) -> R<()> {
    ai_api::cancel_sign_in(&state);
    Ok(())
}

// ---- AI providers
#[tauri::command]
pub(crate) async fn ai_list_providers(state: State<'_, AppState>) -> R<Vec<ai_api::ProviderView>> {
    ai_api::list_providers(&state)
}

#[tauri::command]
pub(crate) async fn ai_save_provider(state: State<'_, AppState>, args: ai_api::SaveProviderArgs) -> R<databrain_workspace::AiProviderRecord> {
    ai_api::save_provider(&state, args)
}

#[tauri::command]
pub(crate) async fn ai_delete_provider(state: State<'_, AppState>, id: String) -> R<()> {
    ai_api::delete_provider(&state, &id)
}

#[tauri::command]
pub(crate) async fn ai_list_models(state: State<'_, AppState>, id: String) -> R<Vec<databrain_ai::ModelInfo>> {
    ai_api::list_models(&state, &id).await
}

#[tauri::command]
pub(crate) async fn ai_provider_sign_in(state: State<'_, AppState>, id: String) -> R<()> {
    ai_api::provider_sign_in(&state, &id).await
}

#[tauri::command]
pub(crate) async fn ai_provider_status(state: State<'_, AppState>, id: String) -> R<databrain_ai::kiro::KiroStatus> {
    ai_api::provider_status(&state, &id).await
}

#[tauri::command]
pub(crate) async fn ai_provider_usage(state: State<'_, AppState>, id: String, since_day: i64, since_month: i64) -> R<ai_api::ProviderUsage> {
    ai_api::provider_usage(&state, &id, since_day, since_month).await
}

#[tauri::command]
pub(crate) async fn terminal_test(setting: databrain_auth::terminal::TerminalSetting) -> R<databrain_auth::terminal::Launch> {
    Ok(ai_api::terminal_test(setting).await)
}

#[tauri::command]
pub(crate) async fn ai_provider_sign_out(state: State<'_, AppState>, id: String) -> R<()> {
    ai_api::provider_sign_out(&state, &id).await
}

// ---- AI agent
#[tauri::command]
pub(crate) async fn ai_send(state: State<'_, AppState>, args: ai_api::AiSendArgs) -> R<String> {
    ai_api::ai_send(&state, args)
}

#[tauri::command]
pub(crate) async fn ai_ghost(state: State<'_, AppState>, request_id: String, request: databrain_ai::ghost::GhostRequest) -> R<String> {
    ai_api::ai_ghost(&state, request_id, request).await
}

#[tauri::command]
pub(crate) async fn ai_cancel(state: State<'_, AppState>, run_id: String) -> R<bool> {
    Ok(ai_api::ai_cancel(&state, &run_id))
}

#[tauri::command]
pub(crate) async fn ai_respond(state: State<'_, AppState>, request_id: String, response: serde_json::Value) -> R<bool> {
    Ok(ai_api::ai_respond(&state, &request_id, response))
}

#[tauri::command]
pub(crate) async fn ai_sessions(state: State<'_, AppState>, connection_id: Option<String>) -> R<Vec<databrain_workspace::AiSessionRecord>> {
    ai_api::list_sessions(&state, connection_id)
}

#[tauri::command]
pub(crate) async fn ai_session_messages(state: State<'_, AppState>, session_id: String) -> R<Vec<databrain_workspace::AiMessageRecord>> {
    ai_api::session_messages(&state, &session_id)
}

#[tauri::command]
pub(crate) async fn ai_leave_session(state: State<'_, AppState>, id: String) -> R<()> {
    ai_api::leave_session(&state, &id);
    Ok(())
}

#[tauri::command]
pub(crate) async fn ai_delete_session(state: State<'_, AppState>, id: String) -> R<()> {
    ai_api::delete_session(&state, &id)
}

#[tauri::command]
pub(crate) async fn ai_audit(state: State<'_, AppState>) -> R<Vec<databrain_workspace::AuditEntry>> {
    ai_api::list_audit(&state)
}

#[tauri::command]
pub(crate) async fn mcp_config() -> R<serde_json::Value> {
    Ok(ai_api::mcp_config())
}

// ---- knowledge
#[tauri::command]
pub(crate) async fn kn_get(state: State<'_, AppState>, connection_id: String) -> R<ai_api::KnowledgeView> {
    ai_api::knowledge(&state, &connection_id)
}

#[tauri::command]
pub(crate) async fn kn_objects(state: State<'_, AppState>, connection_id: String, filter: String, offset: usize, limit: usize) -> R<ai_api::KnObjectPage> {
    ai_api::knowledge_objects(&state, &connection_id, &filter, offset, limit)
}

/// The AI scope's rules (what the AI may use on the connection).
#[tauri::command]
pub(crate) fn ai_scope(state: State<'_, AppState>, connection_id: String) -> R<Vec<databrain_workspace::ScopeRule>> {
    Ok(state.workspace.ai_scope(&connection_id)?)
}

/// Allow or exclude a schema or one table for the AI.
#[tauri::command]
pub(crate) fn ai_put_rule(state: State<'_, AppState>, connection_id: String, rule: databrain_workspace::ScopeRule) -> R<()> {
    Ok(state.workspace.ai_put_rule(&connection_id, &rule)?)
}

/// Remove one rule (a table goes back to its schema's rule).
#[tauri::command]
pub(crate) fn ai_delete_rule(state: State<'_, AppState>, connection_id: String, schema: String, table: String) -> R<()> {
    Ok(state.workspace.ai_delete_rule(&connection_id, &schema, &table)?)
}

#[tauri::command]
pub(crate) async fn kn_index(state: State<'_, AppState>, connection_id: String, scope: Option<Vec<String>>, batch: Option<u32>, full: Option<bool>) -> R<()> {
    ai_api::index_knowledge(&state, &connection_id, scope, batch, full.unwrap_or(false))
}

#[tauri::command]
pub(crate) async fn kn_plan(state: State<'_, AppState>, connection_id: String) -> R<databrain_ai::knowledge::IndexPlan> {
    ai_api::knowledge_plan(&state, &connection_id).await
}

#[tauri::command]
pub(crate) async fn kn_cancel(state: State<'_, AppState>, connection_id: String) -> R<bool> {
    Ok(ai_api::cancel_index(&state, &connection_id))
}

#[tauri::command]
pub(crate) async fn kn_learned(state: State<'_, AppState>, connection_id: String) -> R<ai_api::LearnedView> {
    ai_api::learned(&state, &connection_id)
}

#[tauri::command]
pub(crate) async fn kn_set_join_status(state: State<'_, AppState>, id: i64, status: databrain_workspace::JoinStatus) -> R<()> {
    ai_api::set_join_status(&state, id, status)
}

#[tauri::command]
pub(crate) async fn kn_review_join(state: State<'_, AppState>, id: i64, approve: bool) -> R<()> {
    ai_api::review_join(&state, id, approve)
}

#[tauri::command]
pub(crate) async fn kn_join_evidence(state: State<'_, AppState>, join_id: i64) -> R<Vec<databrain_workspace::HistoryEntry>> {
    ai_api::join_evidence(&state, join_id)
}

#[tauri::command]
pub(crate) async fn kn_learn_now(state: State<'_, AppState>, connection_id: String) -> R<databrain_ai::learn::LearnReport> {
    ai_api::relearn(&state, &connection_id).await
}

#[tauri::command]
pub(crate) async fn kn_forget_learned(state: State<'_, AppState>, connection_id: String) -> R<()> {
    ai_api::forget_learned(&state, &connection_id)
}

#[tauri::command]
pub(crate) async fn kn_save_note(state: State<'_, AppState>, note: databrain_workspace::KnNote) -> R<databrain_workspace::KnNote> {
    ai_api::save_note(&state, note).await
}

#[tauri::command]
pub(crate) async fn hint_layouts(state: State<'_, AppState>, connection_id: String, tables: Vec<String>) -> R<Vec<ai_api::HintLayout>> {
    ai_api::hint_layouts(&state, &connection_id, tables).await
}

#[tauri::command]
pub(crate) async fn kn_check_target(state: State<'_, AppState>, connection_id: String, target: String) -> R<ai_api::TargetCheck> {
    Ok(ai_api::check_target(&state, &connection_id, &target).await)
}

#[tauri::command]
pub(crate) async fn kn_delete_note(state: State<'_, AppState>, id: String) -> R<()> {
    ai_api::delete_note(&state, &id)
}

#[tauri::command]
pub(crate) async fn kn_export_notes(state: State<'_, AppState>, connection_id: String, path: String) -> R<usize> {
    ai_api::export_notes(&state, &connection_id, &paths::checked_str(&path, paths::Purpose::WriteJson)?)
}

#[tauri::command]
pub(crate) async fn kn_read_notes_file(state: State<'_, AppState>, connection_id: String, path: String) -> R<ai_api::NotesImportPreview> {
    ai_api::read_notes_file(&state, &connection_id, &paths::checked_str(&path, paths::Purpose::ReadJson)?).await
}

#[tauri::command]
pub(crate) async fn kn_import_notes(state: State<'_, AppState>, connection_id: String, actions: Vec<databrain_workspace::ImportAction>) -> R<usize> {
    ai_api::import_notes(&state, &connection_id, actions).await
}

#[tauri::command]
pub(crate) async fn kn_clear(state: State<'_, AppState>, connection_id: String) -> R<()> {
    ai_api::clear_knowledge(&state, &connection_id)
}
