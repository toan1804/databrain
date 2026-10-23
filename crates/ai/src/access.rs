//! Per-table access rules for the AI (`AiPolicy::read_allow` / `read_deny` /
//! `write_allow` / `write_deny`): which tables its SQL may read and change.
//!
//! Patterns are SQL LIKE patterns (`%` any text, `_` one character; `*` and
//! `?` work too), case-insensitive, quotes around them ignored. A table
//! matches when the pattern matches its full name (`catalog.schema.table`)
//! or a shorter form (`schema.table`, `table`). Unqualified names in SQL are
//! completed with the connection's default database and schema. Deny wins.
//!
//! Fail-closed: with rules set, SQL whose tables can't be told (unparsed,
//! procedure calls, other query languages), or that changes the default
//! schema (`USE`, `SET search_path`), is refused.

use databrain_connector_core::ConnectorKind;
use databrain_connector_core::tables::{TableName, table_refs};
use databrain_workspace::{AiPolicy, ConnectionProfile};

/// Whether any table rule is set.
pub fn has_rules(p: &AiPolicy) -> bool {
    [&p.read_allow, &p.read_deny, &p.write_allow, &p.write_deny].iter().any(|l| l.iter().any(|x| !clean(x).is_empty()))
}

fn has_list(l: &[String]) -> bool {
    l.iter().any(|x| !clean(x).is_empty())
}

/// A pattern without surrounding quotes and spaces, lowercased.
fn clean(p: &str) -> String {
    p.trim().trim_matches(|c| c == '\'' || c == '"' || c == '`').trim().to_ascii_lowercase()
}

/// Split a user-typed list (`'a%', 'b%'` or one per line) into patterns.
pub fn parse_patterns(text: &str) -> Vec<String> {
    text.split([',', '\n', ';']).map(clean).filter(|p| !p.is_empty()).collect()
}

/// SQL LIKE (`%`, `_`, also `*`, `?`), case-insensitive.
pub fn like(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '_' || p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && (p[pi] == '%' || p[pi] == '*') {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && (p[pi] == '%' || p[pi] == '*') {
        pi += 1;
    }
    pi == p.len()
}

/// Levels of names on this engine (table included) and the defaults that
/// complete an unqualified name, outermost first (`None` = not known).
fn defaults(profile: &ConnectionProfile) -> Vec<Option<String>> {
    let c = &profile.config;
    let opt = |k: &str| c.opt(k).map(str::trim).filter(|v| !v.is_empty()).map(str::to_string);
    let db = c.database.clone().filter(|d| !d.trim().is_empty());
    match c.kind {
        ConnectorKind::Postgres => vec![db.or(Some("postgres".into())), Some("public".into())],
        ConnectorKind::Mssql => vec![db.or(Some("master".into())), Some("dbo".into())],
        ConnectorKind::Snowflake => vec![db, opt("schema").or(Some("PUBLIC".into()))],
        ConnectorKind::Databricks => vec![opt("catalog"), opt("schema").or(Some("default".into()))],
        ConnectorKind::Bigquery => vec![opt("project"), opt("dataset")],
        ConnectorKind::Duckdb => {
            let file = c.file_path.as_deref().map(str::trim).filter(|f| !f.is_empty() && *f != ":memory:");
            let cat = file.and_then(|f| std::path::Path::new(f).file_stem().map(|s| s.to_string_lossy().to_string())).unwrap_or_else(|| "memory".into());
            vec![Some(cat), Some("main".into())]
        }
        ConnectorKind::Mysql => vec![db],
        ConnectorKind::Oracle => vec![profile.config.auth.user().map(|u| u.to_ascii_uppercase())],
        ConnectorKind::Sqlite => vec![Some("main".into())],
        ConnectorKind::Mongodb => vec![db],
        _ => vec![],
    }
}

/// The table's full name as far as it is known, and whether leading parts
/// (catalog or schema) stayed unknown.
fn qualify(profile: &ConnectionProfile, name: &TableName) -> (Vec<String>, bool) {
    let d = defaults(profile);
    let levels = d.len() + 1;
    if name.len() >= levels {
        return (name.clone(), false);
    }
    let need = levels - name.len();
    let mut full = Vec::new();
    let mut unknown = false;
    for v in d.iter().take(need) {
        match v {
            Some(v) if !unknown => full.push(v.clone()),
            Some(_) => {}
            None => {
                // Parts after an unknown one can't be placed either.
                unknown = true;
                full.clear();
            }
        }
    }
    full.extend(name.iter().cloned());
    (full, unknown)
}

#[derive(Debug, PartialEq, Eq)]
enum Match {
    Yes,
    No,
    /// A pattern names more levels than the known name has.
    Unknown,
}

fn matches_any(patterns: &[String], full: &[String], unknown: bool) -> Match {
    let forms: Vec<String> = (0..full.len()).map(|i| full[i..].join(".")).collect();
    let mut undecided = false;
    for p in patterns.iter().map(|p| clean(p)).filter(|p| !p.is_empty()) {
        if forms.iter().any(|f| like(&p, f)) {
            return Match::Yes;
        }
        // `cat.sch.%` (or a `%` that could cover the catalog) against a name
        // whose catalog is unknown: can't say.
        if unknown && (p.matches('.').count() + 1 > full.len() || p.contains(['%', '*'])) {
            undecided = true;
        }
    }
    if undecided { Match::Unknown } else { Match::No }
}

/// May the AI read (or write) this table? `Err` says why not.
fn allowed(profile: &ConnectionProfile, name: &TableName, write: bool) -> Result<(), String> {
    let p = &profile.ai_policy;
    let (allow, deny, what) = if write { (&p.write_allow, &p.write_deny, "write") } else { (&p.read_allow, &p.read_deny, "read") };
    let (full, unknown) = qualify(profile, name);
    let shown = full.join(".");
    let hint = "write the table's full name (catalog.schema.table): the access rules can't tell which catalog or schema it is in";
    match matches_any(deny, &full, unknown) {
        Match::Yes => return Err(format!("the AI may not {what} {shown} on this connection (it matches the {what} deny list)")),
        Match::Unknown => return Err(format!("{shown}: {hint}")),
        Match::No => {}
    }
    if has_list(allow) {
        match matches_any(allow, &full, unknown) {
            Match::Yes => {}
            Match::Unknown => return Err(format!("{shown}: {hint}")),
            Match::No => return Err(format!("the AI may only {what} tables matching {} on this connection; {shown} doesn't match", list(allow))),
        }
    }
    Ok(())
}

fn list(l: &[String]) -> String {
    l.iter().map(|p| clean(p)).filter(|p| !p.is_empty()).map(|p| format!("'{p}'")).collect::<Vec<_>>().join(", ")
}

/// Check the tables `sql` reads and writes against the connection's rules.
/// `Ok` when no rules are set.
pub fn check_sql(profile: &ConnectionProfile, sql: &str) -> Result<(), String> {
    if !has_rules(&profile.ai_policy) {
        return Ok(());
    }
    let r = table_refs(sql, profile.config.kind);
    if r.changes_default {
        return Err("changing the default database or schema (USE, SET search_path…) isn't allowed while table access rules are set; qualify the table names instead".into());
    }
    if let Some(k) = r.opaque.first() {
        return Err(format!("table access rules are set on this connection, and the tables a {k} statement uses can't be checked; use plain SELECT/INSERT/UPDATE/DELETE statements that name their tables"));
    }
    for t in &r.reads {
        allowed(profile, t, false)?;
    }
    for t in &r.writes {
        allowed(profile, t, true)?;
    }
    Ok(())
}

/// Check one table the AI would write (a job's load target, `schema.table`).
pub fn check_write_table(profile: &ConnectionProfile, table: &str) -> Result<(), String> {
    if !has_rules(&profile.ai_policy) {
        return Ok(());
    }
    let name: TableName = table.split('.').map(|p| p.trim().trim_matches(|c| c == '"' || c == '`' || c == '[' || c == ']').to_string()).filter(|p| !p.is_empty()).collect();
    allowed(profile, &name, true)
}

/// Check one table the AI would read by name (`catalog.schema.table`).
pub fn check_read_table(profile: &ConnectionProfile, table: &str) -> Result<(), String> {
    if !has_rules(&profile.ai_policy) {
        return Ok(());
    }
    let name: TableName = table.split('.').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect();
    allowed(profile, &name, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use databrain_auth::AuthMethod;
    use databrain_connector_core::ConnectionConfig;
    use databrain_workspace::EnvTag;

    fn profile(kind: ConnectorKind, db: Option<&str>, policy: AiPolicy) -> ConnectionProfile {
        let mut config = ConnectionConfig::new(kind, AuthMethod::Password { user: "app".into() });
        config.database = db.map(String::from);
        ConnectionProfile { id: "c".into(), name: "c".into(), config, color: None, env: EnvTag::Dev, folder_id: None, has_secret: false, ai_policy: policy, created_at: 0, updated_at: 0 }
    }

    #[test]
    fn like_patterns() {
        assert!(like("catalog_read%", "catalog_read_x.s.t"));
        assert!(like("%schema_can_read%", "c.my_schema_can_read.t"));
        assert!(like("sales._rders", "SALES.orders"));
        assert!(like("*.public.*", "db.public.t"));
        assert!(!like("cannot_read%", "can_read.t"));
        assert!(!like("a_c", "abbc"));
        assert_eq!(parse_patterns("'catalog_read%', '%schema_can_read%'\n  other "), ["catalog_read%", "%schema_can_read%", "other"]);
    }

    #[test]
    fn read_rules() {
        let pol = AiPolicy { read_allow: parse_patterns("'catalog_read%', '%schema_can_read%'"), read_deny: parse_patterns("'%secret%'"), ..Default::default() };
        let p = profile(ConnectorKind::Databricks, None, pol.clone());
        assert!(check_sql(&p, "select * from catalog_read_1.sales.orders").is_ok());
        assert!(check_sql(&p, "select * from other.my_schema_can_read.t").is_ok());
        let e = check_sql(&p, "select * from other.sales.orders").unwrap_err();
        assert!(e.contains("only read tables matching") && e.contains("other.sales.orders"), "{e}");
        // Deny wins over allow, also in a subquery or a join.
        assert!(check_sql(&p, "select * from catalog_read_1.sales.orders where id in (select id from catalog_read_1.hr.secret_pay)").unwrap_err().contains("deny"));
        // Writes are checked against the write rules (none here), reads still apply.
        assert!(check_sql(&p, "insert into anywhere.s.t select * from catalog_read_1.s.x").is_ok());
        assert!(check_sql(&p, "insert into anywhere.s.t select * from nope.s.x").is_err());
        // Unqualified name with an unknown catalog (no default catalog set): a
        // catalog-level pattern can't decide it.
        assert!(check_sql(&p, "select * from sales.orders").unwrap_err().contains("full name"));
        // With a default catalog, unqualified names are completed.
        let mut with_cat = p.clone();
        with_cat.config.options.insert("catalog".into(), "catalog_read_main".into());
        assert!(check_sql(&with_cat, "select * from orders").is_ok());
        assert!(check_sql(&p, "use catalog other").unwrap_err().contains("default database"));
        assert!(check_sql(&p, "call x()").unwrap_err().contains("can't be checked"));
        // No rules: anything goes (other checks still apply elsewhere).
        assert!(check_sql(&profile(ConnectorKind::Postgres, None, AiPolicy::default()), "call x()").is_ok());
    }

    #[test]
    fn write_rules_and_defaults() {
        let pol = AiPolicy { write_allow: parse_patterns("scratch.%"), write_deny: parse_patterns("scratch.keep%"), ..Default::default() };
        let p = profile(ConnectorKind::Postgres, Some("app"), pol);
        assert!(check_sql(&p, "update scratch.t set a = 1 where id = 1").is_ok());
        assert!(check_sql(&p, "delete from scratch.keep_me where id = 1").unwrap_err().contains("write deny"));
        // `orders` is app.public.orders: not in scratch.
        assert!(check_sql(&p, "insert into orders values (1)").unwrap_err().contains("app.public.orders"));
        assert!(check_sql(&p, "drop table scratch.x").is_ok());
        assert!(check_write_table(&p, "public.t").is_err());
        assert!(check_write_table(&p, "scratch.t").is_ok());
        // Reads are free here (no read rules).
        assert!(check_sql(&p, "select * from hr.salaries").is_ok());
        // Two-level engines: MySQL's database is the schema.
        let my = profile(ConnectorKind::Mysql, Some("shop"), AiPolicy { read_deny: parse_patterns("shop.pay%"), ..Default::default() });
        assert!(check_sql(&my, "select * from payroll").is_err());
        assert!(check_sql(&my, "select * from orders").is_ok());
        // Mongo: collections, lookups included.
        let mg = profile(ConnectorKind::Mongodb, Some("shop"), AiPolicy { read_deny: parse_patterns("%.users"), ..Default::default() });
        assert!(check_sql(&mg, "db.orders.aggregate([{$lookup: {from: 'users', localField: 'u', foreignField: '_id', as: 'u'}}])").is_err());
        assert!(check_sql(&mg, "db.orders.find({})").is_ok());
        // Neo4j can't be checked: refused while rules are set.
        let neo = profile(ConnectorKind::Neo4j, None, AiPolicy { read_deny: parse_patterns("x"), ..Default::default() });
        assert!(check_sql(&neo, "MATCH (n) RETURN n").is_err());
    }
}
