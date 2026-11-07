use serde::Serialize;

/// Byte range in the script.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ts_rs::TS)]
#[ts(export)]
#[ts(rename = "LineageRange")]
pub struct Range {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ts_rs::TS)]
#[ts(export)]
#[ts(rename = "LineageNodeKind")]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    /// A table or view in the database (read, or written by the script).
    Table,
    Cte,
    Subquery,
    /// `UNION` / `INTERSECT` / `EXCEPT`.
    SetOp,
    /// The rows a `SELECT` statement returns.
    Result,
}

#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[ts(export)]
#[ts(rename = "LineageColumn")]
pub struct Column {
    pub name: String,
    /// Where the column is defined (alias or expression).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub span: Option<Range>,
    /// The expression of a computed column (shortened).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub expr: Option<String>,
}

#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[ts(export)]
#[ts(rename = "LineageNode")]
pub struct Node {
    pub id: String,
    pub kind: NodeKind,
    pub name: String,
    pub columns: Vec<Column>,
    /// Table columns came from the catalog (else only the referenced ones are listed).
    pub columns_known: bool,
    /// Written by the script (created, inserted into, updated, merged into,
    /// deleted from).
    pub written: bool,
    /// Created by the script as a temporary table.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    #[ts(as = "Option<bool>", optional)]
    pub temporary: bool,
    /// A database view whose definition was traced: its columns come from
    /// the tables it reads.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    #[ts(as = "Option<bool>", optional)]
    pub view: bool,
    /// Statement that defines it (tables: none).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub statement: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub span: Option<Range>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, ts_rs::TS)]
#[ts(export)]
#[ts(rename = "LineageEdgeKind")]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// The value is copied as is.
    Direct,
    /// Computed from it (`upper(a)`, `a * b`, `case …`, window functions).
    Transform,
    /// Aggregated (`sum`, `count`, …).
    Aggregate,
    /// Decides which rows are kept (`WHERE`, `HAVING`, `QUALIFY`).
    Filter,
    /// Join condition.
    Join,
}

/// `from` column (or the whole node) feeds `to` column (or, for filters and
/// joins, the rows of the `to` node).
#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[ts(export)]
#[ts(rename = "LineageEdge")]
pub struct Edge {
    pub from: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub from_column: Option<String>,
    pub to: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub to_column: Option<String>,
    pub kind: EdgeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub span: Option<Range>,
    /// Filters and joins: the clause it comes from (`LEFT JOIN customers c ON c.id = o.customer_id`,
    /// `WHERE o.status = 'paid'`), shortened.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub detail: Option<String>,
    /// Statement (index) the edge comes from.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub statement: Option<usize>,
}

/// Table-level summary of the chain: rows of `from` (a table, or a result)
/// end up in, or decide the rows of, `to` — through any CTEs and subqueries
/// in between, possibly over several statements.
#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[ts(export)]
#[ts(rename = "LineageFlow")]
pub struct TableFlow {
    pub from: String,
    pub to: String,
    /// Strongest way `from` feeds `to`: `aggregate` > `transform` > `direct`
    /// for values; `filter` / `join` when it only decides which rows.
    pub kind: EdgeKind,
    /// Statements (indexes) the flow goes through, in order.
    pub statements: Vec<usize>,
}

#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[ts(export)]
#[ts(rename = "LineageStatement")]
pub struct StatementInfo {
    pub index: usize,
    /// `select`, `select_into`, `create_table_as`, `create_table`,
    /// `create_view`, `insert`, `update`, `delete`, `merge`, `drop`,
    /// `truncate`, `alter`, `other`, `error`.
    pub kind: String,
    pub range: Range,
    /// The query part to check with the database (`EXPLAIN`), when it is a query.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub query: Option<Range>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[ts(export)]
#[ts(rename = "LineageWarning")]
pub struct Warning {
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub span: Option<Range>,
}

#[derive(Debug, Clone, Default, Serialize, ts_rs::TS)]
#[ts(export)]
#[ts(rename = "LineageGraph")]
pub struct Lineage {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub statements: Vec<StatementInfo>,
    pub warnings: Vec<Warning>,
    /// Some statement could not be parsed: tables are shown without lineage.
    pub partial: bool,
    /// Database tables referenced whose columns are unknown (`schema.name` as
    /// written); tables the script creates are not among them.
    pub unknown_tables: Vec<String>,
    /// The chain table to table (and to the results of `SELECT`s).
    #[ts(as = "Option<Vec<TableFlow>>", optional)]
    pub flows: Vec<TableFlow>,
}

/// A table as the catalog knows it.
#[derive(Debug, Clone, Default)]
pub struct CatalogTable {
    /// Qualified display name (`sales.orders`).
    pub name: String,
    pub columns: Vec<String>,
    /// A view's definition: its `CREATE VIEW … AS …` DDL or the query alone.
    /// A view the SQL reads is expanded into the tables it reads.
    pub view_sql: Option<String>,
}

/// Table columns (explorer cache). `parts` = the name as written
/// (`["orders"]`, `["sales", "orders"]`, `["main", "sales", "orders"]`).
pub trait Catalog {
    fn table(&self, parts: &[String]) -> Option<CatalogTable>;
}

/// No schema information: only explicitly named columns are traced.
pub struct NoCatalog;

impl Catalog for NoCatalog {
    fn table(&self, _: &[String]) -> Option<CatalogTable> {
        None
    }
}

impl<F: Fn(&[String]) -> Option<CatalogTable>> Catalog for F {
    fn table(&self, parts: &[String]) -> Option<CatalogTable> {
        self(parts)
    }
}
