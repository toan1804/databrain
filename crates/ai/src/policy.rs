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
    let stmts: Vec<Classification> = split_statements(sql, kind).iter().map(|s| classify(&s.sql, kind)).collect();
    let worst = stmts.iter().map(|c| c.kind).max().unwrap_or(StatementKind::Unknown);
    let decision = decide(profile, policy, &stmts, worst);
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
    match kind {
        ConnectorKind::Mssql => format!("SELECT TOP ({limit}) * FROM (\n{s}\n) AS _q"),
        ConnectorKind::Oracle => format!("SELECT * FROM (\n{s}\n) WHERE ROWNUM <= {limit}"),
        _ => format!("SELECT * FROM (\n{s}\n) AS _q LIMIT {limit}"),
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
