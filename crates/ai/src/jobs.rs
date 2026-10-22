//! Jobs for the assistant: describe existing jobs, and turn a job the model
//! wrote (step names, connection names, SQL, links) into a [`Job`] the app
//! can save, checking it the way the job canvas does.
//!
//! Steps of a job can run on different connections; a step on the Results
//! (DuckDB) connection reads the output of earlier steps as
//! `results.<step>`, which is how a job combines several databases.

use std::collections::{HashMap, HashSet};

use databrain_connector_core::ConnectorKind;
use databrain_workspace::jobs::{topo_order, valid_node_name};
use databrain_workspace::{Job, JobNode, JobNodeKind, JobSchedule, LoadMode, ScheduleMode};
use serde_json::{Value, json};

use crate::types::{AiError, Result};

/// A connection a job step may use.
#[derive(Debug, Clone)]
pub struct ConnRef {
    pub id: String,
    pub name: String,
    pub kind: ConnectorKind,
    pub read_only: bool,
    /// AI enabled for it (the assistant only builds steps on those).
    pub ai_enabled: bool,
    /// The Results (DuckDB) connection, where `results.<step>` exists.
    pub results: bool,
}

/// What the model may pass to `create_job`.
pub fn create_job_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": {"type": "string", "description": "Job name"},
            "steps": {
                "type": "array",
                "description": "Steps; each produces an output named after the step (results.<name>)",
                "items": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "Output name: a SQL identifier (letters, digits, _), unique across all jobs"},
                        "action": {"type": "string", "enum": ["query", "load", "export"], "description": "query: run SQL on a connection; load: write rows into a table; export: write rows to a file. Default query"},
                        "connection": {"type": "string", "description": "query: connection name (list_connections). Use \"Results\" for DuckDB SQL over results.<step> of earlier steps (to combine connections)"},
                        "sql": {"type": "string", "description": "query: the SQL (one statement, the connection's dialect). load/export: optional DuckDB SELECT over results.<step>; empty = all rows of the step it runs after"},
                        "after": {"type": "array", "items": {"type": "string"}, "description": "Step names this step runs after (its inputs)"},
                        "target_connection": {"type": "string", "description": "load: connection to write into"},
                        "target_table": {"type": "string", "description": "load: schema.table to write into"},
                        "load_mode": {"type": "string", "enum": ["append", "truncate", "replace", "update", "merge"], "description": "load: append (default; creates a missing table), truncate (delete rows, then insert), replace (drop and create), update / merge (by key_columns; the table must exist)"},
                        "key_columns": {"type": "array", "items": {"type": "string"}, "description": "load update/merge: columns that identify a row"},
                        "create_table": {"type": "boolean", "description": "load append/truncate: create the table when missing (default true)"},
                        "before_sql": {"type": "string", "description": "load: SQL run on the target before writing (e.g. delete a day's rows)"},
                        "after_sql": {"type": "string", "description": "load: SQL run on the target after writing"},
                        "format": {"type": "string", "enum": ["csv", "parquet", "json"], "description": "export: file format (default csv)"},
                        "folder": {"type": "string", "description": "export: local folder (default Downloads), or with storage_connection an object storage folder URL (s3://bucket/reports/, gs://…, r2://…, az://container/…)"},
                        "storage_connection": {"type": "string", "description": "export to object storage: the DuckDB connection with the storage settings (list_connections); folder is then a URL"},
                        "file": {"type": "string", "description": "export: file name; {step} {job} {date} {time} are filled in"}
                    },
                    "required": ["name"]
                }
            },
            "schedule": {
                "type": "object",
                "description": "Optional. Runs while DataBrain is open. update_job: omit to keep the current schedule, {\"off\": true} to turn it off",
                "properties": {
                    "off": {"type": "boolean"},
                    "every_minutes": {"type": "integer", "minimum": 1},
                    "daily_at": {"type": "string", "description": "HH:MM local time"},
                    "weekdays": {"type": "array", "items": {"type": "integer", "minimum": 0, "maximum": 6}, "description": "0 = Sunday; empty = every day"}
                }
            }
        },
        "required": ["name", "steps"]
    })
}

fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(|x| x.as_str()).map(str::trim).filter(|x| !x.is_empty())
}

fn strings(v: &Value, k: &str) -> Vec<String> {
    v.get(k)
        .and_then(|x| x.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str()).map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect())
        .unwrap_or_default()
}

fn names(conns: &[ConnRef]) -> String {
    conns.iter().filter(|c| c.ai_enabled).map(|c| format!("\"{}\"", c.name)).collect::<Vec<_>>().join(", ")
}

/// A connection by id or name (case-insensitive); "Results"/"DuckDB" = the Results connection.
pub fn resolve_conn<'a>(conns: &'a [ConnRef], want: &str) -> Result<&'a ConnRef> {
    let w = want.trim();
    let c = conns
        .iter()
        .find(|c| c.id == w)
        .or_else(|| conns.iter().find(|c| c.name.eq_ignore_ascii_case(w)))
        .or_else(|| {
            let l = w.to_ascii_lowercase();
            (l == "results" || l.contains("results") || l == "duckdb").then(|| conns.iter().find(|c| c.results)).flatten()
        })
        .ok_or_else(|| AiError::Policy(format!("no connection \"{w}\"; use one of: {}", names(conns))))?;
    if !c.ai_enabled {
        return Err(AiError::Policy(format!("AI is disabled for \"{}\"; the assistant can't build steps on it", c.name)));
    }
    Ok(c)
}

/// `results.<name>` references in SQL (lowercased names).
pub fn results_refs(sql: &str) -> Vec<String> {
    let lower = sql.to_ascii_lowercase();
    let b = lower.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(p) = lower[i..].find("results.") {
        let at = i + p;
        let before_ok = at == 0 || !(b[at - 1].is_ascii_alphanumeric() || b[at - 1] == b'_' || b[at - 1] == b'.');
        let mut j = at + "results.".len();
        if b.get(j) == Some(&b'"') {
            j += 1;
        }
        let start = j;
        while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
            j += 1;
        }
        if before_ok && j > start {
            let n = lower[start..j].to_string();
            if !out.contains(&n) {
                out.push(n);
            }
        }
        i = j.max(at + 1);
    }
    out
}

fn valid_hhmm(t: &str) -> bool {
    let Some((h, m)) = t.split_once(':') else { return false };
    h.len() <= 2 && m.len() == 2 && h.parse::<u8>().is_ok_and(|h| h < 24) && m.parse::<u8>().is_ok_and(|m| m < 60)
}

fn parse_schedule(v: Option<&Value>) -> Result<JobSchedule> {
    let Some(v) = v.filter(|v| v.is_object()) else { return Ok(JobSchedule::default()) };
    if v.get("off").and_then(|o| o.as_bool()) == Some(true) {
        return Ok(JobSchedule::default());
    }
    let mut sch = JobSchedule::default();
    if let Some(m) = v.get("every_minutes").and_then(|m| m.as_u64()) {
        sch.enabled = true;
        sch.mode = ScheduleMode::Interval;
        sch.minutes = m.clamp(1, 10_080) as u32;
    } else if let Some(at) = s(v, "daily_at") {
        if !valid_hhmm(at) {
            return Err(AiError::Policy(format!("schedule.daily_at must be HH:MM, got \"{at}\"")));
        }
        sch.enabled = true;
        sch.mode = ScheduleMode::Daily;
        sch.at = at.to_string();
        sch.weekdays = v.get("weekdays").and_then(|w| w.as_array()).map(|a| a.iter().filter_map(|d| d.as_u64()).filter(|d| *d <= 6).map(|d| d as u8).collect()).unwrap_or_default();
    }
    Ok(sch)
}

/// Build a job from `create_job` arguments. `taken` = output names of other
/// jobs' steps (lowercase). Returns the job (not saved) and notes for the model.
pub fn build_job(args: &Value, conns: &[ConnRef], taken: &HashSet<String>) -> Result<(Job, Vec<String>)> {
    let name = s(args, "name").ok_or_else(|| AiError::Policy("missing argument `name`".into()))?.to_string();
    let steps = args.get("steps").and_then(|x| x.as_array()).filter(|a| !a.is_empty()).ok_or_else(|| AiError::Policy("a job needs at least one step".into()))?;
    let mut notes = Vec::new();
    // Names first: links and results.<step> refer to them.
    let mut ids: HashMap<String, String> = HashMap::new();
    for (i, st) in steps.iter().enumerate() {
        let n = s(st, "name").ok_or_else(|| AiError::Policy(format!("step {} has no name", i + 1)))?;
        if !valid_node_name(n) {
            return Err(AiError::Policy(format!("step name \"{n}\" must be a SQL identifier (letters, digits, _; no `__`; at most 63 characters)")));
        }
        let key = n.to_ascii_lowercase();
        if ids.contains_key(&key) {
            return Err(AiError::Policy(format!("two steps are named \"{n}\"")));
        }
        if taken.contains(&key) {
            return Err(AiError::Policy(format!("results.{n} is already the output of another job's step; choose another name")));
        }
        ids.insert(key, format!("s{}", i + 1));
    }
    let id_of = |n: &str| ids.get(&n.to_ascii_lowercase()).cloned();
    let mut nodes: Vec<JobNode> = Vec::new();
    let mut edges: Vec<(String, String)> = Vec::new();
    for st in steps {
        let n = s(st, "name").unwrap_or_default().to_string();
        let id = id_of(&n).unwrap_or_default();
        let action = s(st, "action").unwrap_or("query").to_ascii_lowercase();
        let kind = match action.as_str() {
            "query" => JobNodeKind::Query,
            "load" => JobNodeKind::Load,
            "export" | "save" | "file" => JobNodeKind::Export,
            other => return Err(AiError::Policy(format!("step \"{n}\": unknown action \"{other}\" (query, load or export)"))),
        };
        let mut ups: Vec<String> = Vec::new();
        for a in strings(st, "after") {
            let from = id_of(&a).ok_or_else(|| AiError::Policy(format!("step \"{n}\" runs after \"{a}\", which is not a step of this job")))?;
            if from == id {
                return Err(AiError::Policy(format!("step \"{n}\" can't run after itself")));
            }
            if !ups.contains(&from) {
                ups.push(from);
            }
        }
        let sql = s(st, "sql").unwrap_or_default().to_string();
        // results.<step> of this job: the step reads it, so it runs after it.
        for r in results_refs(&sql) {
            if let Some(from) = id_of(&r) {
                if from != id && !ups.contains(&from) {
                    ups.push(from);
                    notes.push(format!("\"{n}\" reads results.{r}, so it runs after \"{r}\""));
                }
            }
        }
        let mut node = json!({"id": id, "name": n, "kind": kind, "sql": sql});
        match kind {
            JobNodeKind::Query => {
                let c = resolve_conn(conns, s(st, "connection").ok_or_else(|| AiError::Policy(format!("query step \"{n}\" needs a connection")))?)?;
                if sql.is_empty() {
                    return Err(AiError::Policy(format!("query step \"{n}\" needs SQL")));
                }
                let reads = results_refs(&sql);
                if !(reads.is_empty() || c.results || c.kind == ConnectorKind::Duckdb) {
                    return Err(AiError::Policy(format!(
                        "step \"{n}\" reads results.{} but runs on \"{}\"; outputs only exist on the Results (DuckDB) connection: use connection \"Results\" and DuckDB SQL",
                        reads[0], c.name
                    )));
                }
                node["connection_id"] = json!(c.id);
            }
            JobNodeKind::Load => {
                let t = resolve_conn(conns, s(st, "target_connection").ok_or_else(|| AiError::Policy(format!("load step \"{n}\" needs a target_connection")))?)?;
                if t.read_only {
                    return Err(AiError::Policy(format!("\"{}\" is read-only; a load step can't write into it", t.name)));
                }
                let table = s(st, "target_table").ok_or_else(|| AiError::Policy(format!("load step \"{n}\" needs a target_table")))?;
                let mode = match s(st, "load_mode").unwrap_or("append").to_ascii_lowercase().as_str() {
                    "append" | "insert" => LoadMode::Append,
                    "truncate" | "delete" => LoadMode::Truncate,
                    "replace" => LoadMode::Replace,
                    "update" => LoadMode::Update,
                    "merge" | "upsert" => LoadMode::Merge,
                    other => return Err(AiError::Policy(format!("step \"{n}\": unknown load_mode \"{other}\""))),
                };
                let keys = strings(st, "key_columns");
                if mode.needs_keys() && keys.is_empty() {
                    return Err(AiError::Policy(format!("load step \"{n}\" ({}) needs key_columns", s(st, "load_mode").unwrap_or_default())));
                }
                if ups.is_empty() && sql.is_empty() {
                    return Err(AiError::Policy(format!("load step \"{n}\" needs an input: run it after a step (after) or give DuckDB SQL over results.<step>")));
                }
                node["target_connection_id"] = json!(t.id);
                node["target_table"] = json!(table);
                node["load_mode"] = json!(mode);
                node["key_columns"] = json!(keys);
                node["create_table"] = json!(st.get("create_table").and_then(|b| b.as_bool()).unwrap_or(true));
                node["load_before_sql"] = json!(s(st, "before_sql"));
                node["load_after_sql"] = json!(s(st, "after_sql"));
            }
            JobNodeKind::Export => {
                if ups.is_empty() && sql.is_empty() {
                    return Err(AiError::Policy(format!("export step \"{n}\" needs an input: run it after a step (after) or give DuckDB SQL over results.<step>")));
                }
                let fmt = match s(st, "format").unwrap_or("csv").to_ascii_lowercase().as_str() {
                    "parquet" => "parquet",
                    "json" | "ndjson" | "jsonl" => "json",
                    _ => "csv",
                };
                node["export_format"] = json!(fmt);
                if let Some(sc) = s(st, "storage_connection") {
                    let c = resolve_conn(conns, sc)?;
                    if c.kind != ConnectorKind::Duckdb || c.results {
                        return Err(AiError::Policy(format!("export step \"{n}\": \"{}\" is not a DuckDB connection with object storage", c.name)));
                    }
                    if c.read_only {
                        return Err(AiError::Policy(format!("\"{}\" is read-only; an export step can't write through it", c.name)));
                    }
                    if !s(st, "folder").is_some_and(|f| f.contains("://")) {
                        return Err(AiError::Policy(format!("export step \"{n}\" writes to object storage: folder must be a URL such as s3://bucket/reports/")));
                    }
                    node["export_connection_id"] = json!(c.id);
                }
                node["export_folder"] = json!(s(st, "folder"));
                node["export_file"] = json!(s(st, "file"));
            }
        }
        for u in &ups {
            edges.push((u.clone(), id.clone()));
        }
        let mut node: JobNode = serde_json::from_value(node).map_err(|e| AiError::Internal(e.to_string()))?;
        node.keep_own_settings();
        nodes.push(node);
    }
    let mut job = Job {
        id: String::new(),
        name,
        nodes,
        edges: edges.into_iter().map(|(from, to)| databrain_workspace::JobEdge { from, to }).collect(),
        schedule: parse_schedule(args.get("schedule"))?,
        last_scheduled_at: None,
        created_at: 0,
        updated_at: 0,
    };
    let order = topo_order(&job).map_err(|cycle| {
        let names: Vec<String> = cycle.iter().filter_map(|id| job.nodes.iter().find(|n| &n.id == id).map(|n| n.name.clone())).collect();
        AiError::Policy(format!("the steps run in a circle ({}); remove a link", names.join(", ")))
    })?;
    layout(&mut job, &order);
    Ok((job, notes))
}

/// Top-down layout: a row per dependency level (sizes as on the canvas).
fn layout(job: &mut Job, order: &[String]) {
    let mut level: HashMap<String, usize> = HashMap::new();
    for id in order {
        let l = job.edges.iter().filter(|e| &e.to == id).filter_map(|e| level.get(&e.from)).map(|l| l + 1).max().unwrap_or(0);
        level.insert(id.clone(), l);
    }
    let mut col: HashMap<usize, usize> = HashMap::new();
    for id in order {
        let l = level[id];
        let c = col.entry(l).or_default();
        if let Some(n) = job.nodes.iter_mut().find(|n| &n.id == id) {
            n.x = 80.0 + *c as f64 * 260.0;
            n.y = 60.0 + l as f64 * 146.0;
        }
        *c += 1;
    }
}

fn conn_name(conns: &[ConnRef], id: Option<&str>) -> String {
    match id {
        Some(id) => conns.iter().find(|c| c.id == id).map(|c| c.name.clone()).unwrap_or_else(|| format!("(missing connection {id})")),
        None => "(none)".into(),
    }
}

/// A job as text, for the model and the approval prompt.
pub fn describe(job: &Job, conns: &[ConnRef]) -> String {
    let name_of = |id: &str| job.nodes.iter().find(|n| n.id == id).map(|n| n.name.clone()).unwrap_or_default();
    let mut t = format!("Job \"{}\"", job.name);
    if job.schedule.enabled {
        t.push_str(&match job.schedule.mode {
            ScheduleMode::Interval => format!(" · every {} min", job.schedule.minutes),
            ScheduleMode::Daily => format!(" · daily at {}{}", job.schedule.at, if job.schedule.weekdays.is_empty() { String::new() } else { format!(" on days {:?}", job.schedule.weekdays) }),
        });
    }
    t.push('\n');
    for n in &job.nodes {
        let after: Vec<String> = job.edges.iter().filter(|e| e.to == n.id).map(|e| name_of(&e.from)).collect();
        let after = if after.is_empty() { String::new() } else { format!(" (after {})", after.join(", ")) };
        match n.kind {
            JobNodeKind::Query => t.push_str(&format!("- {} = query on \"{}\"{after}\n", n.name, conn_name(conns, n.connection_id.as_deref()))),
            JobNodeKind::Load => t.push_str(&format!(
                "- {} = load into \"{}\" table {} ({:?}{}{}){after}\n",
                n.name,
                conn_name(conns, n.target_connection_id.as_deref()),
                n.target_table.as_deref().unwrap_or("?"),
                n.load_mode,
                if n.key_columns.is_empty() { String::new() } else { format!(", keys {}", n.key_columns.join(", ")) },
                if n.load_mode == LoadMode::Append || n.load_mode == LoadMode::Truncate { if n.create_table { ", creates a missing table" } else { "" } } else { "" },
            )),
            JobNodeKind::Export => t.push_str(&format!(
                "- {} = save to a {} file in {}{}{after}\n",
                n.name,
                n.export_format.extension(),
                n.export_folder.as_deref().unwrap_or("Downloads"),
                if n.export_connection_id.is_some() { " (object storage)" } else { "" },
            )),
        }
        if !n.sql.trim().is_empty() {
            t.push_str(&format!("  sql: {}\n", n.sql.trim().replace('\n', "\n       ")));
        }
        if let Some(b) = n.load_before_sql.as_deref() {
            t.push_str(&format!("  before: {b}\n"));
        }
        if let Some(a) = n.load_after_sql.as_deref() {
            t.push_str(&format!("  after: {a}\n"));
        }
        if let Some(r) = &n.last_run {
            t.push_str(&format!(
                "  last run: {}{}{}\n",
                r.status,
                r.rows.map(|x| format!(", {x} rows")).unwrap_or_default(),
                r.error.as_deref().map(|e| format!(", error: {e}")).unwrap_or_default()
            ));
        }
    }
    t
}

/// `update_job` arguments: the job, then the complete new job in the
/// `create_job` shape (as `get_job` shows it).
pub fn update_job_schema() -> Value {
    let mut v = create_job_schema();
    v["properties"]["job"] = json!({"type": "string", "description": "Name or id of the job to change"});
    v["properties"]["name"]["description"] = json!("New job name (omit to keep it)");
    v["properties"]["steps"]["description"] = json!("The COMPLETE list of steps after the change (start from get_job's spec). Steps keep their place, history and output when their name is kept; a step left out is removed; renaming a step also renames results.<old> in the other steps' SQL");
    v["properties"]["rename"] = json!({"type": "object", "additionalProperties": {"type": "string"}, "description": "Steps renamed: {\"old_name\": \"new_name\"}"});
    v["required"] = json!(["job", "steps"]);
    v
}

/// A job in the `create_job` shape, so the model can change it and send it back.
pub fn spec_of(job: &Job, conns: &[ConnRef]) -> Value {
    let name_of = |id: &str| job.nodes.iter().find(|n| n.id == id).map(|n| n.name.clone()).unwrap_or_default();
    let cname = |id: Option<&str>| id.and_then(|id| conns.iter().find(|c| c.id == id)).map(|c| c.name.clone());
    let steps: Vec<Value> = job
        .nodes
        .iter()
        .map(|n| {
            let after: Vec<String> = job.edges.iter().filter(|e| e.to == n.id).map(|e| name_of(&e.from)).collect();
            let mut v = json!({"name": n.name, "action": n.kind});
            if !n.sql.trim().is_empty() {
                v["sql"] = json!(n.sql);
            }
            if !after.is_empty() {
                v["after"] = json!(after);
            }
            match n.kind {
                JobNodeKind::Query => v["connection"] = json!(cname(n.connection_id.as_deref())),
                JobNodeKind::Load => {
                    v["target_connection"] = json!(cname(n.target_connection_id.as_deref()));
                    v["target_table"] = json!(n.target_table);
                    v["load_mode"] = json!(match n.load_mode {
                        LoadMode::Append => "append",
                        LoadMode::Truncate => "truncate",
                        LoadMode::Replace => "replace",
                        LoadMode::Update => "update",
                        LoadMode::Merge => "merge",
                    });
                    if !n.key_columns.is_empty() {
                        v["key_columns"] = json!(n.key_columns);
                    }
                    if !n.create_table {
                        v["create_table"] = json!(false);
                    }
                    if let Some(b) = &n.load_before_sql {
                        v["before_sql"] = json!(b);
                    }
                    if let Some(a) = &n.load_after_sql {
                        v["after_sql"] = json!(a);
                    }
                }
                JobNodeKind::Export => {
                    v["format"] = json!(n.export_format.extension());
                    if let Some(f) = &n.export_folder {
                        v["folder"] = json!(f);
                    }
                    if let Some(c) = &n.export_connection_id {
                        v["storage_connection"] = json!(cname(Some(c)));
                    }
                    if let Some(f) = &n.export_file {
                        v["file"] = json!(f);
                    }
                }
            }
            v
        })
        .collect();
    let mut out = json!({"name": job.name, "steps": steps});
    if job.schedule.enabled {
        out["schedule"] = match job.schedule.mode {
            ScheduleMode::Interval => json!({"every_minutes": job.schedule.minutes}),
            ScheduleMode::Daily => json!({"daily_at": job.schedule.at, "weekdays": job.schedule.weekdays}),
        };
    }
    out
}

/// `results.<old>` → `results.<new>` in SQL (any case, quoted or not).
pub fn rename_results(sql: &str, old: &str, new: &str) -> String {
    let lower = sql.to_ascii_lowercase();
    let (old_l, b) = (old.to_ascii_lowercase(), lower.as_bytes());
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    while let Some(p) = lower[i..].find("results.") {
        let at = i + p;
        let mut j = at + "results.".len();
        let quoted = b.get(j) == Some(&b'"');
        if quoted {
            j += 1;
        }
        let start = j;
        while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
            j += 1;
        }
        let before_ok = at == 0 || !(b[at - 1].is_ascii_alphanumeric() || b[at - 1] == b'_' || b[at - 1] == b'.');
        if before_ok && lower[start..j] == old_l {
            out.push_str(&sql[i..start]);
            out.push_str(new);
        } else {
            out.push_str(&sql[i..j.max(at + 1)]);
        }
        i = j.max(at + 1);
    }
    out.push_str(&sql[i..]);
    out
}

/// What changed between two versions of a job, one line each.
pub fn changes(old: &Job, new: &Job) -> Vec<String> {
    let mut out = Vec::new();
    if old.name != new.name {
        out.push(format!("renamed the job to \"{}\"", new.name));
    }
    for n in &new.nodes {
        match old.nodes.iter().find(|o| o.id == n.id) {
            None => out.push(format!("adds step {}", n.name)),
            Some(o) => {
                if o.name != n.name {
                    out.push(format!("renames step {} to {}", o.name, n.name));
                }
                let (mut a, mut b) = (o.clone(), n.clone());
                a.name.clear();
                b.name.clear();
                let deps = |j: &Job, id: &str| {
                    let mut v: Vec<String> = j.edges.iter().filter(|e| e.to == id).map(|e| e.from.clone()).collect();
                    v.sort();
                    v
                };
                let same = a.kind == b.kind
                    && a.sql == b.sql
                    && a.connection_id == b.connection_id
                    && a.target_connection_id == b.target_connection_id
                    && a.target_table == b.target_table
                    && a.load_mode == b.load_mode
                    && a.key_columns == b.key_columns
                    && a.create_table == b.create_table
                    && a.load_before_sql == b.load_before_sql
                    && a.load_after_sql == b.load_after_sql
                    && a.export_format == b.export_format
                    && a.export_folder == b.export_folder
                    && a.export_connection_id == b.export_connection_id
                    && a.export_file == b.export_file
                    && deps(old, &o.id) == deps(new, &n.id);
                if !same {
                    out.push(format!("changes step {}", n.name));
                }
            }
        }
    }
    for o in &old.nodes {
        if !new.nodes.iter().any(|n| n.id == o.id) {
            out.push(format!("removes step {}", o.name));
        }
    }
    if old.schedule != new.schedule {
        out.push(if new.schedule.enabled { "changes the schedule".to_string() } else { "turns the schedule off".to_string() });
    }
    out
}

/// Apply `update_job` arguments to `old`: steps whose name is kept (or
/// renamed via `rename`) keep their id, place and history; `taken` = other
/// jobs' output names. Returns the new job and notes for the model.
pub fn merge_job(old: &Job, args: &Value, conns: &[ConnRef], taken: &HashSet<String>) -> Result<(Job, Vec<String>)> {
    let rename: Vec<(String, String)> = args
        .get("rename")
        .and_then(|r| r.as_object())
        .map(|m| m.iter().filter_map(|(k, v)| v.as_str().map(|v| (k.trim().to_string(), v.trim().to_string()))).collect())
        .unwrap_or_default();
    let mut a = args.clone();
    if s(&a, "name").is_none() {
        a["name"] = json!(old.name);
    }
    // Renamed outputs: the other steps' SQL follows.
    if !rename.is_empty() {
        if let Some(steps) = a.get_mut("steps").and_then(|x| x.as_array_mut()) {
            for st in steps {
                if let Some(after) = st.get_mut("after").and_then(|x| x.as_array_mut()) {
                    for a in after.iter_mut() {
                        if let Some((_, n)) = a.as_str().and_then(|v| rename.iter().find(|(o, _)| o.eq_ignore_ascii_case(v.trim()))) {
                            *a = json!(n);
                        }
                    }
                }
                if let Some(sql) = st.get("sql").and_then(|x| x.as_str()).map(str::to_string) {
                    let mut sql2 = sql.clone();
                    for (o, n) in &rename {
                        sql2 = rename_results(&sql2, o, n);
                    }
                    if sql2 != sql {
                        st["sql"] = json!(sql2);
                    }
                }
            }
        }
    }
    let (mut job, notes) = build_job(&a, conns, taken)?;
    // The old id of each new step: same name, or its name before a rename.
    let old_name = |new: &str| rename.iter().find(|(_, n)| n.eq_ignore_ascii_case(new)).map(|(o, _)| o.clone()).unwrap_or_else(|| new.to_string());
    let mut map: HashMap<String, String> = HashMap::new();
    for n in &job.nodes {
        if let Some(o) = old.nodes.iter().find(|o| o.name.eq_ignore_ascii_case(&old_name(&n.name))) {
            map.insert(n.id.clone(), o.id.clone());
        }
    }
    let max_x = old.nodes.iter().map(|n| n.x).fold(f64::MIN, f64::max);
    let new_ids: HashMap<String, String> = job.nodes.iter().filter(|n| !map.contains_key(&n.id)).map(|n| (n.id.clone(), databrain_workspace::new_id())).collect();
    let fresh = |id: &str| new_ids.get(id).cloned().unwrap_or_default();
    for n in &mut job.nodes {
        match map.get(&n.id).and_then(|oid| old.nodes.iter().find(|o| &o.id == oid)) {
            Some(o) => {
                n.x = o.x;
                n.y = o.y;
                n.last_run = o.last_run.clone();
                n.id = o.id.clone();
            }
            None => {
                // New steps go right of the existing ones.
                if max_x > f64::MIN {
                    n.x += max_x + 260.0;
                }
                n.id = fresh(&n.id);
            }
        }
    }
    let id_of = |tmp: &str| map.get(tmp).cloned().unwrap_or_else(|| fresh(tmp));
    for e in &mut job.edges {
        e.from = id_of(&e.from);
        e.to = id_of(&e.to);
    }
    job.id = old.id.clone();
    job.created_at = old.created_at;
    job.last_scheduled_at = old.last_scheduled_at;
    if args.get("schedule").is_none() {
        job.schedule = old.schedule.clone();
    }
    Ok((job, notes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conns() -> Vec<ConnRef> {
        let c = |id: &str, name: &str, kind, results| ConnRef { id: id.into(), name: name.into(), kind, read_only: false, ai_enabled: true, results };
        let mut v = vec![
            c("pg", "Sales PG", ConnectorKind::Postgres, false),
            c("sf", "Snowflake", ConnectorKind::Snowflake, false),
            c("res", "Results", ConnectorKind::Duckdb, true),
        ];
        v.push(ConnRef { id: "ro".into(), name: "Reporting".into(), kind: ConnectorKind::Postgres, read_only: true, ai_enabled: true, results: false });
        v.push(ConnRef { id: "off".into(), name: "Secret".into(), kind: ConnectorKind::Mysql, read_only: false, ai_enabled: false, results: false });
        v
    }

    #[test]
    fn builds_a_job_across_connections() {
        let args = json!({
            "name": "Daily revenue",
            "steps": [
                {"name": "orders", "connection": "sales pg", "sql": "select * from orders"},
                {"name": "fx", "connection": "Snowflake", "sql": "select * from fx_rates"},
                {"name": "revenue", "connection": "Results", "sql": "select o.day, sum(o.amount * f.rate) from results.orders o join results.fx f using (currency) group by 1"},
                {"name": "save_revenue", "action": "load", "after": ["revenue"], "target_connection": "Sales PG", "target_table": "rpt.revenue", "load_mode": "merge", "key_columns": ["day"]},
                {"name": "revenue_file", "action": "export", "after": ["revenue"], "format": "parquet"}
            ],
            "schedule": {"daily_at": "06:30", "weekdays": [1, 2, 3, 4, 5]}
        });
        let (job, notes) = build_job(&args, &conns(), &HashSet::new()).unwrap();
        assert_eq!(job.nodes.len(), 5);
        assert_eq!(notes.len(), 2, "{notes:?}");
        let e: Vec<(String, String)> = job.edges.iter().map(|e| (e.from.clone(), e.to.clone())).collect();
        assert!(e.contains(&("s1".into(), "s3".into())) && e.contains(&("s2".into(), "s3".into())) && e.contains(&("s3".into(), "s4".into())));
        assert_eq!(job.nodes[2].connection_id.as_deref(), Some("res"));
        assert_eq!(job.nodes[3].load_mode, LoadMode::Merge);
        assert_eq!(job.nodes[3].connection_id, None);
        assert!(job.schedule.enabled && job.schedule.at == "06:30");
        // Levels: sources on top, revenue below, outputs last.
        assert_eq!((job.nodes[0].y, job.nodes[2].y, job.nodes[3].y), (60.0, 206.0, 352.0));
        let d = describe(&job, &conns());
        assert!(d.contains("revenue = query on \"Results\" (after orders, fx)") && d.contains("load into \"Sales PG\" table rpt.revenue"), "{d}");
    }

    #[test]
    fn refuses_what_would_not_run() {
        let c = conns();
        let bad = |steps: Value| build_job(&json!({"name": "j", "steps": steps}), &c, &HashSet::new()).unwrap_err().to_string();
        assert!(bad(json!([{"name": "a", "connection": "Sales PG", "sql": "select 1"}, {"name": "b", "connection": "Sales PG", "sql": "select * from results.a"}])).contains("Results (DuckDB)"));
        assert!(bad(json!([{"name": "a", "connection": "Nope", "sql": "select 1"}])).contains("no connection"));
        assert!(bad(json!([{"name": "a", "connection": "Secret", "sql": "select 1"}])).contains("AI is disabled"));
        assert!(bad(json!([{"name": "a", "connection": "Results", "sql": "select 1"}, {"name": "w", "action": "load", "after": ["a"], "target_connection": "Reporting", "target_table": "t"}])).contains("read-only"));
        assert!(bad(json!([{"name": "a", "connection": "Results", "sql": "select 1", "after": ["b"]}, {"name": "b", "connection": "Results", "sql": "select 2", "after": ["a"]}])).contains("circle"));
        assert!(bad(json!([{"name": "bad name", "connection": "Results", "sql": "select 1"}])).contains("identifier"));
        let taken: HashSet<String> = ["orders".to_string()].into();
        assert!(build_job(&json!({"name": "j", "steps": [{"name": "Orders", "connection": "Results", "sql": "select 1"}]}), &c, &taken).unwrap_err().to_string().contains("already"));
    }

    #[test]
    fn edits_keep_steps_and_follow_renames() {
        let c = conns();
        let args = json!({"name": "j", "steps": [
            {"name": "orders", "connection": "Sales PG", "sql": "select * from orders"},
            {"name": "daily", "connection": "Results", "sql": "select day, count(*) from results.orders group by 1"}
        ], "schedule": {"every_minutes": 30}});
        let (mut old, _) = build_job(&args, &c, &HashSet::new()).unwrap();
        old.id = "J".into();
        for (i, n) in old.nodes.iter_mut().enumerate() {
            n.id = format!("n{i}");
            n.x = 500.0 + i as f64;
        }
        old.edges = vec![databrain_workspace::JobEdge { from: "n0".into(), to: "n1".into() }];
        // The spec round-trips without changes.
        let mut spec = spec_of(&old, &c);
        spec["job"] = json!("j");
        let (same, _) = merge_job(&old, &spec, &c, &HashSet::new()).unwrap();
        assert!(changes(&old, &same).is_empty(), "{:?}", changes(&old, &same));
        assert_eq!(same.schedule, old.schedule);
        // Fix the SQL, rename a step, add a file export.
        spec["steps"][0]["name"] = json!("orders_2026");
        spec["steps"][0]["sql"] = json!("select * from orders where day >= date '2026-01-01'");
        spec["rename"] = json!({"orders": "orders_2026"});
        spec["steps"].as_array_mut().unwrap().push(json!({"name": "daily_file", "action": "export", "after": ["daily"]}));
        spec["schedule"] = json!({"off": true});
        let (new, _) = merge_job(&old, &spec, &c, &HashSet::new()).unwrap();
        assert_eq!((new.nodes[0].id.as_str(), new.nodes[0].x), ("n0", 500.0), "kept its place");
        assert!(new.nodes[1].sql.contains("results.orders_2026"), "{}", new.nodes[1].sql);
        assert!(new.nodes[2].id.len() > 5 && new.nodes[2].x > 500.0);
        assert!(new.edges.iter().any(|e| e.from == "n1" && e.to == new.nodes[2].id));
        let ch = changes(&old, &new);
        assert!(ch.contains(&"renames step orders to orders_2026".to_string()) && ch.contains(&"adds step daily_file".to_string()) && ch.contains(&"turns the schedule off".to_string()), "{ch:?}");
        // Leaving a step out removes it.
        let (fewer, _) = merge_job(&old, &json!({"job": "j", "steps": [old_step(&spec)]}), &c, &HashSet::new()).unwrap();
        assert!(changes(&old, &fewer).iter().any(|x| x == "removes step daily"));
    }

    fn old_step(_spec: &Value) -> Value {
        json!({"name": "orders", "connection": "Sales PG", "sql": "select * from orders"})
    }

    #[test]
    fn renames_results_references() {
        assert_eq!(rename_results("select * from results.a join RESULTS.\"a\" x join results.ab", "a", "b"), "select * from results.b join RESULTS.\"b\" x join results.ab");
    }

    #[test]
    fn finds_results_references() {
        assert_eq!(results_refs("select * from results.a join RESULTS.\"b\" x, myresults.c, results.a"), vec!["a", "b"]);
    }
}
