use databrain_workspace::ConnectionProfile;
use serde_json::{Value, json};

use super::{ToolContext, ToolOutput, arg, out, trunc};
use crate::policy::{self, Decision};
use crate::types::{AiError, Result};

impl ToolContext {
    pub(super) fn list_jobs(&self, args: &Value) -> Result<ToolOutput> {
        let f = args.get("filter").and_then(|f| f.as_str()).unwrap_or("").to_lowercase();
        let jobs = self.engine.workspace().list_jobs()?;
        let mut text = String::new();
        for j in jobs.iter().filter(|j| f.is_empty() || j.name.to_lowercase().contains(&f) || j.step_names.iter().any(|n| n.to_lowercase().contains(&f))) {
            let last = j.last_run.as_ref().map(|r| format!(" · last run {}", r.status)).unwrap_or_default();
            let sch = if j.schedule.enabled { " · scheduled" } else { "" };
            text.push_str(&format!("- \"{}\" (id {}): steps {}{sch}{last}\n", j.name, j.id, j.step_names.join(", ")));
        }
        if text.is_empty() {
            text = "No jobs.".into();
        }
        self.audit("list_jobs", args, "allowed", None);
        Ok(out(text, json!({"count": jobs.len()})))
    }

    /// The job a tool argument names (name or id).
    pub(super) fn find_job(&self, want: &str) -> Result<databrain_workspace::Job> {
        let ws = self.engine.workspace();
        let id = ws
            .list_jobs()?
            .into_iter()
            .find(|j| j.id == want || j.name.eq_ignore_ascii_case(want))
            .map(|j| j.id)
            .ok_or_else(|| AiError::Policy(format!("no job \"{want}\" (see list_jobs)")))?;
        Ok(ws.get_job(&id)?)
    }

    pub(super) fn get_job(&self, args: &Value) -> Result<ToolOutput> {
        let job = self.find_job(arg(args, "job")?)?;
        let ws = self.engine.workspace();
        let refs = self.conn_refs()?;
        let mut text = crate::jobs::describe(&job, &refs);
        // Latest run: what failed, with the statement (to fix it).
        if let Some(run) = ws.job_runs(&job.id, 1)?.into_iter().next() {
            text.push_str(&format!("\nLatest run ({}, {}){}\n", run.trigger, run.status, run.error.as_deref().map(|e| format!(": {e}")).unwrap_or_default()));
            let log = ws.job_run_log(&job.id, run.id).unwrap_or_default();
            let bad: Vec<_> = log.iter().filter(|l| l.level == "error" || l.level == "warning").collect();
            for l in bad.iter().rev().take(15).rev() {
                text.push_str(&format!("- [{}] {}{}\n", l.level, l.step.as_deref().map(|s| format!("{s}: ")).unwrap_or_default(), trunc(&l.message, 600)));
                if let Some(sql) = &l.sql {
                    text.push_str(&format!("  statement: {}\n", trunc(sql, 1500)));
                }
            }
        }
        text.push_str(&format!(
            "\nSpec (edit and send to update_job):\n{}\n",
            serde_json::to_string_pretty(&crate::jobs::spec_of(&job, &refs)).unwrap_or_default()
        ));
        self.audit("get_job", args, "allowed", None);
        Ok(out(text, json!({"job_id": job.id, "name": job.name})))
    }

    /// Steps of a job the AI builds or runs pass the rules its own queries
    /// do, per step and connection (runs don't prompt, so this is where
    /// they apply):
    /// - query steps: `review_sql` on their connection (writes need
    ///   `allow_write`, never on production or read-only connections; table
    ///   rules). Steps on the Results DuckDB, and the SQL of load and export
    ///   steps (which runs there), only read, like `query_outputs`;
    /// - load steps: the AI may write to the target connection and table,
    ///   and its before / after SQL passes `review_sql` there.
    ///
    /// Returns how the user approves: `Confirm` when a step calls functions
    /// that could write where the AI may not (see `review_sql`), else `ask`.
    pub(super) fn review_job(&self, job: &databrain_workspace::Job, only: &[String], ask: &str) -> Result<Decision> {
        let ws = self.engine.workspace();
        let mut confirm = Vec::new();
        let results = std::cell::OnceCell::new();
        for n in job.nodes.iter().filter(|n| only.is_empty() || only.contains(&n.id)) {
            let step = |r: String| AiError::Policy(format!("step {}: {r}", n.name));
            let conn = |id: &str| ws.get_connection(id).map_err(|_| step("its connection no longer exists".into()));
            let mut review = |p: &ConnectionProfile, sql: &str| -> Result<()> {
                match policy::review_sql(p, sql).decision {
                    Decision::Deny(r) => Err(step(r)),
                    Decision::Confirm(r) => {
                        confirm.push(format!("step {}: {r}", n.name));
                        Ok(())
                    }
                    _ => Ok(()),
                }
            };
            // SQL that runs on the Results DuckDB.
            let read_outputs = |sql: &str| -> Result<()> {
                if sql.trim().is_empty() {
                    return Ok(());
                }
                if results.get().is_none() {
                    let _ = results.set(self.engine.results_connection().map_err(|e| AiError::Internal(e.message))?);
                }
                if policy::review_sql(results.get().expect("set above"), sql).kind.is_read() {
                    Ok(())
                } else {
                    Err(step("SQL on DataBrain's outputs (the Results DuckDB) may only read; write into a connection with a load step".into()))
                }
            };
            match n.kind {
                databrain_workspace::JobNodeKind::Query => match n.connection_id.as_deref().filter(|c| !c.is_empty()) {
                    Some(c) => review(&conn(c)?, &n.sql)?,
                    None => read_outputs(&n.sql)?,
                },
                databrain_workspace::JobNodeKind::Load => {
                    read_outputs(&n.sql)?;
                    if let Some(c) = n.target_connection_id.as_deref().filter(|c| !c.is_empty()) {
                        let target = conn(c)?;
                        if let Some(r) = policy::write_denied(&target) {
                            return Err(step(format!("it loads into \"{}\": {r}", target.name)));
                        }
                        if let Some(t) = n.target_table.as_deref() {
                            crate::access::check_write_table(&target, t).map_err(step)?;
                        }
                        for sql in [&n.load_before_sql, &n.load_after_sql].into_iter().flatten().filter(|s| !s.trim().is_empty()) {
                            review(&target, sql)?;
                        }
                    }
                }
                databrain_workspace::JobNodeKind::Export => read_outputs(&n.sql)?,
            }
        }
        Ok(if confirm.is_empty() { Decision::Ask(ask.into()) } else { Decision::Confirm(confirm.join("; ")) })
    }

    pub(super) async fn run_job(&self, args: &Value) -> Result<ToolOutput> {
        let job = self.find_job(arg(args, "job")?)?;
        let refs = self.conn_refs()?;
        let want: Vec<String> = args
            .get("steps")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.trim().to_string())).collect())
            .unwrap_or_default();
        let mut only_ids = Vec::new();
        for w in &want {
            let n = job
                .nodes
                .iter()
                .find(|n| n.name.eq_ignore_ascii_case(w) || n.id == *w)
                .ok_or_else(|| AiError::Policy(format!("job \"{}\" has no step \"{w}\"", job.name)))?;
            only_ids.push(n.id.clone());
        }
        let scope = if want.is_empty() { "all steps".to_string() } else { format!("steps {}", want.join(", ")) };
        let decision = self.review_job(&job, &only_ids, "Run a job")?;
        // What a run changes: loads write tables, exports write files.
        let writes: Vec<String> = job
            .nodes
            .iter()
            .filter(|n| only_ids.is_empty() || only_ids.contains(&n.id))
            .filter_map(|n| match n.kind {
                databrain_workspace::JobNodeKind::Load => Some(format!("{} loads into {}", n.name, n.target_table.as_deref().unwrap_or("a table"))),
                databrain_workspace::JobNodeKind::Export => Some(format!("{} writes a {} file", n.name, n.export_format.extension())),
                _ => None,
            })
            .collect();
        let preview = format!(
            "Runs {scope} of \"{}\".{}\n\n{}",
            job.name,
            if writes.is_empty() { " It only reads data.".to_string() } else { format!(" It changes data: {}.", writes.join("; ")) },
            crate::jobs::describe(&job, &refs)
        );
        self.gate("run_job", decision, &format!("\"{}\" ({scope})", job.name), &json!({"preview": preview, "name": job.name})).await?;
        let run_id = self.host.start_job(&job.id, (!only_ids.is_empty()).then_some(only_ids)).await.map_err(AiError::Policy)?;
        let wait = args.get("wait_seconds").and_then(|v| v.as_u64()).unwrap_or(300).clamp(10, 1800);
        let ws = self.engine.workspace();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(wait);
        let run = loop {
            let r = ws.job_runs(&job.id, 20)?.into_iter().find(|r| r.id == run_id);
            if let Some(r) = r.filter(|r| r.status != "running") {
                break Some(r);
            }
            if tokio::time::Instant::now() > deadline {
                break None;
            }
            tokio::select! {
                _ = self.cancel.cancelled() => return Err(AiError::Cancelled),
                _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {}
            }
        };
        let Some(run) = run else {
            return Ok(out(
                format!("Job \"{}\" is still running after {wait} s; it keeps running. Check it later with get_job.", job.name),
                json!({"job_id": job.id, "name": job.name, "run_id": run_id, "running": true}),
            ));
        };
        let mut text = format!("Job \"{}\" finished: {}{}\n", job.name, run.status, run.error.as_deref().map(|e| format!(" ({e})")).unwrap_or_default());
        for n in &run.nodes {
            let st = &n.summary;
            text.push_str(&format!(
                "- {}: {}{}{}{}\n",
                n.name,
                st.status,
                st.rows.map(|r| format!(", {r} rows")).unwrap_or_default(),
                st.error.as_deref().map(|e| format!(", error: {}", trunc(e, 600))).unwrap_or_default(),
                if st.status == "success" && self.engine.outputs().resolve(&n.name).is_some() {
                    format!(" → output results.{}", n.name)
                } else {
                    String::new()
                },
            ));
        }
        if run.status != "success" {
            text.push_str("Use get_job to see the failing statement.\n");
        }
        Ok(out(text, json!({"job_id": job.id, "name": job.name, "run_id": run_id, "status": run.status})))
    }

    pub(super) async fn update_job(&self, args: &Value) -> Result<ToolOutput> {
        let old = self.find_job(arg(args, "job")?)?;
        let ws = self.engine.workspace();
        let taken: std::collections::HashSet<String> = ws.job_step_names()?.into_iter().filter(|(_, o)| o.job_id != old.id).map(|(n, _)| n).collect();
        let refs = self.conn_refs()?;
        let (mut job, notes) = crate::jobs::merge_job(&old, args, &refs, &taken)?;
        crate::jobs::mark_ai_sql(&mut job, Some(&old));
        let decision = self.review_job(&job, &[], "Change a job")?;
        let changes = crate::jobs::changes(&old, &job);
        if changes.is_empty() {
            return Ok(out("Nothing changes: the job is already like that.", json!({"job_id": old.id, "name": old.name})));
        }
        let preview = format!(
            "Changes:\n{}\n\nAfter the change:\n{}",
            changes.iter().map(|c| format!("- {c}")).collect::<Vec<_>>().join("\n"),
            crate::jobs::describe(&job, &refs)
        );
        self.gate("update_job", decision, &format!("\"{}\": {}", old.name, changes.join("; ")), &json!({"preview": preview, "name": old.name})).await?;
        let saved = ws.save_job(job)?;
        let mut text = format!("Updated job \"{}\". It is not run; the user runs it (Run) or its schedule does.\n{preview}", saved.name);
        for n in notes {
            text.push_str(&format!("\nNote: {n}"));
        }
        Ok(out(text, json!({"job_id": saved.id, "name": saved.name, "updated": true})))
    }

    pub(super) async fn create_job(&self, args: &Value) -> Result<ToolOutput> {
        let ws = self.engine.workspace();
        let taken: std::collections::HashSet<String> = ws.job_step_names()?.into_iter().map(|(n, _)| n).collect();
        let refs = self.conn_refs()?;
        let (mut job, notes) = crate::jobs::build_job(args, &refs, &taken)?;
        crate::jobs::mark_ai_sql(&mut job, None);
        let decision = self.review_job(&job, &[], "Create a job")?;
        let preview = crate::jobs::describe(&job, &refs);
        self.gate("create_job", decision, &format!("\"{}\" with {} steps", job.name, job.nodes.len()), &json!({"preview": preview, "name": job.name})).await?;
        let saved = ws.save_job(job)?;
        let mut text = format!(
            "Created job \"{}\" (id {}). It is not run yet; the user runs it from its tab (Run) or the schedule does.\n{preview}",
            saved.name, saved.id
        );
        for n in notes {
            text.push_str(&format!("Note: {n}\n"));
        }
        Ok(out(text, json!({"job_id": saved.id, "name": saved.name})))
    }
}
