//! Query plans: the database's own `EXPLAIN` read into one tree shape, so
//! the UI can draw any engine's plan the same way (operation, object, cost,
//! estimated rows and, with ANALYZE, actual rows, time and loops).
//!
//! | Engine | Plan | ANALYZE |
//! |---|---|---|
//! | Postgres | `EXPLAIN (FORMAT JSON)` | `ANALYZE, BUFFERS` |
//! | MySQL/MariaDB | `EXPLAIN FORMAT=TREE` (else the classic table) | `EXPLAIN ANALYZE` |
//! | SQLite | `EXPLAIN QUERY PLAN` | – |
//! | DuckDB | `EXPLAIN (FORMAT JSON)` | `EXPLAIN (ANALYZE, FORMAT JSON)` |
//! | SQL Server | `SET SHOWPLAN_ALL ON` | – |
//! | Oracle | `EXPLAIN PLAN` + `PLAN_TABLE` | – |
//! | Snowflake | `EXPLAIN USING JSON` | – |
//! | Databricks | `EXPLAIN FORMATTED` (text only) | – |
//!
//! ANALYZE runs the statement, so it is only offered for read queries.

use std::collections::HashMap;
use std::sync::Arc;

use databrain_connector_core::sql::{classify, StatementKind};
use databrain_connector_core::{query_table, ConnectorKind, Session};
use serde::Serialize;
use serde_json::Value;

use crate::{EngineError, QueryEngine, Result};

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct PlanNode {
    /// What the step does (`Seq Scan`, `Hash Join`, `TABLE ACCESS FULL`…).
    pub op: String,
    /// What it works on (`on public.orders o`, `using orders_pkey`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object: Option<String>,
    /// Estimated cost as the engine counts it (total for the subtree).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows: Option<f64>,
    /// ANALYZE: rows produced (per loop where the engine says so), time in ms
    /// (inclusive, per loop) and loops.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_rows: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loops: Option<f64>,
    /// Other details (conditions, filters, buffers…), in the engine's words.
    pub extra: Vec<(String, String)>,
    pub children: Vec<PlanNode>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct QueryPlan {
    pub analyzed: bool,
    pub roots: Vec<PlanNode>,
    /// The plan as the database printed it.
    pub raw: String,
    /// Planning and execution time when the engine reports them (ms).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub planning_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_ms: Option<f64>,
}

/// Can this engine EXPLAIN, and ANALYZE?
pub fn plan_support(kind: ConnectorKind) -> (bool, bool) {
    use ConnectorKind::*;
    match kind {
        Postgres | Mysql | Duckdb => (true, true),
        Sqlite | Mssql | Oracle | Snowflake | Databricks => (true, false),
        _ => (false, false),
    }
}

impl QueryEngine {
    /// The plan of one statement on the session `session_key` (the tab's,
    /// so temporary tables and the open transaction are seen).
    pub async fn explain(&self, connection_id: &str, session_key: &str, sql: &str, analyze: bool) -> Result<QueryPlan> {
        let kind = self.workspace().get_connection(connection_id)?.config.kind;
        let (plan, can_analyze) = plan_support(kind);
        if !plan {
            return Err(EngineError::new("unsupported", format!("{} has no query plan DataBrain can read", kind.traits().name)));
        }
        let sql = sql.trim().trim_end_matches(';').trim();
        if sql.is_empty() {
            return Err(EngineError::new("invalid", "Nothing to explain"));
        }
        if analyze && !can_analyze {
            return Err(EngineError::new("unsupported", format!("{} plans can't be analyzed here: use Explain for the estimated plan", kind.traits().name)));
        }
        if analyze && classify(sql, kind).kind != StatementKind::Read {
            return Err(EngineError::new("invalid", "ANALYZE runs the statement: it is only offered for read queries"));
        }
        if self.jobs.lock().values().any(|j| j.session_key == session_key) {
            return Err(EngineError::new("busy", "A query is running in this tab: wait for it or stop it first"));
        }
        let session = self.session(connection_id, session_key).await?;
        Ok(explain_on(&*session, kind, sql, analyze).await?)
    }
}

type Rows = (Vec<String>, Vec<Vec<Option<String>>>);

async fn explain_on(s: &dyn Session, kind: ConnectorKind, sql: &str, analyze: bool) -> databrain_connector_core::Result<QueryPlan> {
    use ConnectorKind::*;
    let first_cell = |r: &Rows| r.1.iter().filter_map(|row| row.first().cloned().flatten()).collect::<Vec<_>>().join("\n");
    match kind {
        Postgres => {
            let opts = if analyze { "FORMAT JSON, ANALYZE, BUFFERS" } else { "FORMAT JSON" };
            let raw = first_cell(&query_table(s, &format!("EXPLAIN ({opts}) {sql}")).await?);
            Ok(postgres_plan(&raw, analyze))
        }
        Mysql => {
            let q = if analyze { format!("EXPLAIN ANALYZE {sql}") } else { format!("EXPLAIN FORMAT=TREE {sql}") };
            match query_table(s, &q).await {
                Ok(r) => {
                    let raw = first_cell(&r);
                    Ok(QueryPlan { analyzed: analyze, roots: arrow_tree(&raw), raw, ..Default::default() })
                }
                // MariaDB / old MySQL: the classic table.
                Err(e) if !analyze => {
                    let r = query_table(s, &format!("EXPLAIN {sql}")).await.map_err(|_| e)?;
                    Ok(QueryPlan { roots: table_rows(&r), raw: rows_text(&r), ..Default::default() })
                }
                Err(e) => Err(e),
            }
        }
        Sqlite => {
            let r = query_table(s, &format!("EXPLAIN QUERY PLAN {sql}")).await?;
            let col = |n: &str| r.0.iter().position(|c| c.eq_ignore_ascii_case(n));
            let (id, parent, detail) = (col("id").unwrap_or(0), col("parent").unwrap_or(1), col("detail").unwrap_or(3));
            let items = r.1.iter().map(|row| {
                let get = |i: usize| row.get(i).cloned().flatten().unwrap_or_default();
                (get(id), get(parent), PlanNode { op: get(detail), ..Default::default() })
            });
            Ok(QueryPlan { roots: by_parent(items, "0"), raw: rows_text(&r), ..Default::default() })
        }
        Duckdb => {
            let opts = if analyze { "ANALYZE, FORMAT JSON" } else { "FORMAT JSON" };
            let r = query_table(s, &format!("EXPLAIN ({opts}) {sql}")).await?;
            // (explain_key, explain_value): the JSON is in the last column.
            let raw = r.1.iter().filter_map(|row| row.last().cloned().flatten()).collect::<Vec<_>>().join("\n");
            Ok(duckdb_plan(&raw, analyze))
        }
        Mssql => {
            s.execute("SET SHOWPLAN_ALL ON", Default::default()).await?.collect().await?;
            let r = query_table(s, sql).await;
            // Always back off, also after an error.
            let off = async { s.execute("SET SHOWPLAN_ALL OFF", Default::default()).await?.collect().await };
            off.await?;
            let r = r?;
            Ok(QueryPlan { roots: mssql_rows(&r), raw: rows_text(&r), ..Default::default() })
        }
        Oracle => {
            let id = format!("DB{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
            s.execute(&format!("EXPLAIN PLAN SET STATEMENT_ID = '{id}' FOR {sql}"), Default::default()).await?.collect().await?;
            let r = query_table(
                s,
                &format!(
                    "SELECT id, parent_id, operation, options, object_owner, object_name, object_alias, cost, cardinality, bytes, access_predicates, filter_predicates, partition_start, partition_stop \
                     FROM plan_table WHERE statement_id = '{id}' ORDER BY id"
                ),
            )
            .await;
            let _ = s.execute(&format!("DELETE FROM plan_table WHERE statement_id = '{id}'"), Default::default()).await.map(|st| st.collect());
            let r = r?;
            Ok(QueryPlan { roots: oracle_rows(&r), raw: rows_text(&r), ..Default::default() })
        }
        Snowflake => {
            let raw = first_cell(&query_table(s, &format!("EXPLAIN USING JSON {sql}")).await?);
            Ok(snowflake_plan(&raw))
        }
        Databricks => {
            let raw = first_cell(&query_table(s, &format!("EXPLAIN FORMATTED {sql}")).await?);
            Ok(QueryPlan { raw, ..Default::default() })
        }
        _ => Err(databrain_connector_core::ConnectorError::unsupported("query plans")),
    }
}

fn num(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn scalar_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Array(a) if a.iter().all(|x| !x.is_object()) => Some(a.iter().filter_map(scalar_text).collect::<Vec<_>>().join(", ")),
        _ => None,
    }
}

fn postgres_plan(raw: &str, analyze: bool) -> QueryPlan {
    let v: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
    let top = v.get(0).cloned().unwrap_or(Value::Null);
    fn node(p: &Value) -> PlanNode {
        let s = |k: &str| p.get(k).and_then(Value::as_str);
        let mut object = String::new();
        if let Some(r) = s("Relation Name").or(s("CTE Name")).or(s("Function Name")) {
            object = format!("on {}{r}", s("Schema").map(|x| format!("{x}.")).unwrap_or_default());
            if let Some(a) = s("Alias").filter(|a| *a != r) {
                object.push_str(&format!(" {a}"));
            }
        }
        if let Some(i) = s("Index Name") {
            object = format!("using {i}{}{object}", if object.is_empty() { "" } else { " " });
        }
        let mut op = s("Node Type").unwrap_or("?").to_string();
        if let Some(j) = s("Join Type").filter(|j| *j != "Inner" || op.contains("Join") || op == "Nested Loop") {
            op = format!("{op} ({j})");
        }
        const SHOWN: &[&str] = &["Node Type", "Relation Name", "Schema", "Alias", "Index Name", "Join Type", "Startup Cost", "Total Cost", "Plan Rows", "Actual Rows", "Actual Total Time", "Actual Loops", "Plans", "CTE Name", "Function Name", "Parallel Aware", "Async Capable", "Plan Width", "Actual Startup Time"];
        let extra = p
            .as_object()
            .map(|o| o.iter().filter(|(k, _)| !SHOWN.contains(&k.as_str())).filter_map(|(k, v)| scalar_text(v).map(|t| (k.clone(), t))).collect())
            .unwrap_or_default();
        PlanNode {
            op,
            object: (!object.is_empty()).then_some(object),
            cost: num(p.get("Total Cost")),
            rows: num(p.get("Plan Rows")),
            actual_rows: num(p.get("Actual Rows")),
            time_ms: num(p.get("Actual Total Time")),
            loops: num(p.get("Actual Loops")),
            extra,
            children: p.get("Plans").and_then(Value::as_array).map(|a| a.iter().map(node).collect()).unwrap_or_default(),
        }
    }
    QueryPlan {
        analyzed: analyze,
        roots: top.get("Plan").map(|p| vec![node(p)]).unwrap_or_default(),
        raw: serde_json::to_string_pretty(&v).unwrap_or_else(|_| raw.to_string()),
        planning_ms: num(top.get("Planning Time")),
        execution_ms: num(top.get("Execution Time")),
    }
}

/// MySQL `EXPLAIN FORMAT=TREE` / `EXPLAIN ANALYZE`: lines `-> op  (cost=… rows=…) (actual time=a..b rows=… loops=…)`,
/// indented 4 spaces per level.
fn arrow_tree(raw: &str) -> Vec<PlanNode> {
    let mut stack: Vec<(usize, PlanNode)> = Vec::new();
    let mut roots = Vec::new();
    let attach = |stack: &mut Vec<(usize, PlanNode)>, roots: &mut Vec<PlanNode>, depth: usize| {
        while stack.last().is_some_and(|(d, _)| *d >= depth) {
            let (_, n) = stack.pop().unwrap();
            match stack.last_mut() {
                Some((_, p)) => p.children.push(n),
                None => roots.push(n),
            }
        }
    };
    for line in raw.lines() {
        let indent = line.len() - line.trim_start().len();
        let Some(body) = line.trim_start().strip_prefix("-> ") else {
            // Continuation of the previous step's text.
            if let Some((_, n)) = stack.last_mut() {
                if !line.trim().is_empty() {
                    n.op.push(' ');
                    n.op.push_str(line.trim());
                }
            }
            continue;
        };
        let depth = indent / 4;
        attach(&mut stack, &mut roots, depth);
        stack.push((depth, mysql_step(body)));
    }
    attach(&mut stack, &mut roots, 0);
    roots
}

fn mysql_step(body: &str) -> PlanNode {
    let mut n = PlanNode::default();
    let mut text = body;
    // Trailing "(…)" groups hold the numbers.
    while let Some(open) = text.rfind(" (") {
        let group = &text[open + 2..];
        let Some(inner) = group.strip_suffix(')') else { break };
        let kv: HashMap<&str, &str> = inner.split_whitespace().filter_map(|p| p.split_once('=')).collect();
        if inner.starts_with("cost=") || inner.starts_with("rows=") {
            n.cost = kv.get("cost").and_then(|v| v.parse().ok());
            n.rows = kv.get("rows").and_then(|v| v.parse().ok());
        } else if let Some(t) = inner.strip_prefix("actual time=") {
            n.time_ms = t.split_whitespace().next().and_then(|r| r.split("..").last()).and_then(|v| v.parse().ok());
            n.actual_rows = kv.get("rows").and_then(|v| v.parse().ok());
            n.loops = kv.get("loops").and_then(|v| v.parse().ok());
        } else if inner == "never executed" {
            n.loops = Some(0.0);
        } else {
            break;
        }
        text = text[..open].trim_end();
    }
    // "Table scan on o", "Index lookup on c using PRIMARY (id=o.customer_id)".
    match text.split_once(" on ") {
        Some((op, obj)) if !op.contains(':') => {
            n.op = op.trim().to_string();
            n.object = Some(format!("on {}", obj.trim()));
        }
        _ => match text.split_once(": ") {
            Some((op, cond)) => {
                n.op = op.trim().to_string();
                n.extra.push(("Condition".into(), cond.trim().to_string()));
            }
            None => n.op = text.trim().to_string(),
        },
    }
    n
}

fn duckdb_plan(raw: &str, analyze: bool) -> QueryPlan {
    let v: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
    fn node(p: &Value) -> PlanNode {
        let name = p.get("operator_name").or(p.get("name")).and_then(Value::as_str).unwrap_or("?").trim().to_string();
        let mut n = PlanNode { op: name, ..Default::default() };
        if let Some(extra) = p.get("extra_info").and_then(Value::as_object) {
            for (k, v) in extra {
                let t = scalar_text(v).unwrap_or_else(|| v.to_string());
                match k.as_str() {
                    "Table" => n.object = Some(format!("on {t}")),
                    "Estimated Cardinality" | "~Rows" => n.rows = t.trim_start_matches('~').trim().parse().ok(),
                    _ => n.extra.push((k.clone(), t)),
                }
            }
        }
        n.actual_rows = num(p.get("operator_cardinality"));
        n.time_ms = num(p.get("operator_timing")).map(|s| s * 1000.0);
        n.children = p.get("children").and_then(Value::as_array).map(|a| a.iter().map(node).collect()).unwrap_or_default();
        n
    }
    let execution_ms = num(v.get("latency")).map(|s| s * 1000.0);
    // ANALYZE wraps the plan in a query node; plain EXPLAIN is a list of roots.
    let roots: Vec<PlanNode> = match &v {
        Value::Array(a) => a.iter().map(node).collect(),
        Value::Object(_) => v.get("children").and_then(Value::as_array).map(|a| a.iter().map(node).collect()).unwrap_or_default(),
        _ => vec![],
    };
    QueryPlan { analyzed: analyze, roots, raw: serde_json::to_string_pretty(&v).unwrap_or_else(|_| raw.to_string()), execution_ms, ..Default::default() }
}

fn snowflake_plan(raw: &str) -> QueryPlan {
    let v: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
    let ops: Vec<&Value> = v.get("Operations").and_then(Value::as_array).map(|a| a.iter().flat_map(|g| g.as_array().into_iter().flatten()).collect()).unwrap_or_default();
    let items = ops.iter().map(|o| {
        let id = o.get("id").map(|x| x.to_string()).unwrap_or_default();
        let parent = o.get("parentOperators").and_then(Value::as_array).and_then(|a| a.first()).map(|x| x.to_string()).unwrap_or_else(|| "-".into());
        let mut n = PlanNode { op: o.get("operation").and_then(Value::as_str).unwrap_or("?").to_string(), ..Default::default() };
        if let Some(objs) = o.get("objects").and_then(scalar_text) {
            n.object = Some(format!("on {objs}"));
        }
        for k in ["expressions", "partitionsAssigned", "partitionsTotal", "bytesAssigned"] {
            if let Some(t) = o.get(k).and_then(scalar_text) {
                n.extra.push((k.into(), t));
            }
        }
        (id, parent, n)
    });
    QueryPlan { roots: by_parent(items, "-"), raw: serde_json::to_string_pretty(&v).unwrap_or_else(|_| raw.to_string()), ..Default::default() }
}

fn mssql_rows(r: &Rows) -> Vec<PlanNode> {
    let col = |n: &str| r.0.iter().position(|c| c.eq_ignore_ascii_case(n));
    let get = |row: &Vec<Option<String>>, n: &str| col(n).and_then(|i| row.get(i).cloned().flatten());
    let items = r.1.iter().filter(|row| get(row, "Type").is_none_or(|t| t != "SELECT" && t != "UPDATE" && t != "INSERT" && t != "DELETE") || get(row, "Parent").is_some_and(|p| p != "0")).map(|row| {
        let mut n = PlanNode {
            op: get(row, "PhysicalOp").or_else(|| get(row, "StmtText")).unwrap_or_default(),
            cost: get(row, "TotalSubtreeCost").and_then(|v| v.parse().ok()),
            rows: get(row, "EstimateRows").and_then(|v| v.parse().ok()),
            ..Default::default()
        };
        if let Some(l) = get(row, "LogicalOp").filter(|l| *l != n.op) {
            n.extra.push(("Logical".into(), l));
        }
        if let Some(a) = get(row, "Argument") {
            if let Some(obj) = a.split("OBJECT:(").nth(1).and_then(|x| x.split(')').next()) {
                n.object = Some(format!("on {obj}"));
            }
            n.extra.push(("Argument".into(), a));
        }
        (get(row, "NodeId").unwrap_or_default(), get(row, "Parent").unwrap_or_else(|| "0".into()), n)
    });
    by_parent(items, "0")
}

fn oracle_rows(r: &Rows) -> Vec<PlanNode> {
    let col = |n: &str| r.0.iter().position(|c| c.eq_ignore_ascii_case(n));
    let get = |row: &Vec<Option<String>>, n: &str| col(n).and_then(|i| row.get(i).cloned().flatten());
    let items = r.1.iter().map(|row| {
        let op = [get(row, "OPERATION"), get(row, "OPTIONS")].into_iter().flatten().collect::<Vec<_>>().join(" ");
        let mut n = PlanNode { op, cost: get(row, "COST").and_then(|v| v.parse().ok()), rows: get(row, "CARDINALITY").and_then(|v| v.parse().ok()), ..Default::default() };
        if let Some(name) = get(row, "OBJECT_NAME") {
            n.object = Some(format!("on {}{name}", get(row, "OBJECT_OWNER").map(|o| format!("{o}.")).unwrap_or_default()));
        }
        for (k, label) in [("ACCESS_PREDICATES", "Access"), ("FILTER_PREDICATES", "Filter"), ("BYTES", "Bytes"), ("PARTITION_START", "Partition start"), ("PARTITION_STOP", "Partition stop")] {
            if let Some(v) = get(row, k) {
                n.extra.push((label.into(), v));
            }
        }
        (get(row, "ID").unwrap_or_default(), get(row, "PARENT_ID").unwrap_or_else(|| "-".into()), n)
    });
    by_parent(items, "-")
}

/// Any other tabular plan: one step per row, its columns as details.
fn table_rows(r: &Rows) -> Vec<PlanNode> {
    r.1.iter()
        .map(|row| {
            let mut n = PlanNode::default();
            for (name, v) in r.0.iter().zip(row) {
                let Some(v) = v.clone() else { continue };
                match name.to_ascii_lowercase().as_str() {
                    "select_type" => n.op = v,
                    "table" => n.object = Some(format!("on {v}")),
                    "rows" => n.rows = v.parse().ok(),
                    _ => n.extra.push((name.clone(), v)),
                }
            }
            n
        })
        .collect()
}

/// Build trees from (id, parent id, node) rows in plan order; rows whose
/// parent is `root` or unknown become roots.
fn by_parent(items: impl Iterator<Item = (String, String, PlanNode)>, root: &str) -> Vec<PlanNode> {
    let items: Vec<(String, String, PlanNode)> = items.collect();
    let ids: std::collections::HashSet<&str> = items.iter().map(|(i, _, _)| i.as_str()).collect();
    let mut kids: HashMap<String, Vec<usize>> = HashMap::new();
    let mut tops = Vec::new();
    for (i, (id, parent, _)) in items.iter().enumerate() {
        if parent == root || !ids.contains(parent.as_str()) || parent == id {
            tops.push(i);
        } else {
            kids.entry(parent.clone()).or_default().push(i);
        }
    }
    let nodes: Vec<Arc<parking_lot::Mutex<Option<PlanNode>>>> = items.iter().map(|(_, _, n)| Arc::new(parking_lot::Mutex::new(Some(n.clone())))).collect();
    fn build(i: usize, items: &[(String, String, PlanNode)], kids: &HashMap<String, Vec<usize>>, nodes: &[Arc<parking_lot::Mutex<Option<PlanNode>>>], depth: usize) -> PlanNode {
        let mut n = nodes[i].lock().take().unwrap_or_default();
        if depth < 200 {
            for &k in kids.get(&items[i].0).map(Vec::as_slice).unwrap_or(&[]) {
                n.children.push(build(k, items, kids, nodes, depth + 1));
            }
        }
        n
    }
    tops.into_iter().map(|i| build(i, &items, &kids, &nodes, 0)).collect()
}

fn rows_text(r: &Rows) -> String {
    let mut out = r.0.join("\t");
    for row in &r.1 {
        out.push('\n');
        out.push_str(&row.iter().map(|v| v.as_deref().unwrap_or("")).collect::<Vec<_>>().join("\t"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_postgres_json() {
        let raw = r#"[{"Plan": {"Node Type": "Hash Join", "Join Type": "Inner", "Total Cost": 35.5, "Plan Rows": 10, "Actual Rows": 12, "Actual Total Time": 0.4, "Actual Loops": 1, "Hash Cond": "(o.customer_id = c.id)",
            "Plans": [{"Node Type": "Seq Scan", "Relation Name": "orders", "Schema": "public", "Alias": "o", "Total Cost": 20.0, "Plan Rows": 1000, "Filter": "(amount > 5)"},
                      {"Node Type": "Index Scan", "Index Name": "customers_pkey", "Relation Name": "customers", "Alias": "c", "Total Cost": 8.1, "Plan Rows": 1}]}, "Planning Time": 0.1, "Execution Time": 0.5}]"#;
        let p = postgres_plan(raw, true);
        assert_eq!((p.planning_ms, p.execution_ms), (Some(0.1), Some(0.5)));
        let root = &p.roots[0];
        assert_eq!((root.op.as_str(), root.cost, root.actual_rows), ("Hash Join (Inner)", Some(35.5), Some(12.0)));
        assert!(root.extra.contains(&("Hash Cond".into(), "(o.customer_id = c.id)".into())));
        assert_eq!(root.children[0].object.as_deref(), Some("on public.orders o"));
        assert_eq!(root.children[1].object.as_deref(), Some("using customers_pkey on customers c"));
    }

    #[test]
    fn reads_mysql_trees() {
        let raw = "-> Nested loop inner join  (cost=1.1 rows=3) (actual time=0.05..0.08 rows=3 loops=1)\n    -> Table scan on o  (cost=0.55 rows=3) (actual time=0.02..0.03 rows=3 loops=1)\n    -> Single-row index lookup on c using PRIMARY (id=o.customer_id)  (cost=0.18 rows=1) (actual time=0.01..0.01 rows=1 loops=3)\n-> Filter: (x > 1)  (cost=2 rows=1)";
        let r = arrow_tree(raw);
        assert_eq!(r.len(), 2);
        assert_eq!((r[0].op.as_str(), r[0].cost, r[0].time_ms, r[0].actual_rows), ("Nested loop inner join", Some(1.1), Some(0.08), Some(3.0)));
        assert_eq!(r[0].children.len(), 2);
        assert_eq!(r[0].children[1].object.as_deref(), Some("on c using PRIMARY (id=o.customer_id)"));
        assert_eq!(r[0].children[1].loops, Some(3.0));
        assert_eq!((r[1].op.as_str(), r[1].extra[0].1.as_str()), ("Filter", "(x > 1)"));
    }

    #[test]
    fn builds_trees_from_parent_ids() {
        let n = |op: &str| PlanNode { op: op.into(), ..Default::default() };
        let items = vec![("0".into(), "-".into(), n("SELECT STATEMENT")), ("1".into(), "0".into(), n("HASH JOIN")), ("2".into(), "1".into(), n("TABLE ACCESS FULL")), ("3".into(), "1".into(), n("INDEX UNIQUE SCAN"))];
        let r = by_parent(items.into_iter(), "-");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].children[0].children.iter().map(|c| c.op.as_str()).collect::<Vec<_>>(), ["TABLE ACCESS FULL", "INDEX UNIQUE SCAN"]);
    }

    #[test]
    fn reads_snowflake_json() {
        let raw = r#"{"GlobalStats":{"partitionsTotal":1},"Operations":[[{"id":0,"operation":"Result","expressions":["T.A"]},{"id":1,"parentOperators":[0],"operation":"TableScan","objects":["DB.S.T"],"partitionsAssigned":1}]]}"#;
        let p = snowflake_plan(raw);
        assert_eq!(p.roots[0].op, "Result");
        assert_eq!(p.roots[0].children[0].object.as_deref(), Some("on DB.S.T"));
    }
}
