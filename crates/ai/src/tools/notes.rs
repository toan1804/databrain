use databrain_workspace::{KnNote, NoteStatus};
use serde_json::{Value, json};

use super::{ToolContext, ToolOutput, arg, out, trunc};
use crate::types::{AiError, Result};
use crate::{access, knowledge};

impl ToolContext {
    pub(super) fn ai_note_status(&self) -> NoteStatus {
        if self.policy().auto_approve_notes { NoteStatus::Approved } else { NoteStatus::Proposed }
    }

    pub(super) fn note_saved_text(&self, status: NoteStatus, what: &str) -> String {
        match status {
            NoteStatus::Approved => format!("Note {what} and saved."),
            NoteStatus::Proposed => format!("Note {what}; the user will review it in Knowledge before it is used."),
        }
    }

    /// Validated, canonical target (tables must exist in this connection).
    pub(super) async fn checked_target(&self, target: Option<&str>) -> Result<Option<String>> {
        match target.map(str::trim).filter(|t| !t.is_empty()) {
            None => Ok(None),
            Some(t) => {
                // Only tables the AI may use: one answer for a table outside
                // its scope and one that doesn't exist.
                let tables = crate::catalog::AiTables::with_profile(self.engine.workspace(), &self.profile);
                let mut canonical = Vec::new();
                for (sep, p) in databrain_workspace::split_target(t) {
                    // Spelled out as the AI's scope resolved it: a bare
                    // `orders` must not be saved against another schema's.
                    let Some((o, col)) = tables.resolve_path(&p).ok().flatten() else {
                        return Err(AiError::Policy(format!("invalid target `{t}`: {}", knowledge::not_available(&p))));
                    };
                    canonical.push((sep, databrain_workspace::table_path(&o.schema, &o.name, col.as_deref())));
                }
                let t = databrain_workspace::join_target(&canonical);
                knowledge::check_note_target(&self.engine, &self.profile.id, &t).await.map(Some).map_err(|e| {
                    AiError::Policy(format!(
                        "invalid target `{t}`: {e}. Use real tables (schema.table, several joined with &), or omit target for a glossary note"
                    ))
                })
            }
        }
    }

    pub(super) async fn add_note(&self, args: &Value) -> Result<ToolOutput> {
        let note = arg(args, "note")?;
        let target = self.checked_target(args.get("target").and_then(|t| t.as_str())).await?;
        let ws = self.engine.workspace();
        // Same text already there: nothing to add.
        let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
        // Among the notes the AI may see: another one's id would name it.
        let tables = crate::catalog::AiTables::with_profile(ws, &self.profile);
        if let Some(dup) = ws.kn_notes(&self.profile.id)?.into_iter().find(|n| norm(&n.body) == norm(note) && tables.note_visible(n.target.as_deref())) {
            return Ok(out(format!("An identical note already exists (id {}).", dup.id), json!({"note": dup})));
        }
        let status = self.ai_note_status();
        let saved = ws.kn_save_note(KnNote {
            id: String::new(),
            connection_id: self.profile.id.clone(),
            target,
            body: note.to_string(),
            author: "ai".into(),
            status,
            created_at: 0,
            replaces: None,
        })?;
        self.audit("add_knowledge_note", args, status.as_str(), None);
        Ok(out(self.note_saved_text(status, "added"), json!({"note": saved})))
    }

    pub(super) async fn update_note(&self, args: &Value) -> Result<ToolOutput> {
        let id = arg(args, "id")?;
        let note = arg(args, "note")?;
        let ws = self.engine.workspace();
        let tables = crate::catalog::AiTables::with_profile(ws, &self.profile);
        let old = ws
            .kn_notes(&self.profile.id)?
            .into_iter()
            .find(|n| n.id == id && tables.note_visible(n.target.as_deref()))
            .ok_or_else(|| AiError::Policy(format!("note {id} not found; use list_knowledge_notes")))?;
        let target = match args.get("target").and_then(|t| t.as_str()) {
            Some(t) => self.checked_target(Some(t)).await?,
            None => old.target.clone(),
        };
        let status = self.ai_note_status();
        let saved = if old.status == NoteStatus::Proposed {
            // Revising its own pending proposal: edit it in place.
            ws.kn_save_note(KnNote { body: note.to_string(), target, ..old.clone() })?
        } else {
            ws.kn_save_note(KnNote {
                id: String::new(),
                connection_id: self.profile.id.clone(),
                target,
                body: note.to_string(),
                author: "ai".into(),
                status,
                created_at: 0,
                replaces: Some(old.id.clone()),
            })?
        };
        self.audit("update_knowledge_note", args, status.as_str(), None);
        Ok(out(self.note_saved_text(saved.status, "updated"), json!({"note": saved, "previous": old})))
    }

    /// A join written down somewhere (a document, the user), checked against
    /// the knowledge index: real tables the AI may read, real columns.
    pub(super) fn add_join(&self, args: &Value) -> Result<ToolOutput> {
        let ws = self.engine.workspace();
        let id = &self.profile.id;
        let source = arg(args, "source")?.trim();
        if source.is_empty() {
            return Err(AiError::Policy("say where the join is written (source), e.g. a document's name or 'the user'".into()));
        }
        let side = |t: &str, c: &str| -> Result<(String, Vec<String>)> {
            let table = arg(args, t)?;
            let o =
                crate::catalog::AiTables::with_profile(ws, &self.profile).get(table)?.ok_or_else(|| AiError::Policy(crate::knowledge::not_available(table)))?;
            let full = o.full_name();
            access::check_read_table(&self.profile, &full).map_err(AiError::Policy)?;
            let names: Vec<&str> = args.get(c).and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|v| v.as_str()).collect()).unwrap_or_default();
            if names.is_empty() {
                return Err(AiError::Policy(format!("{c} must list the join columns of {full}")));
            }
            let cols = names
                .iter()
                .map(|n| {
                    o.columns.iter().find(|x| x.name.eq_ignore_ascii_case(n.trim())).map(|x| x.name.clone()).ok_or_else(|| {
                        AiError::Policy(format!(
                            "{full} has no column `{n}` (columns: {})",
                            o.columns.iter().map(|x| x.name.as_str()).collect::<Vec<_>>().join(", ")
                        ))
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            Ok((full, cols))
        };
        let (lt, lc) = side("left_table", "left_columns")?;
        let (rt, rc) = side("right_table", "right_columns")?;
        if lc.len() != rc.len() {
            return Err(AiError::Policy("left_columns and right_columns must have the same number of columns, in matching order".into()));
        }
        if lt.eq_ignore_ascii_case(&rt) && lc == rc {
            return Err(AiError::Policy("a table joined to itself on the same columns is not a join".into()));
        }
        let approved = self.ai_note_status() == NoteStatus::Approved;
        let by = if self.caller == super::Caller::Mcp { "mcp" } else { "ai" };
        let (join_id, what) = ws.document_join(id, &lt, &lc, &rt, &rc, source, by, approved)?;
        let on: Vec<String> = lc.iter().zip(&rc).map(|(a, b)| format!("{lt}.{a} = {rt}.{b}")).collect();
        let on = on.join(" AND ");
        let text = match what {
            databrain_workspace::Documented::Rejected => {
                self.audit("add_knowledge_join", args, "refused", Some("rejected by the user"));
                return Ok(out(format!("The user rejected the join {on}; it was not recorded. Don't use it."), json!({"id": join_id, "rejected": true})));
            }
            databrain_workspace::Documented::Updated => format!("The join {on} was already recorded; its source is now: {source}."),
            databrain_workspace::Documented::Added if approved => {
                format!("Recorded the join {on} (from {source}); find_join_path and describe_table use it now.")
            }
            databrain_workspace::Documented::Added => {
                format!("Recorded the join {on} (from {source}); the user will review it in Knowledge before it is used.")
            }
        };
        self.audit("add_knowledge_join", args, if approved { "approved" } else { "proposed" }, None);
        Ok(out(text, json!({"id": join_id, "join": on, "approved": approved})))
    }

    pub(super) fn list_notes(&self, args: &Value) -> Result<ToolOutput> {
        let filter = args.get("filter").and_then(|f| f.as_str()).unwrap_or("").to_lowercase();
        let words: Vec<&str> = filter.split_whitespace().collect();
        let tables = crate::catalog::AiTables::with_profile(self.engine.workspace(), &self.profile);
        let notes: Vec<KnNote> = self
            .engine
            .workspace()
            .kn_notes(&self.profile.id)?
            .into_iter()
            .filter(|n| tables.note_visible(n.target.as_deref()))
            .filter(|n| {
                let hay = format!("{} {}", n.target.as_deref().unwrap_or(""), n.body).to_lowercase();
                words.iter().all(|w| hay.contains(w))
            })
            .take(200)
            .collect();
        self.audit("list_knowledge_notes", args, "allowed", Some(&format!("{} notes", notes.len())));
        if notes.is_empty() {
            return Ok(out("No notes match.", json!({"notes": []})));
        }
        let mut text = String::new();
        for n in &notes {
            let st = if n.status == NoteStatus::Proposed { " (pending review)" } else { "" };
            text.push_str(&format!("- id {} · {}{st}: {}\n", n.id, n.target.as_deref().unwrap_or("glossary"), trunc(&n.body, 600)));
        }
        Ok(out(text, json!({"notes": notes})))
    }
}
