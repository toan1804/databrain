//! "Show DDL" built from the ALL_* dictionary views, the way DBeaver does it.
//!
//! `DBMS_METADATA.GET_DDL` needs SELECT_CATALOG_ROLE for other users'
//! objects (ORA-31603 otherwise) and is slow (it builds XML and transforms
//! it), so it is only the last resort. The ALL_* views show every object the
//! user can access, with a few small queries filtered by owner and name.

use databrain_connector_core::{ConnectorKind, quote_ident, quote_literal};

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
/// Table properties (its comment comes with [`COMMENTS_SQL`]). Partitioning
/// isn't part of the DDL: the table's Partitions part shows it, and reading
/// it took up to three more statements.
pub const TABLE_SQL: &str = "select t.temporary, t.duration, t.iot_type from all_tables t where t.owner = :o and t.table_name = :n";
/// Constraints with their columns. A foreign key's referenced columns are
/// [`REF_KEYS_SQL`], a query of its own: joining ALL_CONS_COLUMNS a second
/// time here (paired by position) takes Oracle about ten times longer to
/// plan (0.45 s against 0.04 s on 23ai), and it plans that join badly.
pub const CONSTRAINTS_SQL: &str = "select k.constraint_name, k.constraint_type, k.search_condition_vc, k.delete_rule, k.status, k.generated, \
       k.index_name, cc.column_name, k.deferrable, k.deferred \
     from all_constraints k \
     left join all_cons_columns cc on cc.owner = k.owner and cc.constraint_name = k.constraint_name and cc.table_name = k.table_name \
     where k.owner = :o and k.table_name = :n and k.constraint_type in ('P','U','R','C') \
     order by decode(k.constraint_type, 'P', 0, 'U', 1, 'R', 2, 3), k.constraint_name, cc.position";
/// Before 12.2: the condition only as a LONG (read a row at a time).
pub const CONSTRAINTS_SQL_12_1: &str = "select k.constraint_name, k.constraint_type, k.search_condition, k.delete_rule, k.status, k.generated, \
       k.index_name, cc.column_name, k.deferrable, k.deferred \
     from all_constraints k \
     left join all_cons_columns cc on cc.owner = k.owner and cc.constraint_name = k.constraint_name and cc.table_name = k.table_name \
     where k.owner = :o and k.table_name = :n and k.constraint_type in ('P','U','R','C') \
     order by decode(k.constraint_type, 'P', 0, 'U', 1, 'R', 2, 3), k.constraint_name, cc.position";
/// The columns each foreign key of the table references, in key order
/// (asked only when the table has a foreign key).
pub const REF_KEYS_SQL: &str = "select k.constraint_name, rc.owner, rc.table_name, rc.column_name \
     from all_constraints k \
     join all_cons_columns rc on rc.owner = k.r_owner and rc.constraint_name = k.r_constraint_name \
     where k.owner = :o and k.table_name = :n and k.constraint_type = 'R' \
     order by k.constraint_name, rc.position";
/// Indexes on the table (any owner). The expressions of function-based
/// (and DESC) index columns are [`INDEX_EXPRESSIONS_SQL`].
pub const INDEXES_SQL: &str = "select i.owner, i.index_name, i.index_type, i.uniqueness, ic.column_name, ic.descend, to_char(ic.column_position) \
     from all_indexes i \
     join all_ind_columns ic on ic.index_owner = i.owner and ic.index_name = i.index_name \
     where i.table_owner = :o and i.table_name = :n and i.index_type not in ('LOB', 'IOT - TOP') \
     order by i.index_name, ic.column_position";
/// Expressions of the table's function-based index columns (asked only
/// when it has such an index: each is a LONG, read a row at a time, and
/// joined to [`INDEXES_SQL`] it made every index column a row of its own
/// round trip and the statement five times slower to plan).
pub const INDEX_EXPRESSIONS_SQL: &str = "select index_owner, index_name, to_char(column_position), column_expression from all_ind_expressions \
     where table_owner = :o and table_name = :n";
/// Column comments, and the table's own (no column name).
pub const COMMENTS_SQL: &str = "select column_name, comments from all_col_comments \
     where owner = :o and table_name = :n and comments is not null \
     union all select null, comments from all_tab_comments where owner = :o and table_name = :n and comments is not null";
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
    /// Referenced columns of the foreign keys (see [`REF_KEYS_SQL`]).
    pub ref_keys: Rows,
    pub indexes: Rows,
    /// See [`INDEX_EXPRESSIONS_SQL`].
    pub index_expressions: Rows,
    /// Column comments and the table's (see [`COMMENTS_SQL`]).
    pub comments: Rows,
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

/// Constraints from the rows of [`CONSTRAINTS_SQL`] (one per column, a
/// constraint's rows together), foreign keys with what `ref_keys` says they
/// reference.
fn constraints<'a>(rows: &'a Rows, ref_keys: &'a Rows) -> Vec<Constraint<'a>> {
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
                r_owner: None,
                r_table: None,
                r_cols: vec![],
                deferrable: get(r, 8) == Some("DEFERRABLE"),
                deferred: get(r, 9) == Some("DEFERRED"),
            });
        }
        let c = out.last_mut().unwrap();
        if let Some(col) = get(r, 7)
            && !c.cols.contains(&col)
        {
            c.cols.push(col);
        }
    }
    for c in out.iter_mut().filter(|c| c.kind == "R") {
        for k in ref_keys.iter().filter(|k| get(k, 0) == Some(c.name)) {
            c.r_owner = get(k, 1);
            c.r_table = get(k, 2);
            if let Some(col) = get(k, 3) {
                c.r_cols.push(col);
            }
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

/// CREATE TABLE with constraints, indexes and comments (no partitioning).
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
    let cons = constraints(&p.constraints, &p.ref_keys);
    lines.extend(cons.iter().filter_map(|c| constraint_line(owner, c)));
    let table = full(owner, name);
    let mut ddl = format!("CREATE {}TABLE {table} (\n{}\n)", if temporary { "GLOBAL TEMPORARY " } else { "" }, lines.join(",\n"));
    if temporary {
        ddl.push_str(if t.and_then(|r| get(r, 1)) == Some("SYS$SESSION") { "\nON COMMIT PRESERVE ROWS" } else { "\nON COMMIT DELETE ROWS" });
    }
    if iot {
        ddl.push_str("\nORGANIZATION INDEX");
    }
    ddl.push(';');

    // Indexes, except those created by a primary key or unique constraint.
    let backing: Vec<&str> = cons.iter().filter(|c| matches!(c.kind, "P" | "U")).filter_map(|c| c.index).collect();
    let idx = indexes_ddl(&table, &p.indexes, &p.index_expressions, &backing);
    if !idx.is_empty() {
        ddl.push_str("\n\n");
        ddl.push_str(&idx.join("\n"));
    }

    let mut comments = Vec::new();
    if let Some(c) = p.comments.iter().find(|r| get(r, 0).is_none()).and_then(|r| get(r, 1)).filter(|c| !c.is_empty()) {
        comments.push(format!("COMMENT ON TABLE {table} IS {};", quote_literal(c)));
    }
    for r in &p.comments {
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

/// Whether [`INDEX_EXPRESSIONS_SQL`] has something to say about these rows
/// of [`INDEXES_SQL`]: a function-based index (a DESC column makes one too).
pub fn has_function_based(indexes: &Rows) -> bool {
    indexes.iter().any(|r| get(r, 2).is_some_and(|t| t.contains("FUNCTION-BASED")))
}

/// CREATE INDEX statements (rows of [`INDEXES_SQL`], expressions from
/// [`INDEX_EXPRESSIONS_SQL`]), skipping `skip` names.
pub fn indexes_ddl(table: &str, rows: &Rows, expressions: &Rows, skip: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        let (owner, name) = (get(&rows[i], 0).unwrap_or(""), get(&rows[i], 1).unwrap_or(""));
        let mut j = i;
        let mut cols = Vec::new();
        // A hidden expression column whose expression wasn't read.
        let mut unread = false;
        while j < rows.len() && get(&rows[j], 1) == Some(name) && get(&rows[j], 0) == Some(owner) {
            let r = &rows[j];
            // Function-based (and DESC) columns: the expression.
            let expr = expressions.iter().find(|e| get(e, 0) == Some(owner) && get(e, 1) == Some(name) && get(e, 2).is_some() && get(e, 2) == get(r, 6));
            let mut c = match expr.and_then(|e| get(e, 3)).map(str::trim).filter(|e| !e.is_empty()) {
                Some(e) => e.to_string(),
                None => {
                    unread |= get(r, 4).is_some_and(|c| c.starts_with("SYS_NC"));
                    qi(get(r, 4).unwrap_or(""))
                }
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
        // Its hidden column's name would make a statement that doesn't run.
        if unread {
            out.push(format!("-- index {}: its expression could not be read", full(owner, name)));
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

pub fn mview_ddl(owner: &str, name: &str, mv: &Rows, indexes: &Rows, index_expressions: &Rows) -> Option<String> {
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
    let idx = indexes_ddl(&target, indexes, index_expressions, &[]);
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
    let mut s =
        format!("CREATE SEQUENCE {}\n    START WITH {}\n    INCREMENT BY {}\n    MINVALUE {}\n    MAXVALUE {}", full(owner, name), v(6), v(2), v(0), v(1));
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
            table: vec![row(&[Some("N"), n, n])],
            constraints: vec![
                row(&[Some("PK_O"), Some("P"), n, n, Some("ENABLED"), Some("USER NAME"), Some("PK_O"), Some("ID"), Some("NOT DEFERRABLE"), Some("IMMEDIATE")]),
                row(&[Some("FK_C"), Some("R"), n, Some("CASCADE"), Some("ENABLED"), Some("USER NAME"), n, Some("CUST_ID"), n, n]),
                row(&[Some("FK_P"), Some("R"), n, Some("NO ACTION"), Some("ENABLED"), Some("USER NAME"), n, Some("ID"), Some("DEFERRABLE"), Some("DEFERRED")]),
                row(&[
                    Some("FK_P"),
                    Some("R"),
                    n,
                    Some("NO ACTION"),
                    Some("ENABLED"),
                    Some("USER NAME"),
                    n,
                    Some("CUST_ID"),
                    Some("DEFERRABLE"),
                    Some("DEFERRED"),
                ]),
                // A key this user can't see (no rows in `ref_keys`) isn't written.
                row(&[Some("FK_HIDDEN"), Some("R"), n, n, Some("ENABLED"), Some("USER NAME"), n, Some("CUST_ID"), n, n]),
                row(&[Some("SYS_C001"), Some("C"), Some("\"AMOUNT\" IS NOT NULL"), n, Some("ENABLED"), Some("GENERATED NAME"), n, Some("AMOUNT"), n, n]),
                row(&[Some("CK_AMT"), Some("C"), Some("amount >= 0"), n, Some("ENABLED"), Some("USER NAME"), n, Some("AMOUNT"), n, n]),
            ],
            // In key order per constraint, whatever the order of the constraints.
            ref_keys: vec![
                row(&[Some("FK_P"), Some("OTHER"), Some("PAIRS"), Some("A")]),
                row(&[Some("FK_P"), Some("OTHER"), Some("PAIRS"), Some("B")]),
                row(&[Some("FK_C"), Some("APP"), Some("CUSTOMERS"), Some("ID")]),
            ],
            indexes: vec![
                row(&[Some("APP"), Some("IX_NAME"), Some("FUNCTION-BASED NORMAL"), Some("NONUNIQUE"), Some("SYS_NC00005$"), Some("ASC"), Some("1")]),
                row(&[Some("APP"), Some("IX_NAME"), Some("FUNCTION-BASED NORMAL"), Some("NONUNIQUE"), Some("AMOUNT"), Some("ASC"), Some("2")]),
                row(&[Some("APP"), Some("PK_O"), Some("NORMAL"), Some("UNIQUE"), Some("ID"), Some("ASC"), Some("1")]),
                row(&[Some("APP"), Some("UX_CUST"), Some("NORMAL"), Some("UNIQUE"), Some("CUST_ID"), Some("ASC"), Some("1")]),
            ],
            // Another owner's index of the same name isn't this one's.
            index_expressions: vec![
                row(&[Some("ELSE"), Some("IX_NAME"), Some("2"), Some("LOWER(\"X\")")]),
                row(&[Some("APP"), Some("IX_NAME"), Some("1"), Some("UPPER(\"NAME\")")]),
            ],
            comments: vec![row(&[Some("NAME"), Some("it's the name")]), row(&[n, Some("Orders")])],
        };
        let d = table_ddl("APP", "ORDERS", &p).unwrap();
        let want = r#"CREATE TABLE "APP"."ORDERS" (
    "ID" NUMBER(10) GENERATED ALWAYS AS IDENTITY NOT NULL,
    "NAME" VARCHAR2(40 CHAR) DEFAULT 'x',
    "AMOUNT" NUMBER(12,2) NOT NULL,
    "CUST_ID" NUMBER,
    CONSTRAINT "PK_O" PRIMARY KEY ("ID"),
    CONSTRAINT "FK_C" FOREIGN KEY ("CUST_ID") REFERENCES "CUSTOMERS" ("ID") ON DELETE CASCADE,
    CONSTRAINT "FK_P" FOREIGN KEY ("ID", "CUST_ID") REFERENCES "OTHER"."PAIRS" ("A", "B") DEFERRABLE INITIALLY DEFERRED,
    CONSTRAINT "CK_AMT" CHECK (amount >= 0)
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
        let m = mview_ddl("APP", "M", &vec![row(&[Some("select 1 x from dual"), Some("COMPLETE"), Some("DEMAND"), Some("IMMEDIATE"), None])], &vec![], &vec![])
            .unwrap();
        assert_eq!(m, "CREATE MATERIALIZED VIEW \"APP\".\"M\"\nBUILD IMMEDIATE\nREFRESH COMPLETE ON DEMAND\nAS\nselect 1 x from dual;");
        assert!(
            table_ddl(
                "APP",
                "NONE",
                &TableParts {
                    columns: vec![],
                    table: vec![],
                    constraints: vec![],
                    ref_keys: vec![],
                    indexes: vec![],
                    index_expressions: vec![],
                    comments: vec![],
                }
            )
            .is_none()
        );
    }

    /// Expressions are asked for only when an index is function-based.
    #[test]
    fn expressions_only_for_function_based_indexes() {
        let n = None;
        let plain = vec![row(&[Some("APP"), Some("IX"), Some("NORMAL"), Some("UNIQUE"), Some("ID"), Some("ASC"), Some("1")])];
        assert!(!has_function_based(&plain));
        let desc = vec![row(&[Some("APP"), Some("IX"), Some("FUNCTION-BASED NORMAL"), Some("NONUNIQUE"), Some("SYS_NC00003$"), Some("DESC"), Some("1")])];
        assert!(has_function_based(&desc));
        // Without its expression the index is named, not written with its hidden column; with it, the expression.
        assert_eq!(indexes_ddl("T", &desc, &vec![], &[]), ["-- index \"APP\".\"IX\": its expression could not be read"]);
        let e = vec![row(&[Some("APP"), Some("IX"), Some("1"), Some("\"ID\"")]), row(&[Some("APP"), Some("IX"), n, Some("x")])];
        assert_eq!(indexes_ddl("T", &desc, &e, &[]), ["CREATE INDEX \"APP\".\"IX\" ON T (\"ID\" DESC);"]);
    }

    /// The statements never join ALL_CONS_COLUMNS twice (see [`CONSTRAINTS_SQL`]).
    #[test]
    fn one_cons_columns_join_per_statement() {
        for sql in [CONSTRAINTS_SQL, CONSTRAINTS_SQL_12_1, REF_KEYS_SQL] {
            assert_eq!(sql.matches("all_cons_columns").count(), 1, "{sql}");
        }
        assert!(!INDEXES_SQL.contains("all_ind_expressions"));
    }

    #[test]
    fn not_null_checks() {
        assert!(is_not_null_check("\"A\" IS NOT NULL"));
        assert!(!is_not_null_check("\"A\" IS NOT NULL AND \"B\" IS NOT NULL"));
        assert!(!is_not_null_check("a > 0"));
    }
}
