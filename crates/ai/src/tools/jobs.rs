use serde_json::{Value, json};

use super::{ToolContext, ToolOutput, arg, out, trunc};
use crate::policy::Decision;
use crate::types::{AiError, Result};

impl ToolContext {
    pub(super) fn list_jobs(&self, args: &Value) -> Result<ToolOutput> {
        let f = args.get("filter").and_then(|f| f.as_str()).unwrap_or("").to_lowercase();
        let ws = self.engine.workspace();
        // Only jobs whose connections the assistant may work on.
        let jobs: Vec<_> = ws.list_jobs()?.into_iter().filter(|j| ws.get_job(&j.id).is_ok_and(|job| self.job_visible(&job))).collect();
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

    fn job_visible(&self, job: &databrain_workspace::Job) -> bool {
        crate::jobs::visible(self.engine.workspace(), job, self.caller == super::Caller::Mcp, self.provider_id.as_deref())
    }

    /// The job a tool argument names (name or id). A job on a connection
    /// the assistant may not work on is answered like one that isn't there.
    pub(super) fn find_job(&self, want: &str) -> Result<databrain_workspace::Job> {
        let ws = self.engine.workspace();
        let missing = || AiError::Policy(format!("no job \"{want}\" (see list_jobs)"));
        let id = ws.list_jobs()?.into_iter().find(|j| j.id == want || j.name.eq_ignore_ascii_case(want)).map(|j| j.id).ok_or_else(missing)?;
        let job = ws.get_job(&id)?;
        if !self.job_visible(&job) {
            return Err(missing());
        }
        Ok(job)
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

    /// The current schema of each connection the job's steps run on, for the
    /// table rules ([`crate::access::learn_defaults`]).
    async fn learn_job_defaults(&self, job: &databrain_workspace::Job) {
        let mut ids: Vec<String> = Vec::new();
        for n in &job.nodes {
            for c in [&n.connection_id, &n.target_connection_id].into_iter().flatten() {
                if !c.is_empty() && !ids.contains(c) {
                    ids.push(c.clone());
                }
            }
        }
        for id in ids {
            if let Ok(p) = self.engine.workspace().get_connection(&id) {
                crate::access::learn_defaults(&self.engine, &p).await;
            }
        }
    }

    /// [`crate::jobs::review`] as a decision: `Confirm` when only the user
    /// may approve (see there), else `ask`.
    pub(super) fn review_job(&self, job: &databrain_workspace::Job, only: &[String], ask: &str) -> Result<Decision> {
        let confirm = crate::jobs::review(&self.engine, job, only)?;
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
        self.learn_job_defaults(&job).await;
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
                _ = self.cancel.cancelled() => {
                    // Stop ends the run the assistant started, not only the wait for it.
                    self.host.stop_job(&job.id).await;
                    self.audit("run_job", args, "stopped", Some("the user stopped the assistant; the run was cancelled"));
                    return Err(AiError::Cancelled);
                }
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
        self.learn_job_defaults(&job).await;
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
        self.learn_job_defaults(&job).await;
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
