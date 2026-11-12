use databrain_connector_core::external::OutputMasks;
use databrain_query_engine::RunRequest;
use databrain_result_store::ViewSpec;
use databrain_workspace::Origin;
use serde_json::{Value, json};

use super::{Caller, ToolContext, ToolOutput, arg, out, trunc};
use crate::policy::{self, Decision, is_pii};
use crate::resultsql;
use crate::types::{AiError, Result};

impl ToolContext {
    pub(super) async fn run_query(&self, args: &Value) -> Result<ToolOutput> {
        let sql = arg(args, "sql")?.to_string();
        // `connection`: another connection (its own policy applies).
        let target = self.schema_target(args)?;
        // results.<output> only exists on the Results (DuckDB) connection:
        // such SQL runs there, never on the current connection.
        let duckdb = target.config.kind == databrain_connector_core::ConnectorKind::Duckdb;
        if !duckdb && !self.engine.outputs().referenced(&sql).is_empty() {
            return self.query_outputs(args).await;
        }
        // Any DuckDB session (the Results connection too) can read outputs
        // as results.<name>: the same checks as query_outputs.
        if duckdb {
            self.check_output_sources(&sql)?;
        }
        let purpose = args.get("purpose").and_then(|p| p.as_str()).unwrap_or("");
        let review = policy::review_sql(&target, &sql);
        let on = if target.id == self.profile.id { String::new() } else { format!(" (on \"{}\")", target.name) };
        let approved = self
            .gate(
                "run_query",
                review.decision.clone(),
                &format!("{purpose}{on}"),
                &json!({"sql": sql, "purpose": purpose, "kind": review.kind, "connection": target.name}),
            )
            .await?;
        let sql = approved.get("sql").and_then(|s| s.as_str()).unwrap_or(&sql).to_string();
        // Re-review edited SQL (and the outputs it reads).
        if let Decision::Deny(r) = policy::review_sql(&target, &sql).decision {
            return Err(AiError::Policy(r));
        }
        if duckdb {
            self.check_output_sources(&sql)?;
        }
        let cap = 1000usize;
        let run_sql = if review.kind.is_read() { policy::cap_rows(target.config.kind, &sql, cap) } else { sql.clone() };
        let tab = format!("ai:{}", self.session_id.clone().unwrap_or_else(|| "mcp".into()));
        let outcomes = self
            .engine
            .run_and_wait(
                &self.hub,
                RunRequest {
                    connection_id: target.id.clone(),
                    tab_id: tab,
                    // DuckDB loads results.<name> masked; other databases have none.
                    output_masks: duckdb.then(|| self.output_masks(&sql)),
                    sql: run_sql,
                    base_offset: 0,
                    row_limit: Some(cap),
                    confirmed: true,
                    origin: if self.caller == Caller::Mcp { Origin::Mcp } else { Origin::Ai },
                    session_key: None,
                    output_name: None,
                    params: Default::default(),
                },
                Some(self.cancel.clone()),
            )
            .await?;
        let mut text = String::new();
        let mut display = Vec::new();
        for o in &outcomes {
            if let Some(e) = &o.error {
                text.push_str(&format!("Statement {} failed: {}\n", o.index + 1, e.message));
                display.push(json!({"index": o.index, "error": e.message}));
                continue;
            }
            match &o.result {
                Some(r) => {
                    self.results.lock().push(r.id.clone());
                    let handle =
                        self.engine.outputs().by_result(&r.id).map(|o| format!(" Output {} (query it as {}).", o.handle, o.reference())).unwrap_or_default();
                    text.push_str(&format!(
                        "Statement {} returned {} rows{} in {} ms. result_id={}{handle}\n",
                        o.index + 1,
                        r.total_rows,
                        if r.truncated { " (capped)" } else { "" },
                        o.duration_ms,
                        r.id
                    ));
                    text.push_str(&self.summarize(&r.id)?);
                    display.push(json!({"index": o.index, "result": r, "duration_ms": o.duration_ms}));
                }
                None => {
                    text.push_str(&format!("Statement {} OK{}.\n", o.index + 1, o.rows_affected.map(|n| format!(", {n} rows affected")).unwrap_or_default()));
                    display.push(json!({"index": o.index, "rows_affected": o.rows_affected}));
                }
            }
        }
        self.audit("run_query", &json!({"sql": sql, "connection": target.name}), "executed", Some(text.lines().next().unwrap_or("")));
        Ok(out(
            format!("{}{text}", if on.is_empty() { String::new() } else { format!("Ran{on}.\n") }),
            json!({"sql": sql, "statements": display, "connection_id": target.id}),
        ))
    }

    /// Schema + stats + (optionally) first rows, masked per policy.
    /// Policy that governs a result: its source connection's (data is
    /// shared on that connection's terms), falling back to this one's.
    pub(super) fn source_policy(&self, result_id: &str) -> Result<databrain_workspace::AiPolicy> {
        let Some(o) = self.engine.outputs().by_result(result_id) else { return Ok(self.policy().clone()) };
        // The tables the output was read from must be readable by the AI
        // (a query the user ran on a table the AI may not read stays hidden).
        self.check_output_tables(&o, 0)?;
        if o.connection_id == self.profile.id {
            return Ok(self.policy().clone());
        }
        let src = self
            .engine
            .workspace()
            .get_connection(&o.connection_id)
            .map_err(|_| AiError::Policy(format!("source connection of {} no longer exists", o.handle)))?;
        if !src.ai_policy.ai_enabled {
            return Err(AiError::Policy(format!("{} comes from \"{}\", where AI is disabled", o.handle, src.name)));
        }
        if self.caller == Caller::Mcp && !src.ai_policy.mcp_enabled {
            return Err(AiError::Policy(format!("{} comes from \"{}\", which is not shared with MCP", o.handle, src.name)));
        }
        Ok(src.ai_policy)
    }

    /// The output's SQL reads only tables its connection lets the AI read;
    /// outputs combined on DuckDB (`results.<x>`) are checked through their inputs.
    pub(super) fn check_output_tables(&self, o: &databrain_query_engine::outputs::OutputInfo, depth: usize) -> Result<()> {
        let src = if o.connection_id == self.profile.id {
            self.profile.clone()
        } else {
            self.engine
                .workspace()
                .get_connection(&o.connection_id)
                .map_err(|_| AiError::Policy(format!("source connection of {} no longer exists", o.handle)))?
        };
        if let Err(r) = crate::access::check_sql(&src, &o.sql) {
            return Err(AiError::Policy(format!("{} can't be shared with the assistant: {r}", o.handle)));
        }
        if depth < 5 && src.config.kind == databrain_connector_core::ConnectorKind::Duckdb {
            for input in self.engine.outputs().referenced(&o.sql) {
                if input.result_id != o.result_id {
                    self.check_output_tables(&input, depth + 1)?;
                }
            }
        }
        Ok(())
    }

    /// Accept a result id or an output reference (`r12`, `revenue`, `results.x`).
    pub(super) fn resolve_result(&self, r: &str) -> Result<String> {
        if self.engine.results().contains(r) {
            return Ok(r.to_string());
        }
        let o = self.engine.outputs().ensure_loaded(r).map_err(|e| AiError::Policy(e.message))?;
        Ok(o.result_id)
    }

    /// Which columns of a result hold PII under its source connection's
    /// rules, traced through the SQL that made it (aliases, expressions,
    /// `SELECT *`, and outputs combined on DuckDB through their inputs).
    pub(super) fn pii_mask(&self, result_id: &str, columns: &[String], depth: usize) -> Vec<bool> {
        let Some(o) = self.engine.outputs().by_result(result_id) else {
            // Not an output (no SQL to trace): by name, on this connection's rules.
            return columns.iter().map(|c| is_pii(self.policy(), None, c)).collect();
        };
        let src = if o.connection_id == self.profile.id { Ok(self.profile.clone()) } else { self.engine.workspace().get_connection(&o.connection_id) };
        let Ok(src) = src else { return vec![true; columns.len()] };
        let pol = &src.ai_policy;
        let mut rule_columns: Vec<String> = pol.pii_columns.iter().filter_map(|p| crate::policy::name_parts(p).pop()).collect();
        // Inputs (`results.<x>`): their masked columns, by every name the SQL may use.
        let mut inputs: std::collections::HashMap<String, Vec<String>> = Default::default();
        if depth < 5 {
            for input in self.engine.outputs().referenced(&o.sql) {
                if input.result_id == o.result_id {
                    continue;
                }
                let names: Vec<String> = input.columns.iter().map(|c| c.name.clone()).collect();
                let masked: Vec<String> =
                    names.iter().zip(self.pii_mask(&input.result_id, &names, depth + 1)).filter(|(_, m)| *m).map(|(n, _)| n.to_ascii_lowercase()).collect();
                if masked.is_empty() {
                    continue;
                }
                rule_columns.extend(masked.iter().cloned());
                let mut keys = vec![input.handle.clone(), input.reference()];
                keys.extend(input.name.clone());
                for k in keys {
                    let k = k.to_ascii_lowercase();
                    let bare = k.strip_prefix("results.").unwrap_or(&k).trim_matches('"').to_string();
                    inputs.insert(bare.clone(), masked.clone());
                    inputs.insert(format!("results.{bare}"), masked.clone());
                }
            }
        }
        let check = |t: Option<&str>, c: &str| match t.map(|t| t.to_ascii_lowercase().replace('"', "")) {
            Some(t) if inputs.contains_key(&t) => inputs[&t].contains(&c.to_ascii_lowercase()),
            Some(t) if t.starts_with("results.") => false,
            t => is_pii(pol, t.as_deref(), c),
        };
        crate::pii::masked_columns(&o.sql, src.config.kind, columns, &rule_columns, &check)
    }

    pub(super) fn summarize(&self, result_id: &str) -> Result<String> {
        let pol = self.source_policy(result_id)?;
        let rs = self.engine.results().get(result_id).map_err(|e| AiError::Internal(e.to_string()))?;
        // Before locking the result: tracing looks outputs up (and their results).
        let names: Vec<String> = rs.lock().info().columns.iter().map(|c| c.name.clone()).collect();
        let mask = self.pii_mask(result_id, &names, 0);
        let mut g = rs.lock();
        let info = g.info();
        let view = ViewSpec::default();
        let mut s = String::from("Columns:\n");
        for (i, c) in info.columns.iter().enumerate() {
            let pii = mask.get(i).copied().unwrap_or(true);
            s.push_str(&format!("- {} ({})", c.name, c.db_type.clone().unwrap_or_else(|| c.data_type.clone())));
            if !pii && info.total_rows > 0 {
                if let Ok(st) = g.column_stats(&view, i) {
                    s.push_str(&format!(" nulls={} distinct={}", st.nulls, st.distinct));
                    if (pol.share_sample_values || pol.share_result_rows)
                        && let (Some(mn), Some(mx)) = (&st.min, &st.max)
                    {
                        s.push_str(&format!(" min={} max={}", trunc(mn, 40), trunc(mx, 40)));
                    }
                }
            } else if pii {
                s.push_str(" [PII]");
            }
            s.push('\n');
        }
        if pol.share_result_rows && info.total_rows > 0 {
            let n = (pol.max_rows_to_model as usize).min(info.total_rows);
            let page = g.page(&view, 0, n).map_err(|e| AiError::Internal(e.to_string()))?;
            s.push_str(&format!("First {n} rows (TSV):\n"));
            s.push_str(&info.columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>().join("\t"));
            s.push('\n');
            for row in page.rows {
                let cells: Vec<String> = row
                    .iter()
                    .zip(&mask)
                    .map(|(v, m)| if *m { "***".into() } else { v.as_deref().map(|x| trunc(x, 80)).unwrap_or_else(|| "NULL".into()) })
                    .collect();
                s.push_str(&cells.join("\t"));
                s.push('\n');
            }
        } else if info.total_rows > 0 {
            s.push_str("(Raw rows are not shared on this connection; use query_result to aggregate locally.)\n");
        }
        Ok(s)
    }

    pub(super) fn result_summary(&self, args: &Value) -> Result<ToolOutput> {
        let id = args
            .get("result_id")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or_else(|| self.host.current_result())
            .ok_or_else(|| AiError::Policy("no result available".into()))?;
        let id = self.resolve_result(&id)?;
        let text = self.summarize(&id)?;
        self.audit("result_summary", args, "allowed", None);
        Ok(out(format!("result_id={id}\n{text}"), json!({"result_id": id})))
    }

    pub(super) fn allowed_result(&self, id: &str) -> bool {
        self.results.lock().iter().any(|r| r == id) || self.host.current_result().as_deref() == Some(id)
    }

    pub(super) async fn query_result(&self, args: &Value) -> Result<ToolOutput> {
        let id = self.resolve_result(arg(args, "result_id")?)?;
        let id = id.as_str();
        let sql = arg(args, "sql")?;
        // Results of this conversation, the user's current/mentioned ones,
        // or any output the source connection lets the AI read.
        if !self.allowed_result(id) && !self.source_policy(id)?.share_result_rows {
            return Err(AiError::Policy("this output was not shared with the assistant (mention it with @, or enable result rows on its connection)".into()));
        }
        let rs = self.engine.results().get(id).map_err(|e| AiError::Internal(e.to_string()))?;
        let (names, batches) = {
            let mut g = rs.lock();
            let names: Vec<String> = g.schema().fields().iter().map(|f| f.name().clone()).collect();
            (names, g.view_batches(&ViewSpec::default(), 50_000).map_err(|e| AiError::Internal(e.to_string()))?)
        };
        let mask = self.pii_mask(id, &names, 0);
        let masked: Vec<String> = names.iter().zip(&mask).filter(|(_, m)| **m).map(|(n, _)| n.to_ascii_lowercase()).collect();
        // Block PII columns from being selected in local aggregates' output.
        if masked.iter().any(|n| sql.to_ascii_lowercase().contains(n.as_str())) {
            return Err(AiError::Policy("the query references a PII column".into()));
        }
        // The values themselves are masked before the query sees them: a
        // CTE column list, `SELECT *` under another name or any expression
        // can rename a column, never unmask it.
        let batches = resultsql::mask_columns(&batches, &mask)?;
        // SQLite blocks: on a blocking thread, within the time limit, and
        // stopped at once when the turn is (Stop).
        let limits = resultsql::Limits::new(resultsql::QUERY_TIMEOUT);
        let stop = limits.stop.clone();
        let (q_names, q_sql) = (names.clone(), sql.to_string());
        let task = tokio::task::spawn_blocking(move || resultsql::query(&q_names, &batches, &q_sql, 200, &limits));
        let mut t = tokio::select! {
            r = task => r.map_err(|e| AiError::Internal(e.to_string()))??,
            _ = self.cancel.cancelled() => {
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
                return Err(AiError::Cancelled);
            }
        };
        // `SELECT *` and the like return them under their own names.
        for (i, c) in t.columns.iter().enumerate() {
            if masked.contains(&c.to_ascii_lowercase()) {
                for r in &mut t.rows {
                    if let Some(v) = r.get_mut(i) {
                        *v = Value::String("***".into());
                    }
                }
            }
        }
        let mut text = t.columns.join("\t");
        text.push('\n');
        for r in &t.rows {
            text.push_str(
                &r.iter()
                    .map(|v| match v {
                        Value::String(s) => trunc(s, 80),
                        Value::Null => "NULL".into(),
                        o => o.to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join("\t"),
            );
            text.push('\n');
        }
        if t.truncated {
            text.push_str("(first 200 rows)\n");
        }
        self.audit("query_result", args, "allowed", Some(&format!("{} rows", t.rows.len())));
        Ok(out(text, json!({"columns": t.columns, "rows": t.rows, "truncated": t.truncated})))
    }

    pub(super) fn list_outputs(&self, args: &Value) -> Result<ToolOutput> {
        let filter = args.get("filter").and_then(|f| f.as_str()).unwrap_or("").to_lowercase();
        let mut lines = Vec::new();
        let mut shown = Vec::new();
        for o in self.engine.outputs().list() {
            if o.state == databrain_query_engine::OutputState::Evicted || self.source_policy(&o.result_id).is_err() {
                continue;
            }
            let hay = format!("{} {} {} {}", o.handle, o.name.clone().unwrap_or_default(), o.connection_name, o.sql).to_lowercase();
            if !filter.is_empty() && !hay.contains(&filter) {
                continue;
            }
            let cols: Vec<String> =
                o.columns.iter().take(30).map(|c| format!("{} {}", c.name, c.db_type.clone().unwrap_or_else(|| c.data_type.clone()))).collect();
            lines.push(format!(
                "- {} ({}){} — {} rows{} from \"{}\"\n  columns: {}\n  sql: {}",
                o.reference(),
                o.handle,
                if self.allowed_result(&o.result_id) { " [in this conversation]" } else { "" },
                o.rows,
                if o.truncated { " (capped at the row limit — incomplete)" } else { "" },
                o.connection_name,
                cols.join(", "),
                trunc(&o.sql.replace('\n', " "), 300)
            ));
            shown.push(o.handle.clone());
            if lines.len() >= 40 {
                break;
            }
        }
        self.audit("list_outputs", args, "allowed", Some(&format!("{} outputs", lines.len())));
        let text = if lines.is_empty() { "No outputs available. Run a query first.".to_string() } else { lines.join("\n") };
        Ok(out(text, json!({"outputs": shown})))
    }

    /// Every output `sql` reads as `results.<name>` must be one the assistant
    /// may read: its source connection has AI on (and MCP for an MCP
    /// client), its tables pass the table rules, it shares result rows (or
    /// the user mentioned it). Its sensitive columns are masked as it loads
    /// ([`Self::output_masks`]), so the SQL may name them.
    pub(super) fn check_output_sources(&self, sql: &str) -> Result<()> {
        for o in &self.engine.outputs().referenced(sql) {
            let pol = self.source_policy(&o.result_id)?;
            if !pol.share_result_rows && !self.allowed_result(&o.result_id) {
                return Err(AiError::Policy(format!(
                    "{} is from \"{}\", which does not share result rows with the assistant; mention it with @ to allow it",
                    o.handle, o.connection_name
                )));
            }
        }
        Ok(())
    }

    /// The sensitive columns of every output `sql` reads, for DuckDB to load
    /// as `***` ([`OutputMasks`]): renaming, casting or any expression over
    /// them gets the mask, not the values, and so do error messages.
    pub(super) fn output_masks(&self, sql: &str) -> std::sync::Arc<OutputMasks> {
        let mut masks = std::collections::HashMap::new();
        for o in self.engine.outputs().referenced(sql) {
            let names: Vec<String> = o.columns.iter().map(|c| c.name.clone()).collect();
            let masked = names.iter().zip(self.pii_mask(&o.result_id, &names, 0)).filter(|(_, m)| *m).map(|(n, _)| n.clone()).collect();
            masks.insert(o.result_id.clone(), masked);
        }
        std::sync::Arc::new(OutputMasks(masks))
    }

    pub(super) async fn query_outputs(&self, args: &Value) -> Result<ToolOutput> {
        let sql = arg(args, "sql")?.to_string();
        let refs = self.engine.outputs().referenced(&sql);
        if refs.is_empty() {
            return Err(AiError::Policy("reference outputs as results.<handle or name> (see list_outputs)".into()));
        }
        self.check_output_sources(&sql)?;
        // Local DuckDB, but still reads only.
        let results_conn = self.engine.results_connection().map_err(|e| AiError::Internal(e.message))?;
        if !policy::review_sql(&results_conn, &sql).kind.is_read() {
            return Err(AiError::Policy("query_outputs only runs read-only SQL".into()));
        }
        let outcomes = self
            .engine
            .run_and_wait(
                &self.hub,
                RunRequest {
                    connection_id: results_conn.id.clone(),
                    tab_id: format!("ai-outputs:{}", self.session_id.clone().unwrap_or_else(|| "mcp".into())),
                    output_masks: Some(self.output_masks(&sql)),
                    sql: policy::cap_rows(databrain_connector_core::ConnectorKind::Duckdb, &sql, 1000),
                    base_offset: 0,
                    row_limit: Some(1000),
                    confirmed: true,
                    origin: if self.caller == Caller::Mcp { Origin::Mcp } else { Origin::Ai },
                    session_key: None,
                    output_name: None,
                    params: Default::default(),
                },
                Some(self.cancel.clone()),
            )
            .await?;
        let mut text = String::new();
        let mut display = Vec::new();
        for o in &outcomes {
            if let Some(e) = &o.error {
                text.push_str(&format!("ERROR: {}\n", e.message));
                continue;
            }
            for n in &o.notices {
                text.push_str(&format!("Note: {n}\n"));
            }
            if let Some(r) = &o.result {
                self.results.lock().push(r.id.clone());
                let handle = self.engine.outputs().by_result(&r.id).map(|x| x.handle).unwrap_or_default();
                text.push_str(&format!("{} rows (output {handle}).\n", r.total_rows));
                // Rows go back to the model: allowed because every source shares them (checked above).
                let rs = self.engine.results().get(&r.id).map_err(|e| AiError::Internal(e.to_string()))?;
                let page = rs.lock().page(&ViewSpec::default(), 0, 200).map_err(|e| AiError::Internal(e.to_string()))?;
                let names: Vec<String> = r.columns.iter().map(|c| c.name.clone()).collect();
                // Masked through the inputs (`select * from results.x`, aliases).
                let mask = self.pii_mask(&r.id, &names, 0);
                text.push_str(&names.join("\t"));
                text.push('\n');
                for row in page.rows {
                    text.push_str(
                        &row.iter()
                            .zip(&mask)
                            .map(|(v, m)| if *m { "***".into() } else { v.as_deref().map(|s| trunc(s, 80)).unwrap_or_else(|| "NULL".into()) })
                            .collect::<Vec<_>>()
                            .join("\t"),
                    );
                    text.push('\n');
                }
                if r.total_rows > 200 {
                    text.push_str("(first 200 rows)\n");
                }
                display.push(json!({"index": o.index, "result": r, "duration_ms": o.duration_ms}));
            }
        }
        self.audit("query_outputs", &json!({"sql": sql}), "executed", Some(&format!("{} outputs", refs.len())));
        Ok(out(text, json!({"sql": sql, "statements": display})))
    }

    pub(super) async fn sample_rows(&self, args: &Value) -> Result<ToolOutput> {
        if !self.policy().share_sample_values && !self.policy().share_result_rows {
            return Err(AiError::Policy("sharing sample data is disabled for this connection".into()));
        }
        let t = arg(args, "table")?;
        let o = crate::catalog::AiTables::with_profile(self.engine.workspace(), &self.profile)
            .get(t)?
            .ok_or_else(|| AiError::Policy(crate::knowledge::not_available(t)))?;
        let n = args.get("limit").and_then(|l| l.as_u64()).unwrap_or(5).clamp(1, 20) as usize;
        let kind = self.profile.config.kind;
        let full = o.full_name();
        crate::access::check_read_table(&self.profile, &full).map_err(AiError::Policy)?;
        let sql = databrain_connector_core::traits::preview_query(kind, &o.schema, &o.name, n);
        // Sampling is a read the user enabled via policy; no approval prompt.
        let outcomes = self
            .engine
            .run_and_wait(
                &self.hub,
                RunRequest {
                    connection_id: self.profile.id.clone(),
                    tab_id: format!("ai-sample:{}", self.session_id.clone().unwrap_or_default()),
                    sql,
                    base_offset: 0,
                    row_limit: Some(n),
                    confirmed: true,
                    origin: Origin::Ai,
                    session_key: None,
                    output_name: None,
                    params: Default::default(),
                    output_masks: None,
                },
                Some(self.cancel.clone()),
            )
            .await?;
        let o = outcomes.into_iter().next().ok_or_else(|| AiError::Internal("no result".into()))?;
        if let Some(e) = o.error {
            return Err(AiError::Policy(e.message));
        }
        let r = o.result.ok_or_else(|| AiError::Internal("no rows".into()))?;
        let rs = self.engine.results().get(&r.id).map_err(|e| AiError::Internal(e.to_string()))?;
        let page = rs.lock().page(&ViewSpec::default(), 0, n).map_err(|e| AiError::Internal(e.to_string()))?;
        let pol = self.policy().clone();
        let mut text = r.columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>().join("\t");
        text.push('\n');
        for row in page.rows {
            text.push_str(
                &row.iter()
                    .zip(&r.columns)
                    .map(
                        |(v, c)| {
                            if is_pii(&pol, Some(&full), &c.name) { "***".into() } else { v.as_deref().map(|x| trunc(x, 60)).unwrap_or_else(|| "NULL".into()) }
                        },
                    )
                    .collect::<Vec<_>>()
                    .join("\t"),
            );
            text.push('\n');
        }
        self.engine.results().remove(&r.id);
        self.audit("get_sample_rows", args, "allowed", None);
        Ok(out(text, json!({"table": t})))
    }
}
