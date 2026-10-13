//! "Show DDL" built from the ALL_* dictionary views, the way DBeaver does it.
//!
//! `DBMS_METADATA.GET_DDL` needs SELECT_CATALOG_ROLE for other users'
//! objects (ORA-31603 otherwise) and is slow (it builds XML and transforms
//! it), so it is only the last resort. The ALL_* views show every object the
//! user can access, with a few small queries filtered by owner and name.

use databrain_connector_core::{quote_ident, quote_literal, ConnectorKind};

/// Rows as text, as returned by the session's catalog queries.
pub type Rows = Vec<Vec<Option<String>>>;

fn qi(s: &str) -> String {
    quote_ident(ConnectorKind::Oracle, s)
}

fn get(r: &[Option<String>], i: usize) -> Option<&str> {
    r.get(i).and_then(|v| v.as_deref())
}

fn full(owner: &str, name: &str) -> String {
    format!("{}.{}", qi(owner), qi(name))
}

// ------------------------------------------------------------------ queries

/// Columns (12c+: identity columns). Bind :o, :n.
pub const COLUMNS_SQL: &str = "select c.column_name, c.data_type, to_char(c.data_length), to_char(c.data_precision), to_char(c.data_scale), \
       to_char(c.char_length), c.char_used, c.nullable, c.data_default, c.virtual_column, c.identity_column, \
       (select i.generation_type from all_tab_identity_cols i where i.owner = c.owner and i.table_name = c.table_name and i.column_name = c.column_name) \
     from all_tab_cols c where c.owner = :o and c.table_name = :n and c.hidden_column = 'NO' order by c.column_id";
/// Columns before 12c (no identity columns).
pub const COLUMNS_SQL_11G: &str = "select c.column_name, c.data_type, to_char(c.data_length), to_char(c.data_precision), to_char(c.data_scale), \
       to_char(c.char_length), c.char_used, c.nullable, c.data_default, c.virtual_column, 'NO', null \
     from all_tab_cols c where c.owner = :o and c.table_name = :n and c.hidden_column = 'NO' order by c.column_id";
/// Table properties.
pub const TABLE_SQL: &str = "select t.temporary, t.duration, t.iot_type, t.partitioned, \
       (select c.comments from all_tab_comments c where c.owner = t.owner and c.table_name = t.table_name) \
     from all_tables t where t.owner = :o and t.table_name = :n";
/// Constraints with their columns and, for foreign keys, the referenced columns.
pub const CONSTRAINTS_SQL: &str = "select k.constraint_name, k.constraint_type, k.search_condition, k.delete_rule, k.status, k.generated, \
       k.index_name, cc.column_name, rc.owner, rc.table_name, rc.column_name, k.deferrable, k.deferred \
     from all_constraints k \
     left join all_cons_columns cc on cc.owner = k.owner and cc.constraint_name = k.constraint_name and cc.table_name = k.table_name \
     left join all_cons_columns rc on rc.owner = k.r_owner and rc.constraint_name = k.r_constraint_name and rc.position = cc.position \
     where k.owner = :o and k.table_name = :n and k.constraint_type in ('P','U','R','C') \
     order by decode(k.constraint_type, 'P', 0, 'U', 1, 'R', 2, 3), k.constraint_name, cc.position";
/// Indexes on the table (any owner), with function-based expressions.
pub const INDEXES_SQL: &str = "select i.owner, i.index_name, i.index_type, i.uniqueness, ic.column_name, ic.descend, ie.column_expression \
     from all_indexes i \
     join all_ind_columns ic on ic.index_owner = i.owner and ic.index_name = i.index_name \
     left join all_ind_expressions ie on ie.index_owner = ic.index_owner and ie.index_name = ic.index_name and ie.column_position = ic.column_position \
     where i.table_owner = :o and i.table_name = :n and i.index_type not in ('LOB', 'IOT - TOP') \
     order by i.index_name, ic.column_position";
pub const COL_COMMENTS_SQL: &str = "select column_name, comments from all_col_comments \
     where owner = :o and table_name = :n and comments is not null";
/// Partitioning: type, subpartitioning type, interval.
pub const PART_SQL: &str = "select partitioning_type, subpartitioning_type, interval, to_char(partition_count) from all_part_tables \
     where owner = :o and table_name = :n";
pub const PART_SQL_10G: &str = "select partitioning_type, subpartitioning_type, null, to_char(partition_count) from all_part_tables \
     where owner = :o and table_name = :n";
/// Partition and subpartition key columns (object_type TABLE).
pub const PART_KEYS_SQL: &str = "select 'P', column_name, column_position from all_part_key_columns where owner = :o and name = :n and object_type = 'TABLE' \
     union all select 'S', column_name, column_position from all_subpart_key_columns where owner = :o and name = :n and object_type = 'TABLE' \
     order by 1, 3";
/// Partitions: name, bound (LONG), position.
pub const PARTITIONS_SQL: &str = "select partition_name, high_value from all_tab_partitions \
     where table_owner = :o and table_name = :n order by partition_position";
pub const VIEW_SQL: &str = "select v.text, (select c.comments from all_tab_comments c where c.owner = v.owner and c.table_name = v.view_name) \
     from all_views v where v.owner = :o and v.view_name = :n";
pub const VIEW_COLUMNS_SQL: &str = "select column_name from all_tab_columns where owner = :o and table_name = :n order by column_id";
pub const MVIEW_SQL: &str = "select query, refresh_method, refresh_mode, build_mode, \
       (select c.comments from all_mview_comments c where c.owner = m.owner and c.mview_name = m.mview_name) \
     from all_mviews m where owner = :o and mview_name = :n";
pub const MVIEW_SQL_NO_COMMENTS: &str = "select query, refresh_method, refresh_mode, build_mode, null from all_mviews where owner = :o and mview_name = :n";
/// Source of code objects. Bind :o, :n, :t (PACKAGE, PACKAGE BODY, FUNCTION, …).
pub const SOURCE_SQL: &str = "select text from all_source where owner = :o and name = :n and type = :t order by line";
pub const SEQUENCE_SQL: &str = "select to_char(min_value), to_char(max_value), to_char(increment_by), cycle_flag, order_flag, \
       to_char(cache_size), to_char(last_number) from all_sequences where sequence_owner = :o and sequence_name = :n";
/// Which kind the object is (the explorer's kind may be a guess).
pub const OBJECT_TYPE_SQL: &str = "select object_type from all_objects where owner = :o and object_name = :n \
     and object_type in ('TABLE','VIEW','MATERIALIZED VIEW','PROCEDURE','FUNCTION','PACKAGE','SEQUENCE','TYPE') \
     order by decode(object_type, 'MATERIALIZED VIEW', 0, 'TABLE', 1, 2)";

// ------------------------------------------------------------------ render

/// Oracle type as written in CREATE TABLE.
fn column_type(r: &[Option<String>]) -> String {
    let t = get(r, 1).unwrap_or("");
    let n = |i| get(r, i).and_then(|v: &str| v.parse::<i64>().ok());
    let (len, prec, scale, chars) = (n(2), n(3), n(4), n(5));
    let char_sem = get(r, 6) == Some("C");
    match t {
        "VARCHAR2" | "CHAR" => match chars.filter(|c| *c > 0).or(len) {
            Some(l) => format!("{t}({l}{})", if char_sem { " CHAR" } else { " BYTE" }),
            None => t.to_string(),
        },
        "NVARCHAR2" | "NCHAR" => chars.filter(|c| *c > 0).map(|l| format!("{t}({l})")).unwrap_or_else(|| t.to_string()),
        "RAW" | "UROWID" => len.map(|l| format!("{t}({l})")).unwrap_or_else(|| t.to_string()),
        "NUMBER" => match (prec, scale) {
            (Some(p), Some(0)) => format!("NUMBER({p})"),
            (Some(p), Some(s)) => format!("NUMBER({p},{s})"),
            (None, Some(0)) => "NUMBER(*,0)".into(),
            _ => "NUMBER".into(),
        },
        "FLOAT" => prec.map(|p| format!("FLOAT({p})")).unwrap_or_else(|| "FLOAT".into()),
        _ => t.to_string(),
    }
}

/// `(“A”, “B”)`
fn col_list<'a>(cols: impl IntoIterator<Item = &'a str>) -> String {
    format!("({})", cols.into_iter().map(qi).collect::<Vec<_>>().join(", "))
}

/// `"COL" IS NOT NULL`, the system check behind NOT NULL columns.
fn is_not_null_check(cond: &str) -> bool {
    let c = cond.trim();
    c.ends_with(" IS NOT NULL") && c.starts_with('"') && c[1..c.len() - " IS NOT NULL".len()].trim_end().ends_with('"') && !c.contains(" AND ")
}

pub struct TableParts {
    pub columns: Rows,
    pub table: Rows,
    pub constraints: Rows,
    pub indexes: Rows,
    pub col_comments: Rows,
    pub part: Rows,
    pub part_keys: Rows,
    pub partitions: Rows,
}

struct Constraint<'a> {
    name: &'a str,
    kind: &'a str,
    cond: Option<&'a str>,
    delete_rule: Option<&'a str>,
    disabled: bool,
    generated: bool,
    index: Option<&'a str>,
    cols: Vec<&'a str>,
    r_owner: Option<&'a str>,
    r_table: Option<&'a str>,
    r_cols: Vec<&'a str>,
    deferrable: bool,
    deferred: bool,
}

fn constraints(rows: &Rows) -> Vec<Constraint<'_>> {
    let mut out: Vec<Constraint> = Vec::new();
    for r in rows {
        let name = get(r, 0).unwrap_or("");
        if out.last().map(|c| c.name) != Some(name) {
            out.push(Constraint {
                name,
                kind: get(r, 1).unwrap_or(""),
                cond: get(r, 2),
                delete_rule: get(r, 3),
                disabled: get(r, 4) == Some("DISABLED"),
                generated: get(r, 5) == Some("GENERATED NAME"),
                index: get(r, 6),
                cols: vec![],
                r_owner: get(r, 8),
                r_table: get(r, 9),
                r_cols: vec![],
                deferrable: get(r, 11) == Some("DEFERRABLE"),
                deferred: get(r, 12) == Some("DEFERRED"),
            });
        }
        let c = out.last_mut().unwrap();
        if let Some(col) = get(r, 7) {
            if !c.cols.contains(&col) {
                c.cols.push(col);
            }
        }
        if let Some(rc) = get(r, 10) {
            c.r_cols.push(rc);
        }
    }
    out
}

fn constraint_line(owner: &str, c: &Constraint) -> Option<String> {
    let body = match c.kind {
        "P" => format!("PRIMARY KEY {}", col_list(c.cols.iter().copied())),
        "U" => format!("UNIQUE {}", col_list(c.cols.iter().copied())),
        "R" => {
            let target = match (c.r_owner, c.r_table) {
                (Some(o), Some(t)) if o == owner => qi(t),
                (Some(o), Some(t)) => full(o, t),
                _ => return None,
            };
            let mut s = format!("FOREIGN KEY {} REFERENCES {target} {}", col_list(c.cols.iter().copied()), col_list(c.r_cols.iter().copied()));
            match c.delete_rule {
                Some("CASCADE") => s.push_str(" ON DELETE CASCADE"),
                Some("SET NULL") => s.push_str(" ON DELETE SET NULL"),
                _ => {}
            }
            s
        }
        "C" => {
            let cond = c.cond?.trim();
            // NOT NULL is written on the column.
            if c.generated && is_not_null_check(cond) {
                return None;
            }
            format!("CHECK ({cond})")
        }
        _ => return None,
    };
    let mut s = if c.generated { format!("    {body}") } else { format!("    CONSTRAINT {} {body}", qi(c.name)) };
    if c.deferrable {
        s.push_str(if c.deferred { " DEFERRABLE INITIALLY DEFERRED" } else { " DEFERRABLE" });
    }
    if c.disabled {
        s.push_str(" DISABLE");
    }
    Some(s)
}

fn partition_clause(p: &TableParts) -> Option<String> {
    let r = p.part.first()?;
    let kind = get(r, 0)?;
    let keys = |k: &str| col_list(p.part_keys.iter().filter(|r| get(r, 0) == Some(k)).filter_map(|r| get(r, 1)));
    let mut s = format!("PARTITION BY {kind} {}", keys("P"));
    if let Some(i) = get(r, 2).map(str::trim).filter(|i| !i.is_empty()) {
        s.push_str(&format!(" INTERVAL ({i})"));
    }
    if let Some(sub) = get(r, 1).filter(|s| *s != "NONE") {
        s.push_str(&format!("\nSUBPARTITION BY {sub} {}", keys("S")));
    }
    if kind == "HASH" && p.partitions.iter().all(|r| get(r, 0).is_some_and(|n| n.starts_with("SYS_P"))) {
        if let Some(n) = get(r, 3) {
            s.push_str(&format!("\nPARTITIONS {n}"));
        }
        return Some(s);
    }
    let parts: Vec<String> = p
        .partitions
        .iter()
        .filter_map(|r| {
            let name = qi(get(r, 0)?);
            let hv = get(r, 1).map(str::trim).unwrap_or("");
            Some(match kind {
                "RANGE" => format!("    PARTITION {name} VALUES LESS THAN ({hv})"),
                "LIST" => format!("    PARTITION {name} VALUES ({hv})"),
                _ => format!("    PARTITION {name}"),
            })
        })
        .collect();
    if !parts.is_empty() {
        s.push_str(&format!("\n(\n{}\n)", parts.join(",\n")));
    }
    Some(s)
}

/// CREATE TABLE with constraints, partitioning, indexes and comments.
/// `None` when the table isn't visible (no columns).
pub fn table_ddl(owner: &str, name: &str, p: &TableParts) -> Option<String> {
    if p.columns.is_empty() {
        return None;
    }
    let t = p.table.first();
    let temporary = t.and_then(|r| get(r, 0)) == Some("Y");
    let iot = t.and_then(|r| get(r, 2)) == Some("IOT");
    let mut lines: Vec<String> = p
        .columns
        .iter()
        .map(|r| {
            let mut l = format!("    {} {}", qi(get(r, 0).unwrap_or("")), column_type(r));
            let default = get(r, 8).map(str::trim).filter(|d| !d.is_empty());
            if get(r, 9) == Some("YES") {
                l.push_str(&format!(" GENERATED ALWAYS AS ({}) VIRTUAL", default.unwrap_or("")));
                return l;
            }
            if get(r, 10) == Some("YES") {
                l.push_str(match get(r, 11) {
                    Some("ALWAYS") => " GENERATED ALWAYS AS IDENTITY",
                    Some("BY DEFAULT ON NULL") => " GENERATED BY DEFAULT ON NULL AS IDENTITY",
                    _ => " GENERATED BY DEFAULT AS IDENTITY",
                });
            } else if let Some(d) = default {
                l.push_str(&format!(" DEFAULT {d}"));
            }
            if get(r, 7) == Some("N") {
                l.push_str(" NOT NULL");
            }
            l
        })
        .collect();
    let cons = constraints(&p.constraints);
    lines.extend(cons.iter().filter_map(|c| constraint_line(owner, c)));
    let table = full(owner, name);
    let mut ddl = format!("CREATE {}TABLE {table} (\n{}\n)", if temporary { "GLOBAL TEMPORARY " } else { "" }, lines.join(",\n"));
    if temporary {
        ddl.push_str(if t.and_then(|r| get(r, 1)) == Some("SYS$SESSION") { "\nON COMMIT PRESERVE ROWS" } else { "\nON COMMIT DELETE ROWS" });
    }
    if iot {
        ddl.push_str("\nORGANIZATION INDEX");
    }
    if let Some(pc) = partition_clause(p) {
        ddl.push('\n');
        ddl.push_str(&pc);
    }
    ddl.push(';');

    // Indexes, except those created by a primary key or unique constraint.
    let backing: Vec<&str> = cons.iter().filter(|c| matches!(c.kind, "P" | "U")).filter_map(|c| c.index).collect();
    let idx = indexes_ddl(&table, &p.indexes, &backing);
    if !idx.is_empty() {
        ddl.push_str("\n\n");
        ddl.push_str(&idx.join("\n"));
    }

    let mut comments = Vec::new();
    if let Some(c) = t.and_then(|r| get(r, 4)).filter(|c| !c.is_empty()) {
        comments.push(format!("COMMENT ON TABLE {table} IS {};", quote_literal(c)));
    }
    for r in &p.col_comments {
        if let (Some(col), Some(c)) = (get(r, 0), get(r, 1)) {
            comments.push(format!("COMMENT ON COLUMN {table}.{} IS {};", qi(col), quote_literal(c)));
        }
    }
    if !comments.is_empty() {
        ddl.push_str("\n\n");
        ddl.push_str(&comments.join("\n"));
    }
    Some(ddl)
}

/// CREATE INDEX statements (rows of [`INDEXES_SQL`]), skipping `skip` names.
pub fn indexes_ddl(table: &str, rows: &Rows, skip: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        let (owner, name) = (get(&rows[i], 0).unwrap_or(""), get(&rows[i], 1).unwrap_or(""));
        let mut j = i;
        let mut cols = Vec::new();
        while j < rows.len() && get(&rows[j], 1) == Some(name) && get(&rows[j], 0) == Some(owner) {
            let r = &rows[j];
            // Function-based (and DESC) columns: the expression.
            let mut c = match get(r, 6).map(str::trim).filter(|e| !e.is_empty()) {
                Some(e) => e.to_string(),
                None => qi(get(r, 4).unwrap_or("")),
            };
            if get(r, 5) == Some("DESC") {
                c.push_str(" DESC");
            }
            cols.push(c);
            j += 1;
        }
        let r = &rows[i];
        i = j;
        if skip.contains(&name) {
            continue;
        }
        let ty = get(r, 2).unwrap_or("");
        let kind = if ty.contains("BITMAP") {
            "BITMAP "
        } else if get(r, 3) == Some("UNIQUE") {
            "UNIQUE "
        } else {
            ""
        };
        out.push(format!("CREATE {kind}INDEX {} ON {table} ({});", full(owner, name), cols.join(", ")));
    }
    out
}

pub fn view_ddl(owner: &str, name: &str, view: &Rows, columns: &Rows) -> Option<String> {
    let r = view.first()?;
    let text = get(r, 0)?.trim_end().trim_end_matches(';');
    let cols: Vec<&str> = columns.iter().filter_map(|r| get(r, 0)).collect();
    let target = full(owner, name);
    let list = if cols.is_empty() { String::new() } else { format!(" {}", col_list(cols)) };
    let mut ddl = format!("CREATE OR REPLACE VIEW {target}{list} AS\n{};", text.trim_start());
    if let Some(c) = get(r, 1).filter(|c| !c.is_empty()) {
        ddl.push_str(&format!("\n\nCOMMENT ON TABLE {target} IS {};", quote_literal(c)));
    }
    Some(ddl)
}

pub fn mview_ddl(owner: &str, name: &str, mv: &Rows, indexes: &Rows) -> Option<String> {
    let r = mv.first()?;
    let query = get(r, 0)?.trim_end().trim_end_matches(';');
    let target = full(owner, name);
    let mut ddl = format!("CREATE MATERIALIZED VIEW {target}");
    if let Some(b) = get(r, 3).filter(|b| matches!(*b, "IMMEDIATE" | "DEFERRED")) {
        ddl.push_str(&format!("\nBUILD {b}"));
    } else if get(r, 3) == Some("PREBUILT") {
        ddl.push_str("\nON PREBUILT TABLE");
    }
    match (get(r, 1), get(r, 2)) {
        (Some("NEVER"), _) | (_, Some("NEVER")) => ddl.push_str("\nNEVER REFRESH"),
        (Some(m), Some(on)) => ddl.push_str(&format!("\nREFRESH {m} ON {on}")),
        _ => {}
    }
    ddl.push_str(&format!("\nAS\n{};", query.trim_start()));
    let idx = indexes_ddl(&target, indexes, &[]);
    if !idx.is_empty() {
        ddl.push_str("\n\n");
        ddl.push_str(&idx.join("\n"));
    }
    if let Some(c) = get(r, 4).filter(|c| !c.is_empty()) {
        ddl.push_str(&format!("\n\nCOMMENT ON MATERIALIZED VIEW {target} IS {};", quote_literal(c)));
    }
    Some(ddl)
}

/// `CREATE OR REPLACE` + ALL_SOURCE lines, ended with `/` (SQL*Plus style).
pub fn source_ddl(lines: &Rows) -> Option<String> {
    let text: String = lines.iter().filter_map(|r| get(r, 0)).collect();
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some(format!("CREATE OR REPLACE {text}\n/"))
}

pub fn sequence_ddl(owner: &str, name: &str, rows: &Rows) -> Option<String> {
    let r = rows.first()?;
    let v = |i| get(r, i).unwrap_or("");
    let mut s = format!(
        "CREATE SEQUENCE {}\n    START WITH {}\n    INCREMENT BY {}\n    MINVALUE {}\n    MAXVALUE {}",
        full(owner, name),
        v(6),
        v(2),
        v(0),
        v(1)
    );
    match v(5) {
        "0" | "" => s.push_str("\n    NOCACHE"),
        n => s.push_str(&format!("\n    CACHE {n}")),
    }
    s.push_str(if v(3) == "Y" { "\n    CYCLE" } else { "\n    NOCYCLE" });
    s.push_str(if v(4) == "Y" { "\n    ORDER" } else { "\n    NOORDER" });
    s.push(';');
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(v: &[Option<&str>]) -> Vec<Option<String>> {
        v.iter().map(|x| x.map(str::to_string)).collect()
    }

    #[test]
    fn renders_a_table_with_constraints_indexes_and_comments() {
        let n = None;
        let p = TableParts {
            columns: vec![
                row(&[Some("ID"), Some("NUMBER"), Some("22"), Some("10"), Some("0"), Some("0"), n, Some("N"), n, Some("NO"), Some("YES"), Some("ALWAYS")]),
                row(&[Some("NAME"), Some("VARCHAR2"), Some("160"), n, n, Some("40"), Some("C"), Some("Y"), Some("'x' "), Some("NO"), Some("NO"), n]),
                row(&[Some("AMOUNT"), Some("NUMBER"), Some("22"), Some("12"), Some("2"), Some("0"), n, Some("N"), n, Some("NO"), Some("NO"), n]),
                row(&[Some("CUST_ID"), Some("NUMBER"), Some("22"), n, n, Some("0"), n, Some("Y"), n, Some("NO"), Some("NO"), n]),
            ],
            table: vec![row(&[Some("N"), n, n, Some("NO"), Some("Orders")])],
            constraints: vec![
                row(&[Some("PK_O"), Some("P"), n, n, Some("ENABLED"), Some("USER NAME"), Some("PK_O"), Some("ID"), n, n, n, Some("NOT DEFERRABLE"), Some("IMMEDIATE")]),
                row(&[Some("FK_C"), Some("R"), n, Some("CASCADE"), Some("ENABLED"), Some("USER NAME"), n, Some("CUST_ID"), Some("APP"), Some("CUSTOMERS"), Some("ID"), n, n]),
                row(&[Some("SYS_C001"), Some("C"), Some("\"AMOUNT\" IS NOT NULL"), n, Some("ENABLED"), Some("GENERATED NAME"), n, Some("AMOUNT"), n, n, n, n, n]),
                row(&[Some("CK_AMT"), Some("C"), Some("amount >= 0"), n, Some("ENABLED"), Some("USER NAME"), n, Some("AMOUNT"), n, n, n, n, n]),
            ],
            indexes: vec![
                row(&[Some("APP"), Some("IX_NAME"), Some("FUNCTION-BASED NORMAL"), Some("NONUNIQUE"), Some("SYS_NC00005$"), Some("ASC"), Some("UPPER(\"NAME\")")]),
                row(&[Some("APP"), Some("IX_NAME"), Some("FUNCTION-BASED NORMAL"), Some("NONUNIQUE"), Some("AMOUNT"), Some("ASC"), n]),
                row(&[Some("APP"), Some("PK_O"), Some("NORMAL"), Some("UNIQUE"), Some("ID"), Some("ASC"), n]),
                row(&[Some("APP"), Some("UX_CUST"), Some("NORMAL"), Some("UNIQUE"), Some("CUST_ID"), Some("ASC"), n]),
            ],
            col_comments: vec![row(&[Some("NAME"), Some("it's the name")])],
            part: vec![row(&[Some("RANGE"), Some("NONE"), n, Some("2")])],
            part_keys: vec![row(&[Some("P"), Some("ID"), Some("1")])],
            partitions: vec![row(&[Some("P1"), Some("100")]), row(&[Some("PMAX"), Some("MAXVALUE")])],
        };
        let d = table_ddl("APP", "ORDERS", &p).unwrap();
        let want = r#"CREATE TABLE "APP"."ORDERS" (
    "ID" NUMBER(10) GENERATED ALWAYS AS IDENTITY NOT NULL,
    "NAME" VARCHAR2(40 CHAR) DEFAULT 'x',
    "AMOUNT" NUMBER(12,2) NOT NULL,
    "CUST_ID" NUMBER,
    CONSTRAINT "PK_O" PRIMARY KEY ("ID"),
    CONSTRAINT "FK_C" FOREIGN KEY ("CUST_ID") REFERENCES "CUSTOMERS" ("ID") ON DELETE CASCADE,
    CONSTRAINT "CK_AMT" CHECK (amount >= 0)
)
PARTITION BY RANGE ("ID")
(
    PARTITION "P1" VALUES LESS THAN (100),
    PARTITION "PMAX" VALUES LESS THAN (MAXVALUE)
);

CREATE INDEX "APP"."IX_NAME" ON "APP"."ORDERS" (UPPER("NAME"), "AMOUNT");
CREATE UNIQUE INDEX "APP"."UX_CUST" ON "APP"."ORDERS" ("CUST_ID");

COMMENT ON TABLE "APP"."ORDERS" IS 'Orders';
COMMENT ON COLUMN "APP"."ORDERS"."NAME" IS 'it''s the name';"#;
        assert_eq!(d, want);
    }

    #[test]
    fn renders_views_code_and_sequences() {
        let v = view_ddl("APP", "V", &vec![row(&[Some("select id from t\n"), None])], &vec![row(&[Some("ID")])]).unwrap();
        assert_eq!(v, "CREATE OR REPLACE VIEW \"APP\".\"V\" (\"ID\") AS\nselect id from t;");
        let src = source_ddl(&vec![row(&[Some("procedure p is\n")]), row(&[Some("begin null; end;\n")])]).unwrap();
        assert_eq!(src, "CREATE OR REPLACE procedure p is\nbegin null; end;\n/");
        assert!(source_ddl(&vec![]).is_none());
        let s = sequence_ddl("APP", "S", &vec![row(&[Some("1"), Some("999"), Some("1"), Some("N"), Some("N"), Some("20"), Some("41")])]).unwrap();
        assert!(s.starts_with("CREATE SEQUENCE \"APP\".\"S\"\n    START WITH 41") && s.contains("CACHE 20") && s.ends_with("NOORDER;"));
        let m = mview_ddl("APP", "M", &vec![row(&[Some("select 1 x from dual"), Some("COMPLETE"), Some("DEMAND"), Some("IMMEDIATE"), None])], &vec![]).unwrap();
        assert_eq!(m, "CREATE MATERIALIZED VIEW \"APP\".\"M\"\nBUILD IMMEDIATE\nREFRESH COMPLETE ON DEMAND\nAS\nselect 1 x from dual;");
        assert!(table_ddl("APP", "NONE", &TableParts { columns: vec![], table: vec![], constraints: vec![], indexes: vec![], col_comments: vec![], part: vec![], part_keys: vec![], partitions: vec![] }).is_none());
    }

    #[test]
    fn not_null_checks() {
        assert!(is_not_null_check("\"A\" IS NOT NULL"));
        assert!(!is_not_null_check("\"A\" IS NOT NULL AND \"B\" IS NOT NULL"));
        assert!(!is_not_null_check("a > 0"));
    }
}
