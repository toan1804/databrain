//! Learning from query history: which tables join on which columns, and
//! which tables are used. A pass reads the connection's history entries
//! after its cursor, finds the joins (`databrain_lineage::join_pairs`, or a
//! MongoDB `$lookup`) and tables of each successful one, resolves every
//! table to the one object it names, and records sightings in the
//! workspace. Nothing here talks to the database.
//!
//! What counts is decided by who ran a query, not who wrote it: the user's
//! runs (AI-written SQL run from the editor included) and jobs count; runs
//! the assistant made itself (AI panel, MCP, AI job steps) are recorded
//! apart and never count as evidence.

use std::cell::RefCell;
use std::collections::HashMap;

use databrain_connector_core::{ConnectorKind, ForeignKey};
use databrain_lineage::{CatalogTable, JoinPair};
use databrain_workspace::{ConnectionProfile, HistoryEntry, JoinSighting, JoinStatus, LearnedJoin, RunStatus, Workspace};
use serde::Serialize;

use crate::access::defaults;
use crate::types::Result;

/// History entries read per pass (a pass holds no lock in between).
pub const PASS_LIMIT: u32 = 500;
/// Longer SQL is not parsed (generated scripts, data loads).
pub const MAX_SQL_BYTES: usize = 200_000;
/// A learned join is given to the AI once seen in this many different
/// queries, or on this many different days.
pub const SURFACE_QUERIES: i64 = 2;
pub const SURFACE_DAYS: i64 = 2;
/// Sightings older than this are dropped.
pub const KEEP_DAYS: u32 = 365;

#[derive(Debug, Clone, Default, PartialEq, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct LearnReport {
    /// History entries read.
    pub entries: usize,
    /// Joins and table uses recorded.
    pub joins: usize,
    pub tables: usize,
    /// The cursor after the pass.
    #[ts(as = "f64")]
    pub last_history_id: i64,
    /// No more history to read.
    pub done: bool,
}

/// Can history on this kind of database be learned from?
pub fn supported(kind: ConnectorKind) -> bool {
    kind.traits().language.is_sql() || kind == ConnectorKind::Mongodb
}

/// Is a learned join good enough to give to the AI?
pub fn surfaced(j: &LearnedJoin) -> bool {
    match j.status {
        JoinStatus::Confirmed => true,
        JoinStatus::Rejected => false,
        JoinStatus::Learned => documented(j) || j.queries >= SURFACE_QUERIES || j.days >= SURFACE_DAYS,
    }
}

/// Written down (a document, the user) and approved: given to the AI
/// without being seen in queries.
pub fn documented(j: &LearnedJoin) -> bool {
    j.documented.as_ref().is_some_and(|d| d.state == databrain_workspace::DocState::Approved)
}

/// Learn from up to `limit` history entries after the connection's cursor.
/// With learning off, nothing is read and the cursor stays (turning it on
/// later learns from the history kept so far). On a kind of database that
/// can't be learned from, the cursor moves to the end.
pub fn learn_pass(ws: &Workspace, connection_id: &str, limit: u32) -> Result<LearnReport> {
    let profile = ws.get_connection(connection_id)?;
    let cursor = ws.learn_state(connection_id)?.map(|s| s.last_history_id).unwrap_or(0);
    let mut report = LearnReport { last_history_id: cursor, ..Default::default() };
    if !profile.ai_policy.learn_from_history {
        report.done = true;
        return Ok(report);
    }
    let entries = ws.history_since(connection_id, cursor, limit)?;
    report.entries = entries.len();
    report.done = entries.len() < limit as usize;
    let kind = profile.config.kind;
    let names = Resolver::new(ws, &profile);
    for h in &entries {
        report.last_history_id = h.id;
        if !supported(kind) || h.status != RunStatus::Success || h.sql.len() > MAX_SQL_BYTES {
            continue;
        }
        let fingerprint = databrain_lineage::fingerprint(&h.sql, kind);
        for (left, left_columns, right, right_columns, join_kind) in joins_of(&h.sql, kind, &names) {
            let declared = names.declared(&left, &left_columns, &right, &right_columns) || names.declared(&right, &right_columns, &left, &left_columns);
            let s = JoinSighting {
                left_table: left,
                left_columns,
                right_table: right,
                right_columns,
                kind: join_kind,
                declared,
                fingerprint: fingerprint.clone(),
                history_id: h.id,
                at: h.started_at,
                origin: h.origin,
            };
            ws.record_join(connection_id, &s)?;
            report.joins += 1;
        }
        for t in tables_of(h, kind, &names) {
            ws.record_table_use(connection_id, &t, &fingerprint, h.started_at, h.origin)?;
            report.tables += 1;
        }
    }
    ws.set_learn_state(connection_id, report.last_history_id, None)?;
    if report.done {
        ws.prune_learned(KEEP_DAYS)?;
    }
    Ok(report)
}

/// Learn again from all the history kept, from the start: after indexing,
/// tables that couldn't be placed before may be known now. Sightings are
/// per query and day, so nothing is counted twice; statuses are kept.
pub fn relearn(ws: &Workspace, connection_id: &str) -> Result<LearnReport> {
    ws.set_learn_state(connection_id, 0, None)?;
    learn_all(ws, connection_id)
}

/// Learn until the connection's history is used up, `PASS_LIMIT` entries at
/// a time (for tests and "learn now").
pub fn learn_all(ws: &Workspace, connection_id: &str) -> Result<LearnReport> {
    let mut total = LearnReport::default();
    loop {
        let r = learn_pass(ws, connection_id, PASS_LIMIT)?;
        total.entries += r.entries;
        total.joins += r.joins;
        total.tables += r.tables;
        total.last_history_id = r.last_history_id;
        if r.done {
            total.done = true;
            return Ok(total);
        }
    }
}

type Join = (String, Vec<String>, String, Vec<String>, String);

/// Joins of one query between resolved tables: (left, cols, right, cols, kind).
fn joins_of(sql: &str, kind: ConnectorKind, names: &Resolver<'_>) -> Vec<Join> {
    let mut out = Vec::new();
    if kind == ConnectorKind::Mongodb {
        for j in databrain_connector_core::language::mongo_joins(sql).unwrap_or_default() {
            if let (Some(l), Some(r)) = (names.resolve(&j.local), names.resolve(&j.from)) {
                // `$lookup` keeps every document of the aggregated collection.
                out.push((l, vec![j.local_field], r, vec![j.foreign_field], "left".into()));
            }
        }
        return out;
    }
    let pairs: Vec<JoinPair> = {
        let catalog = |parts: &[String]| names.catalog(parts);
        databrain_lineage::join_pairs(sql, kind, &catalog)
    };
    for p in pairs {
        let (Some(l), Some(r)) = (names.resolve(&split(&p.left)), names.resolve(&split(&p.right))) else { continue };
        out.push((l, p.left_columns, r, p.right_columns, p.kind.as_str().to_string()));
    }
    out
}

/// Tables (and collections) a query reads, resolved.
fn tables_of(h: &HistoryEntry, kind: ConnectorKind, names: &Resolver<'_>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in databrain_connector_core::tables::table_refs(&h.sql, kind).reads {
        if let Some(full) = names.resolve(&t)
            && !out.contains(&full)
        {
            out.push(full);
        }
    }
    out
}

fn split(name: &str) -> Vec<String> {
    name.split('.').map(str::to_string).collect()
}

/// Table names as written → the one table they name (`schema.name` as the
/// knowledge index or explorer cache has it), cached for a pass. A bare
/// name found in several schemas is only placed when one of them is the
/// connection's default; otherwise it is not placed (never guessed).
struct Resolver<'a> {
    ws: &'a Workspace,
    connection_id: String,
    /// Default catalog/schema levels, outermost first (see `access::defaults`).
    defaults: Vec<Option<String>>,
    cache: RefCell<HashMap<String, Option<(String, String)>>>,
    columns: RefCell<HashMap<String, Option<Vec<String>>>>,
}

impl<'a> Resolver<'a> {
    fn new(ws: &'a Workspace, profile: &ConnectionProfile) -> Self {
        Self { ws, connection_id: profile.id.clone(), defaults: defaults(profile), cache: RefCell::default(), columns: RefCell::default() }
    }

    /// `schema` is (the tail of) the connection's defaults.
    fn is_default(&self, schema: &str) -> bool {
        crate::access::is_default_schema(&self.defaults, schema)
    }

    fn lookup(&self, parts: &[String]) -> Option<(String, String)> {
        let parts: Vec<String> = parts.iter().map(|p| p.trim_matches(|c| c == '"' || c == '`' || c == '[' || c == ']').to_string()).collect();
        let key = parts.join(".").to_lowercase();
        if let Some(hit) = self.cache.borrow().get(&key) {
            return hit.clone();
        }
        let found = (|| {
            let (name, qual) = parts.split_last()?;
            let qual = qual.join(".");
            let mut cands = self.ws.tables_named(&self.connection_id, name).ok()?;
            if !qual.is_empty() {
                let q = qual.to_lowercase();
                cands.retain(|(s, _)| {
                    let s = s.to_lowercase();
                    s == q || s.ends_with(&format!(".{q}"))
                });
            }
            if cands.len() > 1 {
                cands.retain(|(s, _)| self.is_default(s));
            }
            match <[_; 1]>::try_from(cands) {
                Ok([one]) => Some(one),
                Err(_) => None,
            }
        })();
        self.cache.borrow_mut().insert(key, found.clone());
        found
    }

    /// `schema.name` of the one table `parts` names.
    fn resolve(&self, parts: &[String]) -> Option<String> {
        self.lookup(parts).map(|(s, n)| format!("{s}.{n}"))
    }

    /// Table columns for lineage (views are not expanded: no server here).
    fn catalog(&self, parts: &[String]) -> Option<CatalogTable> {
        let (schema, name) = self.lookup(parts)?;
        let full = format!("{schema}.{name}");
        let cached = self.columns.borrow().get(&full).cloned();
        let columns = match cached {
            Some(c) => c,
            None => {
                let c = crate::catalog::AiTables::new(self.ws, &self.connection_id).ok().and_then(|t| t.column_names(&schema, &name).ok().flatten());
                self.columns.borrow_mut().insert(full.clone(), c.clone());
                c
            }
        }?;
        Some(CatalogTable { name: full, columns, view_sql: None })
    }

    /// `table(cols)` has a declared foreign key to `ref_table(ref_cols)`.
    fn declared(&self, table: &str, cols: &[String], ref_table: &str, ref_cols: &[String]) -> bool {
        let Some(o) = crate::catalog::AiTables::new(self.ws, &self.connection_id).ok().and_then(|t| t.get(table).ok().flatten()) else { return false };
        let same = |a: &[String], b: &[String]| a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y));
        o.foreign_keys.iter().any(|fk: &ForeignKey| {
            format!("{}.{}", fk.ref_schema, fk.ref_table).eq_ignore_ascii_case(ref_table) && same(&fk.columns, cols) && same(&fk.ref_columns, ref_cols)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use databrain_auth::AuthMethod;
    use databrain_connector_core::{ColumnInfo, ConnectionConfig};
    use databrain_workspace::{AiPolicy, EnvTag, KnObject, NewHistory, Origin};

    const DAY: i64 = 86_400_000;

    /// Day `d` of the last 200 days (sightings older than a year are pruned).
    fn t(d: i64) -> i64 {
        (databrain_workspace::now_ms().div_euclid(DAY) - 200 + d) * DAY
    }

    fn obj(schema: &str, name: &str, cols: &[&str], fks: Vec<ForeignKey>) -> KnObject {
        KnObject {
            schema: schema.into(),
            name: name.into(),
            kind: "table".into(),
            comment: None,
            row_estimate: None,
            columns: cols
                .iter()
                .map(|c| ColumnInfo { name: c.to_string(), data_type: "int".into(), nullable: true, is_primary_key: false, default: None, comment: None })
                .collect(),
            foreign_keys: fks,
        }
    }

    fn setup_kind(kind: ConnectorKind) -> (Workspace, String) {
        let ws = Workspace::open_in_memory().unwrap();
        let mut config = ConnectionConfig::new(kind, AuthMethod::Password { user: "me".into() });
        config.database = Some("app".into());
        let p = ws
            .save_connection(ConnectionProfile {
                id: String::new(),
                name: "db".into(),
                config,
                color: None,
                env: EnvTag::Dev,
                folder_id: None,
                has_secret: false,
                ai_policy: AiPolicy::default(),
                created_at: 0,
                updated_at: 0,
            })
            .unwrap();
        let fk = ForeignKey { columns: vec!["order_id".into()], ref_schema: "sales".into(), ref_table: "orders".into(), ref_columns: vec!["id".into()] };
        ws.kn_upsert_objects(
            &p.id,
            &[
                obj("sales", "orders", &["id", "customer_ref", "status"], vec![]),
                obj("sales", "customers", &["id", "cust_no", "name"], vec![]),
                obj("sales", "order_items", &["order_id", "product_id"], vec![fk]),
                // `events` in two schemas, neither the default (`public`).
                obj("raw", "events", &["id", "user_id"], vec![]),
                obj("staging", "events", &["id", "user_id"], vec![]),
                // `users` in two schemas, one of them the default.
                obj("public", "users", &["id", "email"], vec![]),
                obj("archive", "users", &["id", "email"], vec![]),
            ],
        )
        .unwrap();
        (ws, p.id)
    }

    fn setup() -> (Workspace, String) {
        setup_kind(ConnectorKind::Postgres)
    }

    fn run_at(ws: &Workspace, conn: &str, sql: &str, origin: Origin, at: i64, ok: bool) -> i64 {
        ws.add_history(NewHistory {
            origin,
            connection_id: Some(conn.into()),
            sql: sql.into(),
            started_at: at,
            status: Some(if ok { RunStatus::Success } else { RunStatus::Error }),
            ..Default::default()
        })
        .unwrap()
    }

    fn run(ws: &Workspace, conn: &str, sql: &str, origin: Origin) -> i64 {
        run_at(ws, conn, sql, origin, t(100), true)
    }

    const Q1: &str = "select o.id, c.name from sales.orders o join sales.customers c on c.cust_no = o.customer_ref";
    const Q2: &str = "select count(*) from orders o left join customers c on o.customer_ref = c.cust_no where o.status = 'paid'";

    fn joins(ws: &Workspace, conn: &str) -> Vec<LearnedJoin> {
        ws.learned_joins(conn).unwrap()
    }

    #[test]
    fn one_user_query_is_learned_but_not_yet_given_to_the_ai() {
        let (ws, c) = setup();
        run(&ws, &c, Q1, Origin::User);
        let r = learn_all(&ws, &c).unwrap();
        assert_eq!((r.entries, r.joins, r.done), (1, 1, true));
        let j = &joins(&ws, &c)[0];
        assert_eq!((j.left_table.as_str(), j.left_columns.as_slice()), ("sales.customers", &["cust_no".to_string()][..]));
        assert_eq!((j.right_table.as_str(), j.right_columns.as_slice()), ("sales.orders", &["customer_ref".to_string()][..]));
        assert_eq!((j.queries, j.days), (1, 1));
        assert!(!surfaced(j));
    }

    #[test]
    fn two_different_queries_surface_it() {
        let (ws, c) = setup();
        run(&ws, &c, Q1, Origin::User);
        // Unqualified names, resolved because each exists in one schema only.
        run(&ws, &c, Q2, Origin::User);
        learn_all(&ws, &c).unwrap();
        let j = &joins(&ws, &c)[0];
        assert_eq!((j.queries, j.days), (2, 1));
        assert_eq!(j.kinds.get("inner"), Some(&1));
        // `orders LEFT JOIN customers`, stored customers-first: `right`.
        assert_eq!(j.kinds.get("right"), Some(&1));
        assert!(surfaced(j));
    }

    #[test]
    fn the_same_query_on_two_days_surfaces_it_and_other_values_are_the_same_query() {
        let (ws, c) = setup();
        let q = |status: &str| format!("{Q1} where o.status = '{status}'");
        run_at(&ws, &c, &q("paid"), Origin::User, t(100), true);
        run_at(&ws, &c, &q("open"), Origin::User, t(100) + 10, true);
        learn_all(&ws, &c).unwrap();
        assert!(!surfaced(&joins(&ws, &c)[0]), "one query (other values), one day");
        run_at(&ws, &c, &q("paid"), Origin::User, t(101), true);
        learn_all(&ws, &c).unwrap();
        let j = &joins(&ws, &c)[0];
        assert_eq!((j.queries, j.days), (1, 2));
        assert!(surfaced(j));
    }

    #[test]
    fn the_assistants_own_runs_are_recorded_but_never_count() {
        let (ws, c) = setup();
        run(&ws, &c, Q1, Origin::Ai);
        run_at(&ws, &c, Q2, Origin::Mcp, t(101), true);
        learn_all(&ws, &c).unwrap();
        let j = &joins(&ws, &c)[0];
        assert_eq!((j.queries, j.days, j.ai_queries), (0, 0, 2));
        assert!(!surfaced(j));
        assert!(ws.table_usage(&c).unwrap().is_empty());
    }

    #[test]
    fn ai_written_sql_run_from_the_editor_counts() {
        // The editor records every run as the user's, whoever wrote the SQL.
        let (ws, c) = setup();
        run(&ws, &c, Q1, Origin::User);
        run(&ws, &c, Q2, Origin::User);
        learn_all(&ws, &c).unwrap();
        assert!(surfaced(&joins(&ws, &c)[0]));
    }

    #[test]
    fn a_job_repeating_one_query_all_day_counts_once() {
        let (ws, c) = setup();
        for i in 0..10 {
            run_at(&ws, &c, Q1, Origin::Job, t(100) + i * 300_000, true);
        }
        learn_all(&ws, &c).unwrap();
        let j = &joins(&ws, &c)[0];
        assert_eq!((j.queries, j.days), (1, 1));
        assert!(!surfaced(j));
    }

    #[test]
    fn failed_runs_and_huge_sql_are_skipped() {
        let (ws, c) = setup();
        run_at(&ws, &c, Q1, Origin::User, t(100), false);
        let huge = format!("{Q2} /* {} */", "x".repeat(MAX_SQL_BYTES));
        run(&ws, &c, &huge, Origin::User);
        let r = learn_all(&ws, &c).unwrap();
        assert_eq!((r.entries, r.joins), (2, 0));
        assert!(joins(&ws, &c).is_empty());
    }

    #[test]
    fn passes_resume_at_the_cursor() {
        let (ws, c) = setup();
        let h1 = run(&ws, &c, Q1, Origin::User);
        let h2 = run(&ws, &c, Q2, Origin::User);
        let r = learn_pass(&ws, &c, 1).unwrap();
        assert_eq!((r.entries, r.last_history_id, r.done), (1, h1, false));
        assert_eq!(ws.connections_to_learn().unwrap(), std::slice::from_ref(&c));
        let r = learn_pass(&ws, &c, 1).unwrap();
        assert_eq!((r.entries, r.last_history_id), (1, h2));
        let r = learn_pass(&ws, &c, 1).unwrap();
        assert_eq!((r.entries, r.done), (0, true));
        assert!(ws.connections_to_learn().unwrap().is_empty());
        assert_eq!(joins(&ws, &c)[0].queries, 2, "each entry learned once");
    }

    #[test]
    fn tables_that_cant_be_placed_are_not_learned() {
        let (ws, c) = setup();
        // Unknown table.
        run(&ws, &c, "select * from sales.orders o join nowhere.things t on t.order_id = o.id", Origin::User);
        // `events` is in two schemas, neither the default.
        run(&ws, &c, "select * from events e join sales.customers c on c.id = e.user_id", Origin::User);
        learn_all(&ws, &c).unwrap();
        assert!(joins(&ws, &c).is_empty());
        // Qualified, it is placed.
        run(&ws, &c, "select * from raw.events e join sales.customers c on c.id = e.user_id", Origin::User);
        learn_all(&ws, &c).unwrap();
        // Stored in sorted order: `raw.events` < `sales.customers`.
        assert_eq!(joins(&ws, &c)[0].left_table, "raw.events");
    }

    #[test]
    fn a_bare_name_in_several_schemas_is_the_default_schemas() {
        let (ws, c) = setup();
        run(&ws, &c, "select * from users u join sales.customers c on c.id = u.id", Origin::User);
        learn_all(&ws, &c).unwrap();
        assert_eq!(joins(&ws, &c)[0].left_table, "public.users");
    }

    #[test]
    fn a_declared_foreign_key_is_marked() {
        let (ws, c) = setup();
        run(&ws, &c, "select * from sales.order_items i join sales.orders o on o.id = i.order_id", Origin::User);
        run(&ws, &c, Q1, Origin::User);
        learn_all(&ws, &c).unwrap();
        let all = joins(&ws, &c);
        let fk = all.iter().find(|j| j.left_table == "sales.order_items").unwrap();
        assert!(fk.declared);
        assert!(!all.iter().find(|j| j.left_table == "sales.customers").unwrap().declared);
    }

    #[test]
    fn tables_read_are_counted_as_usage() {
        let (ws, c) = setup();
        run(&ws, &c, Q1, Origin::User);
        run(&ws, &c, "select * from sales.orders where id = 1", Origin::User);
        learn_all(&ws, &c).unwrap();
        let u: Vec<(String, i64)> = ws.table_usage(&c).unwrap().into_iter().map(|t| (t.table, t.queries)).collect();
        assert_eq!(u, [("sales.orders".to_string(), 2), ("sales.customers".to_string(), 1)]);
    }

    #[test]
    fn with_learning_off_nothing_is_read_and_turning_it_on_learns_the_backlog() {
        let (ws, c) = setup();
        let mut p = ws.get_connection(&c).unwrap();
        p.ai_policy.learn_from_history = false;
        ws.save_connection(p.clone()).unwrap();
        run(&ws, &c, Q1, Origin::User);
        let r = learn_all(&ws, &c).unwrap();
        assert_eq!((r.entries, r.last_history_id), (0, 0));
        assert!(joins(&ws, &c).is_empty());
        p.ai_policy.learn_from_history = true;
        ws.save_connection(p).unwrap();
        assert_eq!(learn_all(&ws, &c).unwrap().joins, 1);
    }

    #[test]
    fn databases_without_sql_or_lookups_move_the_cursor_and_learn_nothing() {
        let (ws, c) = setup_kind(ConnectorKind::Neo4j);
        let h = run(&ws, &c, "MATCH (a:Order)-[:BY]->(b:Customer) RETURN a, b", Origin::User);
        let r = learn_all(&ws, &c).unwrap();
        assert_eq!((r.last_history_id, r.joins, r.tables), (h, 0, 0));
        assert!(ws.connections_to_learn().unwrap().is_empty());
        assert!(!supported(ConnectorKind::Neo4j));
        assert!(supported(ConnectorKind::Mongodb) && supported(ConnectorKind::Duckdb));
    }

    #[test]
    fn surfacing_rules() {
        let base = LearnedJoin {
            id: 1,
            left_table: "a".into(),
            left_columns: vec![],
            right_table: "b".into(),
            right_columns: vec![],
            status: JoinStatus::Learned,
            declared: false,
            queries: 1,
            days: 1,
            ai_queries: 9,
            kinds: Default::default(),
            first_seen: 0,
            last_seen: None,
            documented: None,
        };
        assert!(!surfaced(&base));
        assert!(surfaced(&LearnedJoin { queries: 2, ..base.clone() }));
        assert!(surfaced(&LearnedJoin { days: 2, ..base.clone() }));
        assert!(surfaced(&LearnedJoin { status: JoinStatus::Confirmed, queries: 0, days: 0, ..base.clone() }));
        assert!(!surfaced(&LearnedJoin { status: JoinStatus::Rejected, queries: 50, days: 50, ..base.clone() }));
        // Documented: given without use once approved, never when rejected.
        let doc = |state| Some(databrain_workspace::JoinDoc { source: "spec".into(), by: "ai".into(), state, at: 0 });
        assert!(surfaced(&LearnedJoin { queries: 0, days: 0, documented: doc(databrain_workspace::DocState::Approved), ..base.clone() }));
        assert!(!surfaced(&LearnedJoin { queries: 0, days: 0, documented: doc(databrain_workspace::DocState::Proposed), ..base.clone() }));
        assert!(!surfaced(&LearnedJoin { status: JoinStatus::Rejected, documented: doc(databrain_workspace::DocState::Approved), ..base }));
    }
}
