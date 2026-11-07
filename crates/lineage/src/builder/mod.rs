use std::collections::HashMap;

use databrain_connector_core::ConnectorKind;
use databrain_connector_core::sql::{dialect_for, split_statements};
use sqlparser::ast::{Ident, ObjectName, Spanned, TableAlias};
use sqlparser::parser::Parser;
use sqlparser::tokenizer::{Location, Span, Token, Tokenizer};

mod from;
mod joins;
mod query;
mod resolve;
mod statement;

use crate::*;

// ------------------------------------------------------------------ entry

/// Lineage of every statement of `sql`.
pub fn analyze(sql: &str, kind: ConnectorKind, catalog: &dyn Catalog) -> Lineage {
    build(sql, kind, catalog).0
}

/// Database tables joined on equal columns in `sql` (see [`JoinPair`]), for
/// learning how tables join from query history. Statements that don't parse
/// contribute nothing.
pub fn join_pairs(sql: &str, kind: ConnectorKind, catalog: &dyn Catalog) -> Vec<JoinPair> {
    build(sql, kind, catalog).1
}

fn build(sql: &str, kind: ConnectorKind, catalog: &dyn Catalog) -> (Lineage, Vec<JoinPair>) {
    let mut b = Builder {
        g: Lineage::default(),
        cat: catalog,
        kind,
        tables: HashMap::new(),
        created: Default::default(),
        expanded: Default::default(),
        in_view: 0,
        text: "",
        base: 0,
        lines: vec![],
        stmt: 0,
        joins: vec![],
        clause: 0,
    };
    let spans = split_statements(sql, kind);
    let many = spans.len() > 1;
    for (i, s) in spans.iter().enumerate() {
        b.text = &sql[s.start..s.end];
        b.base = s.start;
        b.lines = line_starts(b.text);
        b.stmt = i;
        let range = Range { start: s.start, end: s.end };
        match Parser::parse_sql(&*dialect_for(kind), b.text) {
            Ok(stmts) => {
                for st in &stmts {
                    b.statement(st, range, many);
                }
            }
            Err(e) => {
                b.g.partial = true;
                b.g.statements.push(StatementInfo { index: i, kind: "error".into(), range, query: None, result: None, error: Some(e.to_string()) });
                b.tables_by_tokens();
            }
        }
    }
    // Tables the script defines have known columns; one it only inserts
    // into, updates or deletes from is still a database table to describe.
    let mut unknown: Vec<String> =
        b.g.nodes
            .iter()
            .enumerate()
            .filter(|(i, n)| n.kind == NodeKind::Table && !n.columns_known && !b.created.contains(i))
            .map(|(_, n)| n.name.clone())
            .collect();
    unknown.sort();
    unknown.dedup();
    b.g.unknown_tables = unknown;
    b.g.flows = flows(&b.g);
    let joins = b.join_pairs();
    (b.g, joins)
}

// ------------------------------------------------------------------ builder

struct Builder<'a> {
    g: Lineage,
    cat: &'a dyn Catalog,
    kind: ConnectorKind,
    /// Table key (lower-case qualified name) → node index; shared by statements.
    tables: HashMap<String, usize>,
    /// Table nodes the script creates (not database tables to describe).
    created: std::collections::HashSet<usize>,
    /// View nodes whose definition was traced (or is being traced).
    expanded: std::collections::HashSet<usize>,
    /// Depth of view definitions being traced: their SQL is not the
    /// editor's, so nothing in them gets a span.
    in_view: usize,
    text: &'a str,
    base: usize,
    lines: Vec<usize>,
    stmt: usize,
    /// Equalities between sources, for [`join_pairs`].
    joins: Vec<joins::RawJoin>,
    /// Counter of conditions seen (groups composite keys).
    clause: usize,
}

impl<'a> Builder<'a> {
    // ---- spans

    fn offset(&self, l: Location) -> Option<usize> {
        if l.line == 0 {
            return None;
        }
        let start = *self.lines.get(l.line as usize - 1)?;
        let line = &self.text[start..];
        let col = (l.column as usize).saturating_sub(1);
        let off = line.char_indices().nth(col).map(|(i, _)| i).unwrap_or(line.len());
        Some(self.base + start + off)
    }

    fn range(&self, s: Span) -> Option<Range> {
        if self.in_view > 0 {
            return None;
        }
        let (a, b) = (self.offset(s.start)?, self.offset(s.end)?);
        (b >= a).then_some(Range { start: a, end: b })
    }

    fn warn(&mut self, message: String, span: Option<Range>) {
        if !self.g.warnings.iter().any(|w| w.message == message) {
            self.g.warnings.push(Warning { message, span });
        }
    }

    // ---- nodes

    fn node(&mut self, kind: NodeKind, name: String, span: Option<Range>) -> usize {
        let i = self.g.nodes.len();
        self.g.nodes.push(Node {
            id: format!("n{i}"),
            kind,
            name,
            columns: vec![],
            columns_known: kind != NodeKind::Table,
            written: false,
            temporary: false,
            view: false,
            statement: (kind != NodeKind::Table).then_some(self.stmt),
            span,
        });
        i
    }

    fn table_node(&mut self, name: &ObjectName) -> usize {
        let parts = name_parts(name);
        let found = self.cat.table(&parts);
        let display = found.as_ref().map(|t| t.name.clone()).unwrap_or_else(|| parts.join("."));
        let key = display.to_lowercase();
        if let Some(&i) = self.tables.get(&key) {
            return i;
        }
        // A table the script writes, named once with its schema and once
        // without (`create table tmp …; select … from public.tmp`): one node.
        // Not when the catalog knows the name (it is that table), and never
        // two different schemas.
        let last = parts.last().map(|p| p.to_lowercase()).unwrap_or_default();
        let mut same: Vec<usize> = if found.is_some() {
            vec![]
        } else {
            self.tables
                .iter()
                .filter(|&(k, &i)| {
                    let n = &self.g.nodes[i];
                    let bare = !n.name.contains('.');
                    n.written && (if parts.len() == 1 { k.rsplit('.').next() == Some(last.as_str()) } else { bare && *k == last })
                })
                .map(|(_, &i)| i)
                .collect()
        };
        same.sort();
        same.dedup();
        if let [i] = same[..] {
            if parts.len() > 1 {
                // The qualified name is the better label (it was bare).
                self.g.nodes[i].name = display.clone();
            }
            self.tables.insert(key, i);
            return i;
        }
        let span = self.range(name.span());
        let i = self.node(NodeKind::Table, display, span);
        if let Some(t) = found {
            self.g.nodes[i].columns = t.columns.into_iter().map(|c| Column { name: c, span: None, expr: None }).collect();
            self.g.nodes[i].columns_known = true;
        }
        self.tables.insert(key, i);
        i
    }

    fn col_index(&self, node: usize, name: &str) -> Option<usize> {
        let cols = &self.g.nodes[node].columns;
        cols.iter().position(|c| c.name == name).or_else(|| cols.iter().position(|c| c.name.eq_ignore_ascii_case(name)))
    }

    /// Column of a table node, added when only referenced (unknown columns).
    fn ensure_col(&mut self, node: usize, name: &str) -> String {
        match self.col_index(node, name) {
            Some(i) => self.g.nodes[node].columns[i].name.clone(),
            None => {
                self.g.nodes[node].columns.push(Column { name: name.to_string(), span: None, expr: None });
                name.to_string()
            }
        }
    }

    fn add_out(&mut self, node: usize, name: String, span: Option<Range>, expr: Option<String>) -> String {
        // Duplicate output names (`a, a`) get a suffix so edges stay distinct.
        let mut n = name.clone();
        let mut k = 2;
        while self.col_index(node, &n).is_some_and(|i| self.g.nodes[node].columns[i].name == n) {
            n = format!("{name}_{k}");
            k += 1;
        }
        self.g.nodes[node].columns.push(Column { name: n.clone(), span, expr });
        n
    }

    fn edge(&mut self, from: usize, from_column: Option<String>, to: usize, to_column: Option<String>, kind: EdgeKind, span: Option<Range>) {
        let (from, to) = (self.g.nodes[from].id.clone(), self.g.nodes[to].id.clone());
        let dup = self.g.edges.iter().any(|e| e.from == from && e.from_column == from_column && e.to == to && e.to_column == to_column && e.kind == kind);
        if !dup {
            self.g.edges.push(Edge { from, from_column, to, to_column, kind, span, detail: None, statement: Some(self.stmt) });
        }
    }

    fn rename_columns(&mut self, node: usize, alias: &TableAlias) {
        for (i, c) in alias.columns.iter().enumerate() {
            if let Some(col) = self.g.nodes[node].columns.get_mut(i) {
                col.name = c.name.value.clone();
            }
        }
    }

    /// The statement creates `t` with exactly these columns. Edges of an
    /// earlier version (a dropped and recreated table) to columns it no
    /// longer has stay, and are drawn to the table as a whole.
    fn define_columns(&mut self, t: usize, names: &[String]) {
        self.g.nodes[t].columns = names.iter().map(|n| Column { name: n.clone(), span: None, expr: None }).collect();
        self.g.nodes[t].columns_known = true;
        self.created.insert(t);
    }

    /// `CREATE … IF NOT EXISTS` of a table the catalog knows: it stays as it is.
    fn exists_already(&self, name: &ObjectName, if_not_exists: bool) -> bool {
        if_not_exists && self.cat.table(&name_parts(name)).is_some()
    }

    /// Edges added since `at` from a node into itself (`SET a = a + 1`,
    /// `WHERE` on the target's own columns): not lineage.
    fn drop_self_edges(&mut self, at: usize) {
        let tail: Vec<Edge> = self.g.edges.drain(at..).filter(|e| e.from != e.to).collect();
        self.g.edges.extend(tail);
    }

    /// A statement that does not parse: tables after FROM/JOIN/INTO/UPDATE/TABLE, without lineage.
    fn tables_by_tokens(&mut self) {
        let dialect = dialect_for(self.kind);
        let Ok(tokens) = Tokenizer::new(&*dialect, self.text).tokenize() else { return };
        let words: Vec<&Token> = tokens.iter().filter(|t| !matches!(t, Token::Whitespace(_))).collect();
        let mut i = 0;
        while i < words.len() {
            let after = matches!(words[i], Token::Word(w) if ["FROM", "JOIN", "INTO", "UPDATE", "TABLE"].contains(&w.value.to_uppercase().as_str()));
            i += 1;
            let trigger = |t: Option<&&Token>| matches!(t, Some(Token::Word(w)) if w.quote_style.is_none() && ["FROM", "JOIN", "INTO", "UPDATE", "TABLE"].contains(&w.value.to_uppercase().as_str()));
            if !after || trigger(words.get(i)) {
                continue;
            }
            let mut parts: Vec<Ident> = Vec::new();
            while let Some(Token::Word(w)) = words.get(i) {
                parts.push(Ident { value: w.value.clone(), quote_style: w.quote_style, span: Span::empty() });
                if !matches!(words.get(i + 1), Some(Token::Period)) {
                    i += 1;
                    break;
                }
                i += 2;
            }
            let keyword =
                parts.len() == 1 && parts[0].quote_style.is_none() && ["SELECT", "LATERAL", "ONLY", "IF"].contains(&parts[0].value.to_uppercase().as_str());
            if !parts.is_empty() && !keyword {
                self.table_node(&ObjectName::from(parts));
            }
        }
    }
}
