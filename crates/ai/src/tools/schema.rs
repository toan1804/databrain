use databrain_workspace::NoteStatus;
use serde_json::{Value, json};

use super::{ToolContext, ToolOutput, arg, out};
use crate::catalog::AiTables;
use crate::knowledge;
use crate::policy::Decision;
use crate::types::{AiError, Result};

impl ToolContext {
    pub(super) fn policy(&self) -> &databrain_workspace::AiPolicy {
        &self.profile.ai_policy
    }

    pub(super) fn search_schema(&self, args: &Value) -> Result<ToolOutput> {
        let q = arg(args, "query")?;
        let target = self.schema_target(args)?;
        let r = knowledge::retrieve(self.engine.workspace(), &target.id, q, 8, 12_000)?;
        self.audit("search_schema", args, "allowed", Some(&format!("{} tables", r.tables.len())));
        if r.tables.is_empty() {
            let hint = if AiTables::with_profile(self.engine.workspace(), &target).count()? == 0 {
                "No table is available to the AI on this connection yet. Use index_tables for the tables the question names (it asks the user to allow them; don't ask the user for their columns); for broad questions, suggest allowing schemas (AI panel → Knowledge → Index)."
            } else {
                "No matching tables among those available to the AI. Try other keywords or list_tables; if the user named a table, index_tables asks to allow it and reads it from the database."
            };
            return Ok(out(hint, json!({"tables": []})));
        }
        let mut text = r.text.clone();
        if !r.notes.is_empty() {
            text.push_str("\n\nGlossary:\n");
            for n in &r.notes {
                text.push_str(&format!("- {n}\n"));
            }
        }
        Ok(out(text, json!({"tables": r.tables})))
    }

    pub(super) async fn describe_table(&self, args: &Value) -> Result<ToolOutput> {
        let t = arg(args, "table")?;
        let target = self.schema_target(args)?;
        let ws = self.engine.workspace();
        let tables = AiTables::with_profile(ws, &target);
        let mut o = tables.get(t)?.ok_or_else(|| AiError::Policy(knowledge::not_available(t)))?;
        // Allowed but only listed (no columns yet), or changed since: read it
        // now (metadata of a table the user allowed; cached for next time).
        // Offline, what is cached is answered.
        let stale = ws.meta_table_is_stale(&target.id, &o.schema, &o.name)?;
        if target.ai_policy.share_metadata
            && (o.columns.is_empty() || stale)
            && let Ok(d) = self.engine.describe(&target.id, &o.schema, &o.name).await
            && !d.columns.is_empty()
        {
            ws.meta_put_table(&target.id, &o.schema, &o.name, &d.columns, Some(&d.foreign_keys))?;
            if let Some(fresh) = tables.get(&o.full_name())? {
                o = fresh;
            }
        }
        let full = o.full_name();
        let notes: Vec<String> = ws
            .kn_notes(&target.id)?
            .into_iter()
            .filter(|n| n.status == NoteStatus::Approved && n.target.as_deref().is_some_and(|x| databrain_workspace::target_mentions(x, &o.schema, &o.name)))
            .filter(|n| tables.note_visible(n.target.as_deref()))
            .map(|n| match n.target.as_deref() {
                Some(t) if !t.eq_ignore_ascii_case(&full) => format!("{t}: {}", knowledge::note_for_prompt(&n)),
                _ => knowledge::note_for_prompt(&n),
            })
            .collect();
        // "Share metadata" off: the name only, as in the prompt (no columns, keys or notes).
        if !target.ai_policy.share_metadata {
            self.audit("describe_table", args, "allowed", Some("name only: metadata sharing is off"));
            return Ok(out(knowledge::render_object(&o, &target.ai_policy, &[]), json!({"table": full})));
        }
        let mut text = knowledge::render_object(&o, &target.ai_policy, &notes);
        // Tables that reference this one.
        let refs: Vec<String> = tables
            .objects()?
            .into_iter()
            .filter(|x| x.foreign_keys.iter().any(|f| f.ref_table.eq_ignore_ascii_case(&o.name) && f.ref_schema.eq_ignore_ascii_case(&o.schema)))
            .map(|x| x.full_name())
            .take(10)
            .collect();
        if !refs.is_empty() {
            text.push_str(&format!("\nReferenced by: {}", refs.join(", ")));
        }
        for l in knowledge::join_lines(&full, &knowledge::usable_joins(ws, &target)?) {
            text.push('\n');
            text.push_str(&l);
        }
        self.audit("describe_table", args, "allowed", None);
        Ok(out(text, json!({"table": full})))
    }

    pub(super) fn find_join_path(&self, args: &Value) -> Result<ToolOutput> {
        let target = self.schema_target(args)?;
        if !target.ai_policy.share_metadata {
            return Err(AiError::Policy("sharing table metadata is disabled for this connection".into()));
        }
        let (from, to) = (arg(args, "from")?, arg(args, "to")?);
        let hops = args.get("max_hops").and_then(|h| h.as_u64()).unwrap_or(3).clamp(1, 4) as usize;
        let paths = match knowledge::join_paths(self.engine.workspace(), &target, from, to, hops) {
            Ok(p) => p,
            Err(e) => {
                self.audit("find_join_path", args, "refused", Some(&e.to_string()));
                return Err(e);
            }
        };
        self.audit("find_join_path", args, "allowed", Some(&format!("{} paths", paths.len())));
        if paths.is_empty() {
            return Ok(out(
                format!(
                    "No way to join {from} to {to} in at most {hops} joins over declared foreign keys and learned joins. Check describe_table for both, or ask the user."
                ),
                json!({"paths": []}),
            ));
        }
        let mut text = String::new();
        for (i, p) in paths.iter().enumerate() {
            text.push_str(&format!("Path {} ({} join{}):\n", i + 1, p.len(), if p.len() == 1 { "" } else { "s" }));
            for s in p {
                let on: Vec<String> = s.from_columns.iter().zip(&s.to_columns).map(|(a, b)| format!("{}.{a} = {}.{b}", s.from, s.to)).collect();
                text.push_str(&format!("  JOIN {} ON {}  -- {}\n", s.to, on.join(" AND "), s.source));
            }
        }
        Ok(out(text.trim_end().to_string(), json!({"paths": paths})))
    }

    pub(super) async fn index_tables(&self, args: &Value) -> Result<ToolOutput> {
        let target = self.schema_target(args)?;
        if !target.ai_policy.share_metadata {
            return Err(AiError::Policy("sharing table metadata is disabled for this connection".into()));
        }
        let tables: Vec<String> = match args.get("tables") {
            Some(Value::Array(a)) => a.iter().filter_map(|v| v.as_str()).map(str::trim).filter(|t| !t.is_empty()).map(str::to_string).collect(),
            Some(Value::String(t)) => t.split(',').map(str::trim).filter(|t| !t.is_empty()).map(str::to_string).collect(),
            _ => vec![],
        };
        if tables.is_empty() {
            return Err(AiError::Policy("missing argument `tables`".into()));
        }
        if tables.len() > 20 {
            return Err(AiError::Policy(
                "index at most 20 tables at a time; for whole schemas, ask the user to index the connection (AI panel → Knowledge)".into(),
            ));
        }
        let reason = args.get("reason").and_then(|r| r.as_str()).unwrap_or("").trim();
        let summary = format!(
            "Read the structure (columns, keys, comments) of {} from \"{}\" into the knowledge index{}. No data is read.",
            tables.join(", "),
            target.name,
            if reason.is_empty() { String::new() } else { format!(" ({reason})") }
        );
        let detail = json!({"tables": tables, "connection": target.name});
        self.gate("index_tables", Decision::Ask("Index tables".into()), &summary, &detail).await?;
        let results = knowledge::index_tables(&self.engine, &target.id, &tables, &self.cancel).await?;
        let tables = AiTables::with_profile(self.engine.workspace(), &target);
        let mut text = String::new();
        let mut ok = Vec::new();
        let mut failed = Vec::new();
        for (want, r) in &results {
            // Shown as the AI may see it (no keys to tables outside its scope).
            let shown = match r {
                Ok(o) => tables.get(&o.full_name())?.ok_or_else(|| knowledge::not_available(want)),
                Err(e) => Err(e.clone()),
            };
            match &shown {
                Ok(o) => {
                    let full = o.full_name();
                    text.push_str(&knowledge::render_object(o, &target.ai_policy, &[]));
                    text.push('\n');
                    ok.push(full);
                }
                Err(e) => {
                    text.push_str(&format!("-- {want}: not indexed: {e}\n"));
                    failed.push(json!({"table": want, "error": e}));
                }
            }
        }
        self.audit("index_tables", args, "approved", Some(&format!("{} indexed, {} failed", ok.len(), failed.len())));
        Ok(out(format!("Indexed {} of {} tables.\n{text}", ok.len(), results.len()), json!({"indexed": ok, "failed": failed, "connection": target.name})))
    }

    pub(super) fn list_tables(&self, args: &Value) -> Result<ToolOutput> {
        let schema = args.get("schema").and_then(|s| s.as_str());
        let target = self.schema_target(args)?;
        let names: Vec<String> = AiTables::with_profile(self.engine.workspace(), &target)
            .objects()?
            .into_iter()
            .filter(|o| schema.is_none_or(|s| o.schema.eq_ignore_ascii_case(s)))
            .map(|o| o.full_name())
            .take(500)
            .collect();
        self.audit("list_tables", args, "allowed", Some(&format!("{} tables", names.len())));
        Ok(out(if names.is_empty() { "No tables are available to the AI on this connection.".into() } else { names.join("\n") }, json!({"count": names.len()})))
    }
}
