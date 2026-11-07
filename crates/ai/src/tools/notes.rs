use databrain_workspace::{KnNote, NoteStatus};
use serde_json::{Value, json};

use super::{ToolContext, ToolOutput, arg, out, trunc};
use crate::knowledge;
use crate::types::{AiError, Result};

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
            Some(t) => knowledge::check_note_target(&self.engine, &self.profile.id, t).await.map(Some).map_err(|e| {
                AiError::Policy(format!("invalid target `{t}`: {e}. Use real tables (schema.table, several joined with &), or omit target for a glossary note"))
            }),
        }
    }

    pub(super) async fn add_note(&self, args: &Value) -> Result<ToolOutput> {
        let note = arg(args, "note")?;
        let target = self.checked_target(args.get("target").and_then(|t| t.as_str())).await?;
        let ws = self.engine.workspace();
        // Same text already there: nothing to add.
        let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
        if let Some(dup) = ws.kn_notes(&self.profile.id)?.into_iter().find(|n| norm(&n.body) == norm(note)) {
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
        let old = ws
            .kn_notes(&self.profile.id)?
            .into_iter()
            .find(|n| n.id == id)
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

    pub(super) fn list_notes(&self, args: &Value) -> Result<ToolOutput> {
        let filter = args.get("filter").and_then(|f| f.as_str()).unwrap_or("").to_lowercase();
        let words: Vec<&str> = filter.split_whitespace().collect();
        let notes: Vec<KnNote> = self
            .engine
            .workspace()
            .kn_notes(&self.profile.id)?
            .into_iter()
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
