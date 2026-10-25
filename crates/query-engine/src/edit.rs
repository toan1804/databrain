//! Editing a result in the grid: changed cells, new rows and deleted rows
//! become UPDATE / INSERT / DELETE statements on the table the result was
//! read from, shown before they run and applied together.
//!
//! - A result is editable when its query reads one table (`SELECT … FROM t
//!   [WHERE …] [ORDER BY …]`: no join, grouping, DISTINCT, set operation or
//!   CTE), the table has a primary key and every key column is in the
//!   result. Columns that are expressions stay read-only.
//! - Rows are found by their key values as fetched; an UPDATE or DELETE that
//!   matches no row (changed meanwhile) or several fails the whole apply.
//! - The statements run in one transaction where the database has one
//!   (Oracle: one PL/SQL block), on the tab's session. Inside a transaction
//!   the user opened there, they join it (the user commits).

use databrain_connector_core::arrow::datatypes::SchemaRef;
use databrain_connector_core::sql::dialect_for;
use databrain_connector_core::{ConnectorKind, ExecOptions, ObjectKind, quote_ident, quote_path};
use databrain_export::load::edit_literal;
use serde::{Deserialize, Serialize};
use databrain_connector_core::sqlparser::ast::{Expr, GroupByExpr, Ident, ObjectNamePart, SelectItem, SetExpr, Statement, TableFactor};
use databrain_connector_core::sqlparser::parser::Parser;

use crate::{EngineError, OutputInfo, QueryEngine, Result};

/// The table behind an editable result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditTarget {
    pub connection_id: String,
    /// Schema id (as the explorer names it) and table.
    pub schema: String,
    pub table: String,
    /// Result column indexes of the primary key.
    pub key_columns: Vec<usize>,
    /// Per result column: the table column it shows (None: read-only expression).
    pub columns: Vec<Option<String>>,
    /// How the changes are applied: `transaction`, `block` (Oracle PL/SQL)
    /// or `statements` (one by one, where there are no transactions).
    pub apply: String,
}

/// One row's change, with the key values it had when fetched.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum RowChange {
    Update { key: Vec<Option<String>>, values: Vec<CellValue> },
    Insert { values: Vec<CellValue> },
    Delete { key: Vec<Option<String>> },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CellValue {
    /// Result column index.
    pub column: usize,
    /// None = NULL.
    pub value: Option<String>,
}

/// A statement of an apply and the rows it must touch (`None`: not checked).
#[derive(Debug, Clone, Serialize)]
pub struct EditStatement {
    pub sql: String,
    pub expect_rows: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ApplyReport {
    pub statements: usize,
    pub rows: u64,
    /// Ran inside the transaction already open on the session (not committed).
    pub in_open_transaction: bool,
}

/// The single table a query reads, and per projection item the column it
/// shows (`Some(None)` for `*`). `None` when the query is not that simple.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceTable {
    pub schema: Option<String>,
    pub table: String,
    /// `None` item: `*`; `Some(Some(c))`: column `c`; `Some(None)`: an expression.
    pub items: Vec<Option<Option<String>>>,
}

pub fn source_table(sql: &str, kind: ConnectorKind) -> Option<SourceTable> {
    let stmts = Parser::parse_sql(dialect_for(kind).as_ref(), sql).ok()?;
    let [Statement::Query(q)] = stmts.as_slice() else { return None };
    if q.with.is_some() {
        return None;
    }
    let SetExpr::Select(s) = q.body.as_ref() else { return None };
    if s.distinct.is_some() || s.having.is_some() || s.into.is_some() || s.from.len() != 1 || !s.from[0].joins.is_empty() {
        return None;
    }
    match &s.group_by {
        GroupByExpr::Expressions(e, _) if e.is_empty() => {}
        _ => return None,
    }
    let TableFactor::Table { name, alias, args: None, .. } = &s.from[0].relation else { return None };
    let parts: Vec<String> = name.0.iter().map(|p| match p {
        ObjectNamePart::Identifier(i) => Some(fold(kind, &i.value, i.quote_style.is_some())),
        _ => None,
    }).collect::<Option<_>>()?;
    let (schema, table) = match parts.as_slice() {
        [t] => (None, t.clone()),
        [s, t] => (Some(s.clone()), t.clone()),
        [c, s, t] => (Some(format!("{c}.{s}")), t.clone()),
        _ => return None,
    };
    let alias = alias.as_ref().map(|a| a.name.value.to_ascii_lowercase());
    let own = |q: &[Ident]| q.len() == 1 && (Some(q[0].value.to_ascii_lowercase()) == alias || q[0].value.eq_ignore_ascii_case(&table));
    let column = |e: &Expr| -> Option<String> {
        match e {
            Expr::Identifier(i) => Some(fold(kind, &i.value, i.quote_style.is_some())),
            Expr::CompoundIdentifier(p) if p.len() >= 2 && own(&p[p.len() - 2..p.len() - 1]) => p.last().map(|i| fold(kind, &i.value, i.quote_style.is_some())),
            _ => None,
        }
    };
    let items = s
        .projection
        .iter()
        .map(|it| match it {
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..) => None,
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => Some(column(e)),
            SelectItem::ExprWithAliases { .. } => Some(None),
        })
        .collect();
    Some(SourceTable { schema, table, items })
}

/// How the database stores an unquoted name (Oracle/Snowflake upper,
/// Postgres lower; others keep it and compare without case).
fn fold(kind: ConnectorKind, name: &str, quoted: bool) -> String {
    if quoted {
        return name.to_string();
    }
    match kind {
        ConnectorKind::Oracle | ConnectorKind::Snowflake => name.to_uppercase(),
        ConnectorKind::Postgres => name.to_lowercase(),
        _ => name.to_string(),
    }
}

/// Why an object that isn't a plain table can't be edited.
fn not_a_table(schema: &str, name: &str, kind: ObjectKind) -> String {
    let what = match kind {
        ObjectKind::View => "a view",
        ObjectKind::MaterializedView => "a materialized view",
        ObjectKind::ForeignTable => "a foreign table",
        ObjectKind::Function => "a function",
        ObjectKind::Procedure => "a procedure",
        ObjectKind::Package => "a package",
        ObjectKind::Sequence => "a sequence",
        ObjectKind::Table | ObjectKind::Other => "not a table",
    };
    format!("{schema}.{name} is {what}: only tables can be edited")
}

/// How changes are applied on a database (see [`EditTarget::apply`]).
pub(crate) fn apply_style(kind: ConnectorKind) -> Option<(&'static str, &'static str, &'static str)> {
    match kind {
        ConnectorKind::Postgres | ConnectorKind::Sqlite | ConnectorKind::Duckdb | ConnectorKind::Snowflake => Some(("BEGIN", "COMMIT", "ROLLBACK")),
        ConnectorKind::Mysql => Some(("START TRANSACTION", "COMMIT", "ROLLBACK")),
        ConnectorKind::Mssql => Some(("BEGIN TRANSACTION", "COMMIT", "ROLLBACK")),
        _ => None,
    }
}

/// A result's source table, described, with the table column of each result column.
pub(crate) struct ResolvedSource {
    pub kind: ConnectorKind,
    /// Schema id (as the explorer names it).
    pub schema: String,
    pub detail: databrain_connector_core::ObjectDetail,
    /// Per result column: the table column it shows (None: an expression).
    pub columns: Vec<Option<String>>,
}

impl QueryEngine {
    /// The one table a result's query reads, described, and which result
    /// column shows which of its columns. `Err` says why there is none.
    pub(crate) async fn resolve_source(&self, o: &OutputInfo, schema: &SchemaRef) -> Result<ResolvedSource> {
        let no = |why: &str| Err(EngineError::new("invalid", why.to_string()));
        let kind = self.workspace().get_connection(&o.connection_id)?.config.kind;
        if !kind.traits().language.is_sql() {
            return no("Only results of SQL databases have a source table");
        }
        let Some(src) = source_table(&o.sql, kind) else {
            return no("Only results of a query on one table (no join, grouping or DISTINCT) can be edited");
        };
        let schema_id = match src.schema.clone() {
            Some(s) => s,
            None => {
                let schemas = self.list_schemas(&o.connection_id).await?;
                match schemas.iter().find(|s| s.is_default).or(schemas.first()) {
                    Some(s) => s.name.clone(),
                    None => return no("The table's schema is not known"),
                }
            }
        };
        let detail = match self.describe(&o.connection_id, &schema_id, &src.table).await {
            Ok(d) => d,
            Err(_) => match self.describe(&o.connection_id, &schema_id, &src.table.to_uppercase()).await {
                Ok(d) => d,
                // Not described: say what it is if it exists (a view, a
                // function…) rather than "not found".
                Err(_) => {
                    let names = [src.table.clone(), src.table.to_uppercase(), src.table.to_lowercase()];
                    let found = self.list_objects_named(&o.connection_id, &schema_id, &names).await.ok().and_then(|objs| objs.into_iter().find(|x| x.name.eq_ignore_ascii_case(&src.table)));
                    return match found {
                        Some(obj) if obj.kind != ObjectKind::Table => no(&not_a_table(&schema_id, &obj.name, obj.kind)),
                        _ => no(&format!("Table {schema_id}.{} not found", src.table)),
                    };
                }
            },
        };
        let cols = &detail.columns;
        let find = |n: &str| cols.iter().find(|c| c.name.eq_ignore_ascii_case(n)).map(|c| c.name.clone());
        let names: Vec<String> = schema.fields().iter().map(|f| f.name().clone()).collect();
        let explicit = !src.items.is_empty() && src.items.iter().all(Option::is_some);
        let columns: Vec<Option<String>> = if explicit && src.items.len() == names.len() {
            src.items.iter().map(|it| it.clone().flatten().and_then(|c| find(&c))).collect()
        } else {
            // `*` (maybe with more items): by name.
            names.iter().map(|n| find(n)).collect()
        };
        Ok(ResolvedSource { kind, schema: schema_id, detail, columns })
    }

    /// The table a result can be edited in, or why it can't.
    pub async fn edit_target(&self, o: &OutputInfo, schema: &SchemaRef) -> Result<EditTarget> {
        let no = |why: &str| Err(EngineError::new("invalid", why.to_string()));
        let profile = self.workspace().get_connection(&o.connection_id)?;
        if profile.config.read_only {
            return no("The connection is read-only");
        }
        if !profile.config.kind.traits().language.is_sql() {
            return no("Only results of SQL databases can be edited");
        }
        let ResolvedSource { kind, schema: schema_id, detail, columns } = self.resolve_source(o, schema).await?;
        let table = detail.object.name.clone();
        if detail.object.kind != ObjectKind::Table {
            return no(&not_a_table(&schema_id, &table, detail.object.kind));
        }
        let cols = &detail.columns;
        let pk: Vec<&str> = cols.iter().filter(|c| c.is_primary_key).map(|c| c.name.as_str()).collect();
        if pk.is_empty() {
            return no(&format!("{table} has no primary key: rows can't be told apart"));
        }
        let mut key_columns = Vec::new();
        for k in &pk {
            match columns.iter().position(|c| c.as_deref().is_some_and(|c| c.eq_ignore_ascii_case(k))) {
                Some(i) => key_columns.push(i),
                None => return no(&format!("Add the key column {k} to the query to edit its rows")),
            }
        }
        let apply = match (apply_style(kind), kind) {
            (Some(_), _) => "transaction",
            (None, ConnectorKind::Oracle) => "block",
            _ => "statements",
        };
        Ok(EditTarget { connection_id: o.connection_id.clone(), schema: schema_id, table, key_columns, columns, apply: apply.into() })
    }

    /// The statements for `changes` (in order: updates, inserts, deletes as given).
    pub fn edit_statements(&self, t: &EditTarget, schema: &SchemaRef, changes: &[RowChange]) -> Result<Vec<EditStatement>> {
        let kind = self.workspace().get_connection(&t.connection_id)?.config.kind;
        let bad = |m: String| EngineError::new("invalid", m);
        let table = quote_path(kind, &format!("{}.{}", t.schema, t.table));
        let col = |i: usize| -> Result<(String, &databrain_connector_core::arrow::datatypes::Field)> {
            let name = t.columns.get(i).cloned().flatten().ok_or_else(|| bad(format!("Column {} is not a column of {}", schema.fields().get(i).map(|f| f.name().as_str()).unwrap_or("?"), t.table)))?;
            Ok((quote_ident(kind, &name), schema.field(i)))
        };
        let lit = |i: usize, v: Option<&str>| -> Result<String> { edit_literal(schema.field(i), v, kind).map_err(bad) };
        let key_where = |key: &[Option<String>]| -> Result<String> {
            if key.len() != t.key_columns.len() {
                return Err(bad("The row's key values are missing".into()));
            }
            let parts = t
                .key_columns
                .iter()
                .zip(key)
                .map(|(&i, v)| {
                    let (c, _) = col(i)?;
                    Ok(match v {
                        None => format!("{c} IS NULL"),
                        Some(v) => format!("{c} = {}", lit(i, Some(v))?),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(parts.join(" AND "))
        };
        let mut out = Vec::new();
        for ch in changes {
            match ch {
                RowChange::Update { key, values } => {
                    if values.is_empty() {
                        continue;
                    }
                    let sets = values.iter().map(|v| Ok(format!("{} = {}", col(v.column)?.0, lit(v.column, v.value.as_deref())?))).collect::<Result<Vec<_>>>()?;
                    out.push(EditStatement { sql: format!("UPDATE {table} SET {} WHERE {}", sets.join(", "), key_where(key)?), expect_rows: Some(1) });
                }
                RowChange::Insert { values } => {
                    let vals: Vec<&CellValue> = values.iter().filter(|v| v.value.is_some()).collect();
                    if vals.is_empty() {
                        out.push(EditStatement { sql: format!("INSERT INTO {table} DEFAULT VALUES"), expect_rows: Some(1) });
                        continue;
                    }
                    let names = vals.iter().map(|v| Ok(col(v.column)?.0)).collect::<Result<Vec<_>>>()?;
                    let lits = vals.iter().map(|v| lit(v.column, v.value.as_deref())).collect::<Result<Vec<_>>>()?;
                    out.push(EditStatement { sql: format!("INSERT INTO {table} ({}) VALUES ({})", names.join(", "), lits.join(", ")), expect_rows: Some(1) });
                }
                RowChange::Delete { key } => out.push(EditStatement { sql: format!("DELETE FROM {table} WHERE {}", key_where(key)?), expect_rows: Some(1) }),
            }
        }
        Ok(out)
    }

    /// Run the statements on the session `session_key` (the tab's), all or
    /// nothing where the database allows it.
    pub async fn apply_edits(&self, t: &EditTarget, session_key: &str, stmts: &[EditStatement]) -> Result<ApplyReport> {
        if stmts.is_empty() {
            return Ok(ApplyReport::default());
        }
        let kind = self.workspace().get_connection(&t.connection_id)?.config.kind;
        if self.jobs.lock().values().any(|j| j.session_key == session_key) {
            return Err(EngineError::new("busy", "A query is running in this tab: wait for it to finish, then apply"));
        }
        let session = self.session(&t.connection_id, session_key).await?;
        let run = |sql: String| {
            let s = session.clone();
            async move { s.execute(&sql, ExecOptions::default()).await?.collect().await.map(|c| c.summary.rows_affected) }
        };
        let in_tx = session.in_transaction();
        let mut report = ApplyReport { statements: stmts.len(), in_open_transaction: in_tx, ..Default::default() };
        let check = |i: usize, s: &EditStatement, n: Option<u64>| -> Result<()> {
            match (s.expect_rows, n) {
                (Some(want), Some(got)) if want != got => Err(EngineError::new(
                    "conflict",
                    if got == 0 {
                        format!("Change {}: the row was changed or deleted since it was read; nothing was saved. Refresh and edit again.", i + 1)
                    } else {
                        format!("Change {}: {got} rows matched the key instead of 1; nothing was saved.", i + 1)
                    },
                )),
                _ => Ok(()),
            }
        };
        if kind == ConnectorKind::Oracle && !in_tx {
            // One PL/SQL block: all or nothing (an error undoes the block).
            let mut body = String::from("BEGIN\n");
            for (i, s) in stmts.iter().enumerate() {
                body.push_str(&format!("  {};\n", s.sql));
                if let Some(n) = s.expect_rows {
                    body.push_str(&format!(
                        "  IF SQL%ROWCOUNT <> {n} THEN RAISE_APPLICATION_ERROR(-20001, 'Change {}: ' || SQL%ROWCOUNT || ' rows matched instead of {n} (changed meanwhile?); nothing was saved'); END IF;\n",
                        i + 1
                    ));
                }
            }
            body.push_str("END;");
            run(body).await.map_err(EngineError::from)?;
            report.rows = stmts.iter().filter_map(|s| s.expect_rows).sum();
            return Ok(report);
        }
        let style = apply_style(kind).filter(|_| !in_tx);
        if let Some((begin, _, _)) = style {
            run(begin.to_string()).await?;
        }
        for (i, s) in stmts.iter().enumerate() {
            let r = match run(s.sql.clone()).await {
                Ok(n) => check(i, s, n).map(|_| n),
                Err(e) => Err(e.into()),
            };
            match r {
                Ok(n) => report.rows += n.unwrap_or(0),
                Err(e) => {
                    if let Some((_, _, rollback)) = style {
                        let _ = run(rollback.to_string()).await;
                    }
                    let mut e = e;
                    if style.is_none() && !in_tx && i > 0 {
                        e.message.push_str(&format!(" (the {i} change(s) before it were saved: this database has no transactions)"));
                    }
                    return Err(e);
                }
            }
        }
        if let Some((_, commit, _)) = style {
            run(commit.to_string()).await?;
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ConnectorKind::*;

    fn src(sql: &str, k: ConnectorKind) -> Option<SourceTable> {
        source_table(sql, k)
    }
    fn t(schema: Option<&str>, table: &str, items: Vec<Option<Option<&str>>>) -> Option<SourceTable> {
        Some(SourceTable { schema: schema.map(Into::into), table: table.into(), items: items.into_iter().map(|i| i.map(|c| c.map(Into::into))).collect() })
    }

    #[test]
    fn finds_the_single_source_table() {
        assert_eq!(src("select * from public.orders where id > 3 order by id limit 10", Postgres), t(Some("public"), "orders", vec![None]));
        assert_eq!(src("select o.id, o.amount * 2, name as n from orders o", Postgres), t(None, "orders", vec![Some(Some("id")), Some(None), Some(Some("name"))]));
        assert_eq!(src("select id from emp", Oracle).unwrap().table, "EMP", "Oracle folds unquoted names up");
        assert_eq!(src("select \"Id\" from \"Mixed\"", Postgres).unwrap().table, "Mixed");
        for not in [
            "select * from a join b on a.id = b.id",
            "select country, count(*) from orders group by country",
            "select distinct country from orders",
            "with x as (select 1) select * from x",
            "select * from a union select * from b",
            "update t set a = 1",
            "select 1",
        ] {
            assert!(source_table(not, Postgres).is_none(), "{not}");
        }
    }
}
