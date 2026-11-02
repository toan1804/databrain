//! Policy engine: decides whether an AI tool call may run, needs the user's
//! approval, or is blocked. Enforced in code, never delegated to the model.

use databrain_connector_core::ConnectorKind;
use databrain_connector_core::sql::{Classification, StatementKind, classify, split_statements};
use databrain_workspace::{AiPolicy, ConnectionProfile, EnvTag, RunQueryPolicy};
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "decision", content = "reason", rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Ask(String),
    /// Like `Ask`, but only the user in DataBrain can approve it: an MCP
    /// client's own approval is not enough (it may approve tools blindly).
    Confirm(String),
    Deny(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct SqlReview {
    pub decision: Decision,
    pub statements: Vec<Classification>,
    /// Highest-risk classification across statements.
    pub kind: StatementKind,
}

/// Review SQL the AI wants to execute on a connection.
pub fn review_sql(profile: &ConnectionProfile, sql: &str) -> SqlReview {
    let kind = profile.config.kind;
    let policy = &profile.ai_policy;
    let pieces = split_statements(sql, kind);
    let stmts: Vec<Classification> = pieces.iter().map(|s| classify(&s.sql, kind)).collect();
    let worst = stmts.iter().map(|c| c.kind).max().unwrap_or(StatementKind::Unknown);
    let mut decision = decide(profile, policy, &stmts, worst);
    if profile.config.read_only && !matches!(decision, Decision::Deny(_)) {
        decision = read_only_decision(profile, &pieces, &stmts).unwrap_or(decision);
    } else if !policy.allow_write && !matches!(decision, Decision::Deny(_)) {
        // Not read-only, but the AI may not write: a "read" calling a user
        // function can still write through it.
        decision = function_writes(profile, &pieces, "the AI may not write on this connection").unwrap_or(decision);
    }
    // Tables the AI may read / write (checked after the connection-wide rules,
    // so their reasons come first).
    if !matches!(decision, Decision::Deny(_)) {
        if let Err(reason) = crate::access::check_sql(profile, sql) {
            decision = Decision::Deny(reason);
        }
    }
    SqlReview { decision, statements: stmts, kind: worst }
}

fn decide(profile: &ConnectionProfile, policy: &AiPolicy, stmts: &[Classification], worst: StatementKind) -> Decision {
    if !policy.ai_enabled {
        return Decision::Deny("AI is disabled for this connection".into());
    }
    if stmts.is_empty() {
        return Decision::Deny("no SQL statement".into());
    }
    if policy.run_query == RunQueryPolicy::Never {
        return Decision::Deny("running queries is disabled for the AI on this connection; propose the SQL instead".into());
    }
    let all_read = stmts.iter().all(|c| c.kind.is_read());
    if !all_read {
        let writes = stmts.iter().any(|c| c.kind.is_write() || c.kind == StatementKind::Unknown || c.kind == StatementKind::Other);
        if writes {
            if profile.config.read_only {
                return Decision::Deny("the connection is read-only".into());
            }
            if !policy.allow_write {
                return Decision::Deny(format!(
                    "{worst:?} statements are not allowed for the AI on this connection; write the SQL into the editor for the user to run"
                ));
            }
            if profile.env == EnvTag::Prod {
                return Decision::Deny("the AI may not modify a production connection".into());
            }
            let mw = stmts.iter().any(|c| c.missing_where);
            return Decision::Ask(if mw { "Writes data without a WHERE clause".into() } else { "Modifies data or schema".into() });
        }
        // Session / transaction statements.
        return Decision::Ask("Changes session or transaction state".into());
    }
    match policy.run_query {
        RunQueryPolicy::AutoRead => Decision::Allow,
        _ => Decision::Ask("Run read-only query".into()),
    }
}

/// Why the AI may not write to this connection at all, if it may not: the
/// rules [`review_sql`] applies to writes, for writes that are not SQL the
/// AI wrote (a job's load step inserting into a table).
pub fn write_denied(profile: &ConnectionProfile) -> Option<String> {
    let policy = &profile.ai_policy;
    if !policy.ai_enabled {
        return Some("AI is disabled for this connection".into());
    }
    if policy.run_query == RunQueryPolicy::Never {
        return Some("running queries is disabled for the AI on this connection".into());
    }
    if profile.config.read_only {
        return Some("the connection is read-only".into());
    }
    if !policy.allow_write {
        return Some("writes are not allowed for the AI on this connection".into());
    }
    if profile.env == EnvTag::Prod {
        return Some("the AI may not modify a production connection".into());
    }
    None
}

/// Read-only connections: what `decide` allowed may still be refused.
/// - Session and transaction statements are denied like writes (`SET
///   default_transaction_read_only = off`, `BEGIN READ WRITE`), except a
///   harmless `USE` or `SET` (see `read_only_allows`).
/// - Functions that could write need the user (see [`function_writes`]).
fn read_only_decision(profile: &ConnectionProfile, pieces: &[databrain_connector_core::sql::StatementSpan], stmts: &[Classification]) -> Option<Decision> {
    use databrain_connector_core::sql::read_only_allows;
    if let Some((_, c)) = pieces.iter().zip(stmts).find(|(p, c)| !read_only_allows(&p.sql, c)) {
        return Some(Decision::Deny(format!("the connection is read-only: {} statements are not run on it", c.keyword)));
    }
    function_writes(profile, pieces, "the connection is read-only")
}

/// `Confirm` when the database has no read-only session mode and the SQL
/// calls functions that are not known built-ins (`SELECT dbo.write_fn()`
/// is a read to the parser, but the function may write): the AI may not
/// write here (`why`), so only the user in DataBrain may approve it, never
/// automatically or by an MCP client.
fn function_writes(profile: &ConnectionProfile, pieces: &[databrain_connector_core::sql::StatementSpan], why: &str) -> Option<Decision> {
    use databrain_connector_core::sql::unknown_functions;
    let kind = profile.config.kind;
    if kind.server_read_only() {
        return None;
    }
    let mut fns: Vec<String> = Vec::new();
    for f in pieces.iter().flat_map(|p| unknown_functions(&p.sql, kind)) {
        if !fns.contains(&f) {
            fns.push(f);
        }
    }
    (!fns.is_empty()).then(|| {
        Decision::Confirm(format!(
            "{} has no read-only mode on the server, and this query calls {}, which could write ({why})",
            kind.traits().name,
            fns.join(", ")
        ))
    })
}

/// Wrap a read query with a row cap unless it already has one.
pub fn cap_rows(kind: ConnectorKind, sql: &str, limit: usize) -> String {
    let s = sql.trim().trim_end_matches(';').trim();
    let lower = s.to_ascii_lowercase();
    let has_limit = lower.split_whitespace().rev().take(4).any(|w| w == "limit" || w == "fetch" || w == "top");
    let first = lower.split_whitespace().next().unwrap_or("");
    // Wrapping an ordered query in a subquery does not keep its order in
    // most engines; the engine's row limit still stops the fetch at the cap.
    let ordered = lower.split_whitespace().collect::<Vec<_>>().windows(2).any(|w| w == ["order", "by"]);
    if has_limit || ordered || !(first == "select" || first == "with") {
        return s.to_string();
    }
    use databrain_connector_core::traits::RowLimit;
    if !kind.traits().language.is_sql() {
        // Other languages cap rows through the session's row limit.
        return s.to_string();
    }
    match kind.traits().row_limit {
        RowLimit::Top => format!("SELECT TOP ({limit}) * FROM (\n{s}\n) AS _q"),
        RowLimit::FetchFirst => format!("SELECT * FROM (\n{s}\n) WHERE ROWNUM <= {limit}"),
        RowLimit::Limit => format!("SELECT * FROM (\n{s}\n) AS _q LIMIT {limit}"),
    }
}

/// Whether a column name must be masked for this connection.
pub fn is_pii(policy: &AiPolicy, table: Option<&str>, column: &str) -> bool {
    policy.pii_columns.iter().any(|p| {
        let p = p.trim().to_ascii_lowercase();
        let c = column.to_ascii_lowercase();
        match p.rsplit_once('.') {
            Some((t, col)) => col == c && table.is_some_and(|tb| tb.to_ascii_lowercase().ends_with(t)),
            None => p == c,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use databrain_auth::AuthMethod;
    use databrain_connector_core::ConnectionConfig;

    fn profile(env: EnvTag, policy: AiPolicy) -> ConnectionProfile {
        ConnectionProfile {
            id: "c".into(),
            name: "c".into(),
            config: ConnectionConfig::new(ConnectorKind::Postgres, AuthMethod::Password { user: "u".into() }),
            color: None,
            env,
            folder_id: None,
            has_secret: false,
            ai_policy: policy,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn decisions() {
        let p = profile(EnvTag::Dev, AiPolicy::default());
        assert!(matches!(review_sql(&p, "select 1").decision, Decision::Ask(_)));
        assert!(matches!(review_sql(&p, "delete from t").decision, Decision::Deny(_)));
        let auto = profile(EnvTag::Dev, AiPolicy { run_query: RunQueryPolicy::AutoRead, ..Default::default() });
        assert_eq!(review_sql(&auto, "with x as (select 1) select * from x").decision, Decision::Allow);
        assert!(matches!(review_sql(&auto, "with d as (delete from t returning *) select * from d").decision, Decision::Deny(_)));
        let writer = profile(EnvTag::Dev, AiPolicy { allow_write: true, ..Default::default() });
        assert_eq!(review_sql(&writer, "update t set a=1").decision, Decision::Ask("Writes data without a WHERE clause".into()));
        let prod = profile(EnvTag::Prod, AiPolicy { allow_write: true, ..Default::default() });
        assert!(matches!(review_sql(&prod, "update t set a=1 where id=1").decision, Decision::Deny(_)));
        let never = profile(EnvTag::Dev, AiPolicy { run_query: RunQueryPolicy::Never, ..Default::default() });
        assert!(matches!(review_sql(&never, "select 1").decision, Decision::Deny(_)));
        // Table rules: an allowed read still runs (even automatically), a denied one never.
        let rules = profile(EnvTag::Dev, AiPolicy { run_query: RunQueryPolicy::AutoRead, read_deny: vec!["%.hr.%".into()], ..Default::default() });
        assert_eq!(review_sql(&rules, "select * from sales.orders").decision, Decision::Allow);
        assert!(matches!(review_sql(&rules, "select * from hr.pay").decision, Decision::Deny(r) if r.contains("hr.pay")));
    }

    /// EXPLAIN of a write is that write, in every form: on a production
    /// connection with auto-read it is refused, never run as a read.
    #[test]
    fn explain_of_a_write_is_a_write() {
        let prod = |policy: AiPolicy| {
            let mut p = on(ConnectorKind::Postgres, false, policy);
            p.env = EnvTag::Prod;
            p
        };
        for sql in [
            "EXPLAIN (ANALYZE) DELETE FROM t",
            "explain (analyze true, buffers) update t set a = 1",
            "explain analyze insert into t values (1)",
            "explain delete from t where id = 1",
        ] {
            assert!(matches!(review_sql(&prod(auto_read()), sql).decision, Decision::Deny(_)), "prod, auto-read: {sql}");
            assert!(matches!(review_sql(&prod(AiPolicy { allow_write: true, ..auto_read() }), sql).decision, Decision::Deny(_)), "prod: {sql}");
            assert!(matches!(review_sql(&on(ConnectorKind::Postgres, true, auto_read()), sql).decision, Decision::Deny(_)), "read-only: {sql}");
            assert!(matches!(review_sql(&on(ConnectorKind::Postgres, false, auto_read()), sql).decision, Decision::Deny(_)), "writes not allowed: {sql}");
            assert!(
                matches!(review_sql(&on(ConnectorKind::Postgres, false, AiPolicy { allow_write: true, ..auto_read() }), sql).decision, Decision::Ask(_)),
                "dev, writes allowed: asks: {sql}"
            );
        }
        assert_eq!(review_sql(&prod(auto_read()), "explain (analyze, buffers) select * from t").decision, Decision::Allow);
    }

    fn on(kind: ConnectorKind, read_only: bool, policy: AiPolicy) -> ConnectionProfile {
        let mut p = profile(EnvTag::Dev, policy);
        p.config.kind = kind;
        p.config.read_only = read_only;
        p
    }

    fn auto_read() -> AiPolicy {
        AiPolicy { run_query: RunQueryPolicy::AutoRead, ..Default::default() }
    }

    /// `SELECT dbo.write_fn()` parses as a read; where the server has no
    /// read-only session mode the function can still write.
    #[test]
    fn confirms_functions_that_could_write() {
        use ConnectorKind::*;
        let calls = [
            (Mssql, "select dbo.write_fn()", "dbo.write_fn"),
            (Snowflake, "select db.sch.write_fn(1)", "db.sch.write_fn"),
            (Databricks, "select main.default.write_fn(id) from t", "main.default.write_fn"),
            (Bigquery, "select ds.write_fn()", "ds.write_fn"),
            (Oracle, "select pkg.write_fn() from dual", "pkg.write_fn"),
        ];
        for (kind, sql, f) in calls {
            // The default posture (not read-only, allow_write off), even with automatic reads.
            for p in [on(kind, false, AiPolicy::default()), on(kind, false, auto_read()), on(kind, true, auto_read())] {
                match review_sql(&p, sql).decision {
                    Decision::Confirm(r) => assert!(r.contains(f) && r.contains("could write"), "{kind:?}: {r}"),
                    d => panic!("{kind:?} read_only={} {sql}: {d:?}", p.config.read_only),
                }
            }
            // Built-ins only: an ordinary read.
            assert_eq!(review_sql(&on(kind, false, auto_read()), "select count(*), lower(name) from t").decision, Decision::Allow, "{kind:?}");
            // The AI may write here: writes are its to make (with approval when they look like writes).
            let writer = on(kind, false, AiPolicy { allow_write: true, ..auto_read() });
            assert_eq!(review_sql(&writer, sql).decision, Decision::Allow, "{kind:?}");
        }
        // A table function counts too; each function is named once.
        match review_sql(&on(Mssql, false, auto_read()), "select * from dbo.tvf(1) a join dbo.tvf(2) b on 1=1").decision {
            Decision::Confirm(r) => assert_eq!(r.matches("dbo.tvf").count(), 1, "{r}"),
            d => panic!("{d:?}"),
        }
        // SQL that can't be parsed: assume the worst.
        assert!(matches!(review_sql(&on(Mssql, false, auto_read()), "select dbo.f( from").decision, Decision::Confirm(_) | Decision::Deny(_)));
        // A read-only session on the server stops writes in functions.
        assert_eq!(review_sql(&on(Postgres, true, auto_read()), "select public.write_fn()").decision, Decision::Allow);
    }

    #[test]
    fn read_only_connections() {
        use ConnectorKind::*;
        let ro = on(Postgres, true, AiPolicy { allow_write: true, ..auto_read() });
        for sql in [
            "delete from t where id = 1",
            "create table x (a int)",
            "set default_transaction_read_only = off",
            "begin read write",
            "commit",
            "select 1; update t set a = 1",
        ] {
            assert!(matches!(review_sql(&ro, sql).decision, Decision::Deny(_)), "{sql}");
        }
        assert_eq!(review_sql(&ro, "select * from t").decision, Decision::Allow);
        // Harmless settings are not refused (the user approves session changes).
        for sql in ["set search_path = sales", "set statement_timeout = 1000"] {
            assert!(matches!(review_sql(&ro, sql).decision, Decision::Ask(_)), "{sql}");
        }
        let sf = on(Snowflake, true, auto_read());
        assert!(matches!(review_sql(&sf, "use role accountadmin").decision, Decision::Deny(_)), "another role may write");
        assert!(!matches!(review_sql(&sf, "use warehouse wh").decision, Decision::Deny(_)));
        assert!(matches!(review_sql(&on(Mysql, true, auto_read()), "set session transaction read write").decision, Decision::Deny(_)));
        assert!(matches!(review_sql(&on(Duckdb, true, auto_read()), "attach 'x.db' as x").decision, Decision::Deny(_)));
        // Loads into a connection.
        assert!(write_denied(&ro).is_some_and(|r| r.contains("read-only")));
        assert!(write_denied(&on(Postgres, false, AiPolicy::default())).is_some_and(|r| r.contains("not allowed")));
        let mut prod = on(Postgres, false, AiPolicy { allow_write: true, ..Default::default() });
        assert_eq!(write_denied(&prod), None);
        prod.env = EnvTag::Prod;
        assert!(write_denied(&prod).is_some_and(|r| r.contains("production")));
        assert!(write_denied(&on(Postgres, false, AiPolicy { allow_write: true, ai_enabled: false, ..Default::default() })).is_some());
    }

    #[test]
    fn caps_rows() {
        assert_eq!(cap_rows(ConnectorKind::Postgres, "select * from t;", 50), "SELECT * FROM (\nselect * from t\n) AS _q LIMIT 50");
        assert_eq!(cap_rows(ConnectorKind::Postgres, "select * from t limit 5", 50), "select * from t limit 5");
        assert!(cap_rows(ConnectorKind::Mssql, "select 1", 10).starts_with("SELECT TOP (10)"));
        assert_eq!(cap_rows(ConnectorKind::Postgres, "show tables", 10), "show tables");
        // Ordered queries are not wrapped (a subquery would drop the order).
        assert_eq!(cap_rows(ConnectorKind::Duckdb, "select * from t order by 2 desc", 10), "select * from t order by 2 desc");
    }

    #[test]
    fn pii() {
        let p = AiPolicy { pii_columns: vec!["users.email".into(), "ssn".into()], ..Default::default() };
        assert!(is_pii(&p, Some("public.users"), "EMAIL"));
        assert!(!is_pii(&p, Some("orders"), "email"));
        assert!(is_pii(&p, None, "ssn"));
    }
}
