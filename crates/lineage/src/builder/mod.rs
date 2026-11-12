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
        uses: Default::default(),
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
    b.mark_used();
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
    /// Columns the SQL names itself (node, column): where [`Builder::mark_used`] starts.
    /// (node, column, statement).
    uses: std::collections::HashSet<(usize, String, usize)>,
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
            self.g.nodes[i].columns = t.columns.into_iter().map(Column::named).collect();
            self.g.nodes[i].columns_known = true;
        }
        self.tables.insert(key, i);
        i
    }

    fn col_index(&self, node: usize, name: &str) -> Option<usize> {
        let cols = &self.g.nodes[node].columns;
        cols.iter().position(|c| c.name == name).or_else(|| cols.iter().position(|c| c.name.eq_ignore_ascii_case(name)))
    }

    /// Column of a table node the SQL uses, added when only referenced
    /// (unknown columns). Inside a view's definition it counts as used only
    /// when a column of the view that needs it is (see [`Self::mark_used`]).
    fn ensure_col(&mut self, node: usize, name: &str) -> String {
        let c = self.col_or_add(node, name);
        if self.in_view == 0 {
            self.uses.insert((node, c.clone(), self.stmt));
        }
        c
    }

    /// [`Self::ensure_col`] without counting it as used.
    fn col_or_add(&mut self, node: usize, name: &str) -> String {
        match self.col_index(node, name) {
            Some(i) => self.g.nodes[node].columns[i].name.clone(),
            None => {
                self.g.nodes[node].columns.push(Column::named(name));
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
        self.g.nodes[node].columns.push(Column { name: n.clone(), span, expr, used: false, used_by: vec![] });
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
        self.g.nodes[t].columns = names.iter().map(|n| Column::named(n.clone())).collect();
        self.g.nodes[t].columns_known = true;
        self.created.insert(t);
        let stmt = self.stmt;
        self.uses.extend(names.iter().map(|n| (t, n.clone(), stmt)));
    }

    /// Set [`Column::used`] on table and view columns: those the SQL names
    /// (selected, computed, joined, filtered, grouped, ordered, written,
    /// defined) plus, walking edges back from what the statements return or
    /// write, every column that feeds them (so a `*` uses all of a table's
    /// columns only when they are all used further on, and a view's
    /// definition uses only what the columns read from the view need).
    /// Join and filter inputs always count: they decide the rows.
    /// Each column also lists the statements that use it ([`Column::used_by`]):
    /// those naming it, and those whose results or writes it feeds.
    fn mark_used(&mut self) {
        use std::collections::{BTreeSet, HashSet};
        let index: HashMap<&str, usize> = self.g.nodes.iter().enumerate().map(|(i, n)| (n.id.as_str(), i)).collect();
        let mut into: HashMap<(usize, &str), Vec<(usize, &str)>> = HashMap::new();
        // (node, column, the statement it is used for).
        let mut todo: Vec<(usize, String, usize)> = self.uses.iter().cloned().collect();
        for e in &self.g.edges {
            let (Some(&f), Some(&t)) = (index.get(e.from.as_str()), index.get(e.to.as_str())) else { continue };
            let Some(fc) = e.from_column.as_deref() else { continue };
            match e.to_column.as_deref() {
                Some(tc) => into.entry((t, tc)).or_default().push((f, fc)),
                None => todo.push((f, fc.to_string(), e.statement.unwrap_or(0))),
            }
        }
        for (i, n) in self.g.nodes.iter().enumerate() {
            if n.kind == NodeKind::Result {
                todo.extend(n.columns.iter().map(|c| (i, c.name.clone(), n.statement.unwrap_or(0))));
            }
        }
        let mut seen: HashSet<(usize, String, usize)> = HashSet::new();
        let mut by: HashMap<(usize, String), BTreeSet<usize>> = HashMap::new();
        while let Some(k) = todo.pop() {
            if seen.contains(&k) {
                continue;
            }
            if let Some(from) = into.get(&(k.0, k.1.as_str())) {
                todo.extend(from.iter().map(|&(f, c)| (f, c.to_string(), k.2)));
            }
            by.entry((k.0, k.1.clone())).or_default().insert(k.2);
            seen.insert(k);
        }
        let several = self.g.statements.len() > 1;
        for (i, n) in self.g.nodes.iter_mut().enumerate() {
            if n.kind == NodeKind::Table {
                for c in &mut n.columns {
                    let stmts = by.get(&(i, c.name.clone()));
                    c.used = stmts.is_some();
                    // Only scripts of several statements need to tell them apart.
                    c.used_by = if several { stmts.map(|s| s.iter().copied().collect()).unwrap_or_default() } else { vec![] };
                }
            }
        }
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
