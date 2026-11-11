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
    } else if !matches!(decision, Decision::Deny(_)) {
        match write_denied(profile) {
            // The AI may not write here (writes off, production…): a "read"
            // calling a user function can still write through it.
            Some(why) => decision = function_writes(profile, &pieces, &why).unwrap_or(decision),
            // It may: writes always ask, so a "read" that could write
            // (unknown functions, SQL that can't be parsed) asks too.
            None if decision == Decision::Allow => {
                let fns = could_write(profile, &pieces);
                if !fns.is_empty() {
                    decision = Decision::Ask(format!("This query calls {}, which could write", fns.join(", ")));
                }
            }
            None => {}
        }
    }
    // Tables the AI may read / write (checked after the connection-wide rules,
    // so their reasons come first).
    if !matches!(decision, Decision::Deny(_))
        && let Err(reason) = crate::access::check_sql(profile, sql)
    {
        decision = Decision::Deny(reason);
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

/// Functions that act outside the session's read-only mode: `dblink*` opens
/// its own (writable) connection, `set_config` changes settings such as
/// `search_path` (which table rules rely on), and server-admin and file
/// functions reach beyond the database. Lower case, without schema.
const OUTSIDE_READ_ONLY: &[&str] = &[
    "set_config",
    "pg_terminate_backend",
    "pg_cancel_backend",
    "pg_reload_conf",
    "pg_rotate_logfile",
    "pg_read_file",
    "pg_read_binary_file",
    "pg_ls_dir",
    "pg_file_write",
    "lo_import",
    "lo_export",
    "lo_unlink",
];

/// Functions `pieces` call that are not known built-ins (each once), or a
/// note that the SQL can't be parsed.
fn could_write(profile: &ConnectionProfile, pieces: &[databrain_connector_core::sql::StatementSpan]) -> Vec<String> {
    let mut fns: Vec<String> = Vec::new();
    for f in pieces.iter().flat_map(|p| databrain_connector_core::sql::unknown_functions(&p.sql, profile.config.kind)) {
        if !fns.contains(&f) {
            fns.push(f);
        }
    }
    fns
}

fn outside_read_only(f: &str) -> bool {
    let last = f.rsplit('.').next().unwrap_or(f).trim_matches(['"', '`', '[', ']']).to_ascii_lowercase();
    last.starts_with("dblink") || OUTSIDE_READ_ONLY.contains(&last.as_str())
}

/// `Confirm` when the SQL calls functions that are not known built-ins
/// (`SELECT dbo.write_fn()` is a read to the parser, but the function may
/// write): the AI may not write here (`why`), so only the user in DataBrain
/// may approve it, never automatically or by an MCP client.
///
/// A read-only session on the server (Postgres, MySQL, SQLite, DuckDB,
/// ClickHouse) stops writes in functions, but only on a connection set to
/// read-only: elsewhere the session can write, so every unknown function
/// counts. Functions acting outside that mode count even then.
fn function_writes(profile: &ConnectionProfile, pieces: &[databrain_connector_core::sql::StatementSpan], why: &str) -> Option<Decision> {
    let kind = profile.config.kind;
    let session_read_only = profile.config.read_only && kind.server_read_only();
    let mut fns: Vec<String> = Vec::new();
    for f in could_write(profile, pieces) {
        if session_read_only && !outside_read_only(&f) {
            continue;
        }
        fns.push(f);
    }
    (!fns.is_empty()).then(|| {
        let reason = if session_read_only {
            format!("this query calls {}, which acts outside {}'s read-only session ({why})", fns.join(", "), kind.traits().name)
        } else if kind.server_read_only() {
            format!("this query calls {}, which could write: the connection isn't read-only ({why})", fns.join(", "))
        } else {
            format!("{} has no read-only mode on the server, and this query calls {}, which could write ({why})", kind.traits().name, fns.join(", "))
        };
        Decision::Confirm(reason)
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

/// Whether a column name must be masked for this connection. A rule is
/// `column` (any table) or `[catalog.][schema.]table.column`. Table names
/// are compared part by part from the right, as far as both go: the rule
/// `public.users.email` matches SQL that writes `users` (it may be that
/// table: fail closed) or `public.users`, but not `sales.users`; the rule
/// `users.email` matches `crm.public.users`, not `superusers`.
pub fn is_pii(policy: &AiPolicy, table: Option<&str>, column: &str) -> bool {
    let c = unquote(column);
    policy.pii_columns.iter().any(|p| {
        let parts = name_parts(p);
        let Some((col, rule_table)) = parts.split_last() else { return false };
        if *col != c {
            return false;
        }
        rule_table.is_empty() || table.is_some_and(|t| same_table(rule_table, &name_parts(t)))
    })
}

/// The parts of a dotted name, unquoted and lower-cased (`"Public".users` → `public`, `users`).
pub fn name_parts(name: &str) -> Vec<String> {
    name.split('.').map(unquote).filter(|p| !p.is_empty()).collect()
}

fn unquote(s: &str) -> String {
    s.trim().trim_matches(|c| matches!(c, '"' | '`' | '[' | ']')).to_ascii_lowercase()
}

/// The same table when the parts both names have (counted from the right) agree.
pub fn same_table(a: &[String], b: &[String]) -> bool {
    let n = a.len().min(b.len());
    n > 0 && a[a.len() - n..] == b[b.len() - n..]
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

    /// "Allow write statements" doesn't lift production's refusal or the
    /// function check: on Prod a call that could write needs the user, T-SQL
    /// written without separators is not a read, and where the AI may write
    /// such SQL asks instead of running.
    #[test]
    fn writes_allowed_keeps_prod_and_function_checks() {
        use ConnectorKind::*;
        let writer = AiPolicy { allow_write: true, ..auto_read() };
        let mut prod = on(Mssql, false, writer.clone());
        prod.env = EnvTag::Prod;
        for sql in ["SELECT 1 EXEC xp_cmdshell 'whoami'", "select 1; exec xp_cmdshell 'whoami'", "SELECT 1 WAITFOR DELAY '00:00:05'"] {
            assert!(matches!(review_sql(&prod, sql).decision, Decision::Deny(_) | Decision::Confirm(_)), "prod: {sql}: {:?}", review_sql(&prod, sql).decision);
            assert_ne!(review_sql(&on(Mssql, false, writer.clone()), sql).decision, Decision::Allow, "dev, writes allowed: {sql}");
        }
        assert!(matches!(review_sql(&prod, "select dbo.write_fn()").decision, Decision::Confirm(r) if r.contains("production")));
        let mut pg_prod = on(Postgres, false, writer);
        pg_prod.env = EnvTag::Prod;
        // A call that acts is a write: the AI may not modify production.
        assert!(matches!(review_sql(&pg_prod, "select dblink_exec('x', 'delete from t')").decision, Decision::Deny(r) if r.contains("production")));
        assert_eq!(review_sql(&pg_prod, "select count(*) from t").decision, Decision::Allow);
        // Classification: each statement by its own keyword; unparsed T-SQL with EXEC isn't a read.
        assert_eq!(databrain_connector_core::sql::classify("select 1; exec p", Mssql).kind, StatementKind::Dml);
        assert_eq!(databrain_connector_core::sql::classify("SELECT 1 EXEC xp_cmdshell 'whoami'", Mssql).kind, StatementKind::Unknown);
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
            // The AI may write here: writes always ask, so a call that could write asks too.
            let writer = on(kind, false, AiPolicy { allow_write: true, ..auto_read() });
            assert!(matches!(review_sql(&writer, sql).decision, Decision::Ask(r) if r.contains(f)), "{kind:?}");
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

    /// Engines with a read-only session mode: it only protects connections
    /// set to read-only. Elsewhere a function call needs the user while the
    /// AI may not write; some functions act outside the mode and always do.
    #[test]
    fn functions_on_engines_with_a_read_only_mode() {
        use ConnectorKind::*;
        for kind in [Postgres, Mysql, Sqlite, Duckdb, Clickhouse] {
            for p in [on(kind, false, AiPolicy::default()), on(kind, false, auto_read())] {
                let sql = "select public.write_fn()";
                match review_sql(&p, sql).decision {
                    Decision::Confirm(r) => assert!(r.contains("could write") && r.contains("isn't read-only"), "{kind:?} {sql}: {r}"),
                    d => panic!("{kind:?} not read-only, writes off: {sql}: {d:?}"),
                }
                // Built-ins known to act are writes, refused while the AI may not write.
                for sql in ["select dblink_exec('dbname=x', 'delete from t')", "select set_config('search_path', 'hr', false)"] {
                    match review_sql(&p, sql).decision {
                        Decision::Deny(r) => assert!(r.contains("not allowed for the AI"), "{kind:?} {sql}: {r}"),
                        d => panic!("{kind:?} not read-only, writes off: {sql}: {d:?}"),
                    }
                }
            }
        }
        // A read-only Postgres session: ordinary functions are stopped by it…
        assert_eq!(review_sql(&on(Postgres, true, auto_read()), "select public.write_fn()").decision, Decision::Allow);
        // …but these act outside it: refused, like any write on a read-only connection.
        for sql in [
            "select dblink_exec('dbname=x', 'delete from t')",
            "select public.dblink('x', 'select 1')",
            "select set_config('search_path', 'hr', false)",
            "select pg_terminate_backend(42)",
        ] {
            match review_sql(&on(Postgres, true, auto_read()), sql).decision {
                Decision::Deny(r) => assert!(r.contains("read-only"), "{sql}: {r}"),
                d => panic!("read-only {sql}: {d:?}"),
            }
        }
        // Reading server files is still a read, but outside the read-only session: the user decides.
        match review_sql(&on(Postgres, true, auto_read()), "select pg_read_file('postgresql.conf')").decision {
            Decision::Confirm(r) => assert!(r.contains("outside") && r.contains("read-only session"), "{r}"),
            d => panic!("read-only pg_read_file: {d:?}"),
        }
        // The AI may write here: writes always ask, so a call that could write asks.
        assert!(matches!(
            review_sql(&on(Postgres, false, AiPolicy { allow_write: true, ..auto_read() }), "select public.write_fn()").decision,
            Decision::Ask(_)
        ));
        assert_eq!(review_sql(&on(Postgres, false, auto_read()), "select count(*), lower(name) from t").decision, Decision::Allow);
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

    #[test]
    fn pii_rules_with_a_schema_match_unqualified_sql() {
        let p = AiPolicy { pii_columns: vec!["public.users.email".into(), "crm.public.leads.phone".into()], ..Default::default() };
        // Unqualified or partly qualified: may be the rule's table, so masked.
        assert!(is_pii(&p, Some("users"), "email"));
        assert!(is_pii(&p, Some("public.users"), "email"));
        assert!(is_pii(&p, Some("\"Public\".\"Users\""), "EMAIL"));
        assert!(is_pii(&p, Some("leads"), "phone"));
        assert!(is_pii(&p, Some("public.leads"), "phone"));
        assert!(is_pii(&p, Some("crm.public.leads"), "phone"));
        // Another schema or table: not that rule's table.
        assert!(!is_pii(&p, Some("sales.users"), "email"));
        assert!(!is_pii(&p, Some("other.public.leads"), "phone"));
        assert!(!is_pii(&p, Some("superusers"), "email"));
        assert!(!is_pii(&p, None, "email"));
        // A table-less rule still matches anywhere.
        let any = AiPolicy { pii_columns: vec!["ssn".into(), "users.email".into()], ..Default::default() };
        assert!(is_pii(&any, Some("x.y.z"), "SSN") && is_pii(&any, Some("crm.public.users"), "email"));
    }
}
