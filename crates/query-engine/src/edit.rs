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
use databrain_connector_core::sqlparser::ast::{
    Expr, GroupByExpr, Ident, ObjectNamePart, SelectItem, SelectItemQualifiedWildcardKind, SetExpr, Statement, TableFactor, WildcardAdditionalOptions,
};
use databrain_connector_core::sqlparser::parser::Parser;
use databrain_connector_core::{ConnectorKind, DbObject, ExecOptions, ObjectDetail, ObjectKind, quote_ident, quote_object};
use databrain_export::load::edit_literal;
use serde::{Deserialize, Serialize};

use crate::{EngineError, OutputInfo, QueryEngine, Result};

/// The table behind an editable result.
#[derive(Debug, Clone, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
#[ts(optional_fields = nullable)]
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
#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[ts(export)]
#[ts(optional_fields = nullable)]
pub struct EditStatement {
    pub sql: String,
    #[ts(as = "Option<f64>")]
    pub expect_rows: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct ApplyReport {
    pub statements: usize,
    #[ts(as = "f64")]
    pub rows: u64,
    /// Ran inside the transaction already open on the session (not committed).
    pub in_open_transaction: bool,
}

/// The single table a query reads, and per projection item the column it
/// shows (`None` for `*`). `None` when the query is not that simple.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceTable {
    pub schema: Option<String>,
    pub table: String,
    /// `None` item: `*`; `Some(Some(c))`: column `c`; `Some(None)`: an expression.
    pub items: Vec<Option<Option<String>>>,
    /// Which result column shows which table column can't be told from the
    /// items: a `*` that replaces or renames columns (`* REPLACE (…)`,
    /// `* RENAME (…)`), `(expr).*`, or an item that may expand to several
    /// columns (DuckDB `COLUMNS(…)`, `UNPACK(…)`). Every column is read-only.
    pub opaque: bool,
}

/// The one table a result's query reads, by name as written (`schema`:
/// `None` when unqualified). Keys the grid's per-table settings; nothing is
/// looked up in the database.
#[derive(Debug, Clone, PartialEq, Serialize, ts_rs::TS)]
#[ts(export)]
#[ts(optional_fields = nullable)]
pub struct ResultSource {
    pub connection_id: String,
    pub schema: Option<String>,
    pub table: String,
}

/// [`ResultSource`] of a result's query (SQL sources, one table, no join or grouping).
pub fn result_source(o: &OutputInfo, kind: ConnectorKind) -> Option<ResultSource> {
    if !kind.traits().language.is_sql() {
        return None;
    }
    source_table(&o.sql, kind).map(|s| ResultSource { connection_id: o.connection_id.clone(), schema: s.schema, table: s.table })
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
    let parts: Vec<String> = name
        .0
        .iter()
        .map(|p| match p {
            ObjectNamePart::Identifier(i) => Some(fold(kind, &i.value, i.quote_style.is_some())),
            _ => None,
        })
        .collect::<Option<_>>()?;
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
    let mut opaque = false;
    // A `*` that changes what its columns show, or is not this table's.
    let mut wildcard = |o: &WildcardAdditionalOptions, ours: bool| {
        opaque |= !ours || o.opt_replace.is_some() || o.opt_rename.is_some() || o.opt_alias.is_some();
        None
    };
    let items = s
        .projection
        .iter()
        .map(|it| match it {
            SelectItem::Wildcard(o) => wildcard(o, true),
            SelectItem::QualifiedWildcard(SelectItemQualifiedWildcardKind::ObjectName(n), o) => {
                let ours = match n.0.as_slice() {
                    [ObjectNamePart::Identifier(i)] => own(std::slice::from_ref(i)),
                    _ => false,
                };
                wildcard(o, ours)
            }
            SelectItem::QualifiedWildcard(_, o) => wildcard(o, false),
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => Some(column(e)),
            SelectItem::ExprWithAliases { .. } => Some(None),
        })
        .collect();
    opaque |= s.projection.iter().any(|it| match it {
        SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } | SelectItem::ExprWithAliases { expr: e, .. } => expands(e),
        _ => false,
    });
    Some(SourceTable { schema, table, items, opaque })
}

/// An expression that may show several columns (DuckDB `COLUMNS(…)`, `UNPACK(…)`, `COLUMNS(*)`).
fn expands(e: &Expr) -> bool {
    match e {
        Expr::Function(f) => f.name.0.last().is_some_and(|p| match p {
            ObjectNamePart::Identifier(i) => i.value.eq_ignore_ascii_case("columns") || i.value.eq_ignore_ascii_case("unpack"),
            _ => true,
        }),
        Expr::Nested(e) => expands(e),
        _ => false,
    }
}

/// Per result column (`names`), the table column (of `table`) it shows, or
/// `None`: an expression, or not known for sure (then read-only: an edit
/// must never write to a column the cell doesn't show).
///
/// - Only plain items (no `*`): one result column each, by position.
/// - One `*`: the items before and after it by position; the columns of
///   the `*` (what is left between them) by name, and only when the table
///   columns among them are in table order (else the alignment is not
///   trusted); a name the table description lacks is read-only.
/// - Anything else (`*, *`, a count that doesn't add up): all read-only.
///
/// A table column shown twice is editable only in its first result column.
pub(crate) fn map_columns(src: &SourceTable, names: &[String], table: &[String]) -> Vec<Option<String>> {
    let none = vec![None; names.len()];
    if src.opaque {
        return none;
    }
    let find = |n: &str| table.iter().position(|c| c == n).or_else(|| table.iter().position(|c| c.eq_ignore_ascii_case(n)));
    let item = |it: &Option<Option<String>>| it.clone().flatten().and_then(|c| find(&c));
    let stars: Vec<usize> = src.items.iter().enumerate().filter(|(_, it)| it.is_none()).map(|(i, _)| i).collect();
    let mut idx: Vec<Option<usize>> = match stars.as_slice() {
        [] if src.items.len() == names.len() => src.items.iter().map(item).collect(),
        [star] if names.len() + 1 >= src.items.len() => {
            let (before, after) = (&src.items[..*star], &src.items[star + 1..]);
            let n = names.len() + 1 - src.items.len();
            let mid: Vec<Option<usize>> = names[before.len()..before.len() + n].iter().map(|c| find(c)).collect();
            // In table order: what a `*` (maybe with EXCLUDE / EXCEPT /
            // ILIKE) shows. A column the description leaves out (SQLite's
            // generated columns) stays read-only; the `*`'s span is counted
            // exactly, so it is the table's own column, not a computed one.
            let ordered = mid.iter().try_fold(None::<usize>, |prev, c| match (prev, c) {
                (_, None) => Ok(prev),
                (Some(p), Some(c)) if *c <= p => Err(()),
                (_, Some(c)) => Ok(Some(*c)),
            });
            if ordered.is_err() {
                return none;
            }
            before.iter().map(item).chain(mid).chain(after.iter().map(item)).collect()
        }
        _ => return none,
    };
    let mut seen = std::collections::HashSet::new();
    for c in idx.iter_mut() {
        if c.is_some_and(|i| !seen.insert(i)) {
            *c = None;
        }
    }
    idx.into_iter().map(|c| c.map(|i| table[i].clone())).collect()
}

/// `INSERT` of a row with every column at its default. `DEFAULT VALUES`
/// where the database has it; MySQL takes an empty column list; Oracle,
/// Snowflake, BigQuery, Databricks and ClickHouse take the `DEFAULT`
/// keyword for one column (the others get their defaults as well).
pub(crate) fn insert_defaults(kind: ConnectorKind, table: &str, column: &str) -> String {
    match kind {
        ConnectorKind::Postgres | ConnectorKind::Sqlite | ConnectorKind::Mssql | ConnectorKind::Duckdb => format!("INSERT INTO {table} DEFAULT VALUES"),
        ConnectorKind::Mysql => format!("INSERT INTO {table} () VALUES ()"),
        _ => format!("INSERT INTO {table} ({column}) VALUES (DEFAULT)"),
    }
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

/// How changes are applied on a database (see [`EditTarget::apply`]): its
/// transaction statements (`SourceTraits::transactions`).
pub(crate) fn apply_style(kind: ConnectorKind) -> Option<(&'static str, &'static str, &'static str)> {
    kind.traits().transactions.map(|t| (t.begin, t.commit, t.rollback))
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
    /// Describe the source table (as written, then upper case). When that
    /// fails, an unreachable database says so; else what the object is (a
    /// view, a function…) or that it is not there.
    async fn describe_source(&self, cid: &str, schema_id: &str, table: &str) -> Result<ObjectDetail> {
        let unreachable = |e: &EngineError| matches!(e.kind.as_str(), "connection" | "auth" | "config" | "cancelled");
        let first = match self.describe(cid, schema_id, table).await {
            Ok(d) => return Ok(d),
            Err(e) if unreachable(&e) => return Err(EngineError { message: format!("Can't read {schema_id}.{table}: {}", e.message), ..e }),
            Err(e) => e,
        };
        if table.to_uppercase() != table
            && let Ok(d) = self.describe(cid, schema_id, &table.to_uppercase()).await
        {
            return Ok(d);
        }
        let names = [table.to_string(), table.to_uppercase(), table.to_lowercase()];
        let found = self.list_objects_named(cid, schema_id, &names).await.ok().and_then(|objs| objs.into_iter().find(|x| x.name.eq_ignore_ascii_case(table)));
        Err(match found {
            Some(obj) if obj.kind != ObjectKind::Table => EngineError::new("invalid", not_a_table(schema_id, &obj.name, obj.kind)),
            Some(_) => EngineError::new("invalid", format!("Can't read {schema_id}.{table}: {}", first.message)),
            None => EngineError::new("invalid", format!("Table {schema_id}.{table} not found")),
        })
    }

    /// The one table a result's query reads, described, and which result
    /// column shows which of its columns. `Err` says why there is none.
    /// `cached_columns`: they may come from the explorer's cache (when it has
    /// them, not stale); foreign keys are not cached.
    pub(crate) async fn resolve_source(&self, o: &OutputInfo, schema: &SchemaRef, cached_columns: bool) -> Result<ResolvedSource> {
        let no = |why: &str| Err(EngineError::new("invalid", why.to_string()));
        let kind = self.workspace().get_connection(&o.connection_id)?.config.kind;
        if !kind.traits().language.is_sql() {
            return no("Only results of SQL databases have a source table");
        }
        let Some(src) = source_table(&o.sql, kind) else {
            return no("Only results of a query on one table (no join, grouping or DISTINCT) can be edited");
        };
        let cid = o.connection_id.as_str();
        let ws = self.workspace();
        let mut schema_id = match src.schema.clone() {
            Some(s) => s,
            None => {
                // The explorer's cached schema list first: no round trip.
                let schemas = match ws.meta_schemas(cid, None).ok().flatten().filter(|l| !l.is_empty()) {
                    Some(l) => l,
                    None => self.list_schemas(cid).await?,
                };
                match schemas.iter().find(|s| s.is_default).or(schemas.first()) {
                    Some(s) => s.name.clone(),
                    None => return no("The table's schema is not known"),
                }
            }
        };
        // What the explorer has seen of it: its kind (a view is refused
        // without asking the database) and, for edits, its columns.
        let cached = ws.meta_object(cid, &schema_id, &src.table).ok().flatten().filter(|x| x.kind.is_relation());
        if let Some(obj) = &cached {
            if obj.kind != ObjectKind::Table {
                return no(&not_a_table(&obj.schema, &obj.name, obj.kind));
            }
            schema_id = obj.schema.clone();
        }
        let from_cache = cached.as_ref().filter(|_| cached_columns).and_then(|obj| {
            let c = ws.meta_columns(cid, &obj.schema, &obj.name).ok().flatten()?;
            (!c.stale && !c.columns.is_empty()).then(|| ObjectDetail { object: obj.clone(), columns: c.columns, ddl: None, foreign_keys: vec![] })
        });
        let detail = match from_cache {
            Some(d) => d,
            None => {
                let d = self.describe_source(cid, &schema_id, cached.as_ref().map_or(src.table.as_str(), |x| x.name.as_str())).await?;
                // Remember it like the explorer does: the next Edit (and the
                // check when saving) needs no round trip.
                if !d.columns.is_empty() {
                    let _ = ws.meta_add_objects(cid, &[DbObject { schema: schema_id.clone(), ..d.object.clone() }]);
                    let _ = ws.meta_put_table(cid, &schema_id, &d.object.name, &d.columns, Some(&d.foreign_keys));
                }
                d
            }
        };
        let table: Vec<String> = detail.columns.iter().map(|c| c.name.clone()).collect();
        let names: Vec<String> = schema.fields().iter().map(|f| f.name().clone()).collect();
        let columns = map_columns(&src, &names, &table);
        Ok(ResolvedSource { kind, schema: schema_id, detail, columns })
    }

    /// The table a result can be edited in, or why it can't. `cached`: from
    /// the explorer's cache when it has the table (opening the editor: fast,
    /// offline too); else described now (saving: the table as it is).
    pub async fn edit_target(&self, o: &OutputInfo, schema: &SchemaRef, cached: bool) -> Result<EditTarget> {
        let no = |why: &str| Err(EngineError::new("invalid", why.to_string()));
        let profile = self.workspace().get_connection(&o.connection_id)?;
        if profile.config.read_only {
            return no("The connection is read-only");
        }
        if !profile.config.kind.traits().language.is_sql() {
            return no("Only results of SQL databases can be edited");
        }
        let ResolvedSource { kind, schema: schema_id, detail, columns } = self.resolve_source(o, schema, cached).await?;
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
        let table = quote_object(kind, &t.schema, &t.table);
        let col = |i: usize| -> Result<(String, &databrain_connector_core::arrow::datatypes::Field)> {
            let name =
                t.columns.get(i).cloned().flatten().ok_or_else(|| {
                    bad(format!("Column {} is not a column of {}", schema.fields().get(i).map(|f| f.name().as_str()).unwrap_or("?"), t.table))
                })?;
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
                    let sets =
                        values.iter().map(|v| Ok(format!("{} = {}", col(v.column)?.0, lit(v.column, v.value.as_deref())?))).collect::<Result<Vec<_>>>()?;
                    out.push(EditStatement { sql: format!("UPDATE {table} SET {} WHERE {}", sets.join(", "), key_where(key)?), expect_rows: Some(1) });
                }
                RowChange::Insert { values } => {
                    // Cells not given get the column's default; a cell set
                    // to NULL is inserted as NULL.
                    let vals: Vec<&CellValue> = values.iter().collect();
                    if vals.is_empty() {
                        let Some(first) = t.columns.iter().flatten().next() else {
                            return Err(bad(format!("No column of {} is in the result", t.table)));
                        };
                        out.push(EditStatement { sql: insert_defaults(kind, &table, &quote_ident(kind, first)), expect_rows: Some(1) });
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
        let _claim = self.claim(
            &t.connection_id,
            session_key,
            databrain_connector_core::CancellationToken::new(),
            "A query is running in this tab: wait for it to finish, then apply",
        )?;
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
        // In the user's open transaction: a savepoint, so a conflict undoes
        // this apply only (the user's own changes stay pending).
        let tx = kind.traits().transactions;
        let savepoint = tx.and_then(|t| t.savepoint).filter(|_| in_tx);
        const SAVEPOINT: &str = "databrain_apply";
        if let Some((set, _)) = savepoint {
            run(set.replace("{}", SAVEPOINT)).await?;
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
                    let mut rolled_back = savepoint.is_some();
                    if let Some((_, back)) = savepoint {
                        rolled_back = run(back.replace("{}", SAVEPOINT)).await.is_ok();
                    }
                    let mut e = e;
                    if style.is_none() && !in_tx && i > 0 {
                        e.message.push_str(&format!(" (the {i} change(s) before it were saved: this database has no transactions)"));
                    }
                    // In an open transaction without a savepoint, the changes
                    // before it stay pending: not "nothing was saved".
                    if in_tx && !rolled_back && i > 0 {
                        e.message = e.message.replace("; nothing was saved", "");
                        e.message
                            .push_str(&format!(" (the {i} change(s) before it are pending in your open transaction: Commit keeps them, Rollback undoes them)"));
                    }
                    return Err(e);
                }
            }
        }
        if let Some((_, commit, _)) = style {
            run(commit.to_string()).await?;
        }
        // Kept: the savepoint isn't needed any more (best effort; the
        // changes are already in the user's transaction).
        if savepoint.is_some()
            && let Some(release) = tx.and_then(|t| t.release_savepoint)
        {
            let _ = run(release.replace("{}", SAVEPOINT)).await;
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
        Some(SourceTable {
            schema: schema.map(Into::into),
            table: table.into(),
            items: items.into_iter().map(|i| i.map(|c| c.map(Into::into))).collect(),
            opaque: false,
        })
    }

    #[test]
    fn finds_the_single_source_table() {
        assert_eq!(src("select * from public.orders where id > 3 order by id limit 10", Postgres), t(Some("public"), "orders", vec![None]));
        assert_eq!(
            src("select o.id, o.amount * 2, name as n from orders o", Postgres),
            t(None, "orders", vec![Some(Some("id")), Some(None), Some(Some("name"))])
        );
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

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }
    fn map(sql: &str, k: ConnectorKind, names: &[&str], table: &[&str]) -> Vec<Option<String>> {
        map_columns(&source_table(sql, k).unwrap(), &strs(names), &strs(table))
    }
    fn some(v: &[Option<&str>]) -> Vec<Option<String>> {
        v.iter().map(|c| c.map(Into::into)).collect()
    }

    #[test]
    fn a_computed_column_named_like_a_table_column_stays_read_only() {
        let table = ["id", "name", "price"];
        // The computed `price` after `*` is not the table's price; the real one stays editable.
        assert_eq!(
            map("select *, 0 as price from products", Postgres, &["id", "name", "price", "price"], &table),
            some(&[Some("id"), Some("name"), Some("price"), None])
        );
        assert_eq!(
            map("select 0 as price, * from products", Postgres, &["price", "id", "name", "price"], &table),
            some(&[None, Some("id"), Some("name"), Some("price")])
        );
        assert_eq!(map("select p.*, 0 as price from products p", Postgres, &["id", "name", "price", "price"], &table)[3], None);
        // Explicit items: by position (an alias shows the column it reads).
        assert_eq!(
            map("select id, 0 as price, name as label from products", Postgres, &["id", "price", "label"], &table),
            some(&[Some("id"), None, Some("name")])
        );
        // Still editable: plain `*`, plain columns, `* EXCLUDE` plus a computed column.
        assert_eq!(map("select * from products", Postgres, &table, &table), some(&[Some("id"), Some("name"), Some("price")]));
        assert_eq!(map("select id, price from products", Postgres, &["id", "price"], &table), some(&[Some("id"), Some("price")]));
        assert_eq!(
            map("select * exclude (price), 0 as price from products", Duckdb, &["id", "name", "price"], &table),
            some(&[Some("id"), Some("name"), None])
        );
        // A column shown twice: only its first result column is editable.
        assert_eq!(map("select id, price, price from products", Postgres, &["id", "price", "price"], &table), some(&[Some("id"), Some("price"), None]));
        // Unsure: everything read-only.
        let none = vec![None; 3];
        assert_eq!(map("select * replace (0 as price) from products", Duckdb, &table, &table), none, "* REPLACE");
        assert_eq!(map("select * rename (name as price) from products", Snowflake, &["ID", "price", "PRICE"], &["ID", "NAME", "PRICE"]), none);
        assert_eq!(map("select *, * from products", Postgres, &["id", "name", "price", "id", "name", "price"], &table), vec![None; 6]);
        assert_eq!(map("select columns('id|name'), * from products", Duckdb, &["id", "name", "id", "name", "price"], &table), vec![None; 5]);
        assert_eq!(map("select x.* from products", Postgres, &table, &table), none, "another table's *");
        // Names that don't line up with the table's columns in order (an
        // item expanded to several columns): not trusted.
        assert_eq!(map("select *, 1 from products", Postgres, &["name", "id", "price", "?column?"], &table), vec![None; 4]);
        // A column of the `*` the description leaves out (SQLite's `table_info`
        // skips generated columns): only it is read-only.
        assert_eq!(map("select * from products", Sqlite, &["id", "name", "total", "price"], &table), some(&[Some("id"), Some("name"), None, Some("price")]));
        assert_eq!(
            map("select *, 0 as price from products", Sqlite, &["id", "total", "name", "price", "price"], &table),
            some(&[Some("id"), None, Some("name"), Some("price"), None])
        );
    }

    #[test]
    fn values_keep_trailing_newlines() {
        use databrain_connector_core::arrow::datatypes::{DataType, Field};
        let text = Field::new("k", DataType::Utf8, true);
        assert_eq!(edit_literal(&text, Some("a\n"), Postgres).unwrap(), "'a\n'");
        assert_eq!(edit_literal(&text, Some("a\n\n"), Sqlite).unwrap(), "'a\n\n'");
        assert_eq!(edit_literal(&text, Some("plain"), Postgres).unwrap(), "'plain'");
        let num = Field::new("n", DataType::Int64, true);
        assert_eq!(edit_literal(&num, Some(" 42\n"), Postgres).unwrap(), "42", "numbers are still trimmed");
    }

    #[test]
    fn all_default_rows_use_each_database_s_syntax() {
        let d = |k| insert_defaults(k, "t", "c");
        for k in [Postgres, Sqlite, Mssql, Duckdb] {
            assert_eq!(d(k), "INSERT INTO t DEFAULT VALUES", "{k:?}");
        }
        assert_eq!(d(Mysql), "INSERT INTO t () VALUES ()");
        for k in [Oracle, Snowflake, Bigquery, Databricks, Clickhouse] {
            assert_eq!(d(k), "INSERT INTO t (c) VALUES (DEFAULT)", "{k:?}");
        }
    }
}
