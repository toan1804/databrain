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
        edge_keys: Default::default(),
        warned: Default::default(),
        missed: false,
        search: vec![],
        view_schema: vec![],
        untraced_nodes: Default::default(),
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
    /// Every edge's key, so adding one doesn't scan them all.
    edge_keys: std::collections::HashSet<EdgeKey>,
    /// Messages already warned about.
    warned: std::collections::HashSet<String>,
    /// Set when a reference couldn't be placed (or was guessed): the column
    /// being built is [`Column::untraced`].
    missed: bool,
    /// Schemas a bare table name is looked up in first, set by `USE` and
    /// `SET search_path` (each one the name's parts before the table).
    search: Vec<Vec<String>>,
    /// The schema of each view whose definition is being traced: bare names
    /// there are the view's schema's.
    view_schema: Vec<Vec<String>>,
    /// Nodes with open columns that couldn't be fully traced (a table
    /// function reading a name it couldn't place): every column the SQL
    /// names on them is [`Column::untraced`].
    untraced_nodes: std::collections::HashSet<usize>,
}

/// See [`Builder::fold`].
pub(super) fn fold_ident(kind: ConnectorKind, i: &Ident) -> String {
    match kind {
        ConnectorKind::Clickhouse => i.value.clone(),
        ConnectorKind::Postgres if i.quote_style.is_some() && i.value != i.value.to_lowercase() => format!("\"{}\"", i.value),
        ConnectorKind::Oracle | ConnectorKind::Snowflake if i.quote_style.is_some() && i.value != i.value.to_uppercase() => format!("\"{}\"", i.value),
        _ => i.value.to_lowercase(),
    }
}

type EdgeKey = (String, Option<String>, String, Option<String>, EdgeKind);

fn edge_key(e: &Edge) -> EdgeKey {
    (e.from.clone(), e.from_column.clone(), e.to.clone(), e.to_column.clone(), e.kind)
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
        if self.warned.insert(message.clone()) {
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

    /// The catalog's table for a name as written: a bare name is looked up
    /// in the view's schema (inside a view's definition) or the schemas
    /// `USE` / `SET search_path` chose first. With the qualified name it
    /// was found by.
    fn lookup(&self, parts: &[String]) -> (Option<CatalogTable>, Vec<String>) {
        if parts.len() == 1 {
            let chosen: &[Vec<String>] = if self.in_view > 0 { self.view_schema.last().map(std::slice::from_ref).unwrap_or_default() } else { &self.search };
            let prefixes: Vec<&Vec<String>> = chosen.iter().filter(|p| !p.is_empty()).collect();
            for prefix in &prefixes {
                let full: Vec<String> = prefix.iter().chain(parts).cloned().collect();
                if let Some(t) = self.cat.table(&full) {
                    return (Some(t), full);
                }
            }
            // Not cached there: still the chosen schema's table, not one of
            // the same name elsewhere.
            if let Some(prefix) = prefixes.first() {
                return (None, prefix.iter().chain(parts).cloned().collect());
            }
        }
        (self.cat.table(parts), parts.to_vec())
    }

    /// One part of a name as the dialect compares it: unquoted names fold
    /// to lower case; quoted ones keep their case where the dialect does
    /// (`"Orders"` is not `orders` on Postgres, Oracle and Snowflake; on
    /// ClickHouse every name keeps it), marked so they can't meet a folded one.
    pub(super) fn fold(&self, i: &Ident) -> String {
        fold_ident(self.kind, i)
    }

    /// The name a table is referred to by in a statement: its alias, else its name's last part.
    pub(super) fn alias_key(&self, alias: &Option<TableAlias>, name: &ObjectName) -> String {
        alias.as_ref().map(|a| self.fold(&a.name)).unwrap_or_else(|| self.fold_name(name).last().cloned().unwrap_or_default())
    }

    pub(super) fn fold_name(&self, name: &ObjectName) -> Vec<String> {
        name.0
            .iter()
            .map(|p| match p {
                sqlparser::ast::ObjectNamePart::Identifier(i) => self.fold(i),
                other => other.to_string().to_lowercase(),
            })
            .collect()
    }

    /// The key of the table node for `name` (see [`Self::table_node`]).
    fn table_key(&self, name: &ObjectName) -> String {
        let (found, full) = self.lookup(&name_parts(name));
        match found {
            Some(t) => t.name.to_lowercase(),
            None if full.len() > name.0.len() => {
                let mut k: Vec<String> = full[..full.len() - 1].iter().map(|p| p.to_lowercase()).collect();
                k.extend(self.fold_name(name));
                k.join(".")
            }
            None => self.fold_name(name).join("."),
        }
    }

    fn table_node(&mut self, name: &ObjectName) -> usize {
        let parts = name_parts(name);
        let (found, full) = self.lookup(&parts);
        let display = found.as_ref().map(|t| t.name.clone()).unwrap_or_else(|| full.join("."));
        let key = self.table_key(name);
        if let Some(&i) = self.tables.get(&key) {
            return i;
        }
        let parts = full;
        // A table the script writes, named once with its schema and once
        // without (`create table tmp …; select … from public.tmp`): one node.
        // Not when the catalog knows the name (it is that table), and never
        // two different schemas.
        let last = key.rsplit('.').next().unwrap_or_default().to_string();
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

    /// Mark column `col` of `node` [`Column::untraced`].
    fn set_untraced(&mut self, node: usize, col: &str) {
        if let Some(i) = self.col_index(node, col) {
            self.g.nodes[node].columns[i].untraced = true;
        }
    }

    /// Columns of this node are only known by the names the SQL uses (a
    /// database table not in the catalog, a table function).
    fn open_columns(&self, node: usize) -> bool {
        !self.g.nodes[node].columns_known
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
                let mut c = Column::named(name);
                c.untraced = self.untraced_nodes.contains(&node);
                self.g.nodes[node].columns.push(c);
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
        self.g.nodes[node].columns.push(Column { name: n.clone(), span, expr, used: false, used_by: vec![], untraced: false });
        n
    }

    fn edge(&mut self, from: usize, from_column: Option<String>, to: usize, to_column: Option<String>, kind: EdgeKind, span: Option<Range>) {
        let (from, to) = (self.g.nodes[from].id.clone(), self.g.nodes[to].id.clone());
        let e = Edge { from, from_column, to, to_column, kind, span, detail: None, statement: Some(self.stmt) };
        if self.edge_keys.insert(edge_key(&e)) {
            self.g.edges.push(e);
        }
    }

    /// Rewrite edges in place (`f` returns whether it changed one), keeping the keys in step.
    fn rewrite_edges(&mut self, mut f: impl FnMut(&mut Edge) -> bool) {
        let mut changed = false;
        for e in &mut self.g.edges {
            changed |= f(e);
        }
        if changed {
            // Two edges may be one now.
            self.edge_keys.clear();
            let edges = std::mem::take(&mut self.g.edges);
            for e in edges {
                if self.edge_keys.insert(edge_key(&e)) {
                    self.g.edges.push(e);
                }
            }
        }
    }

    /// A column list (`c(a, b)`, `(…) s(a, b)`) names `node`'s columns in
    /// order; the edges into and out of them follow.
    fn rename_columns(&mut self, node: usize, alias: &TableAlias) {
        let id = self.g.nodes[node].id.clone();
        let mut renamed: Vec<(String, String)> = Vec::new();
        for (i, c) in alias.columns.iter().enumerate() {
            if let Some(col) = self.g.nodes[node].columns.get_mut(i)
                && col.name != c.name.value
            {
                renamed.push((col.name.clone(), c.name.value.clone()));
                col.name = c.name.value.clone();
            }
        }
        if renamed.is_empty() {
            return;
        }
        let new = |old: &str| renamed.iter().find(|(o, _)| o == old).map(|(_, n)| n.clone());
        self.rewrite_edges(|e| {
            let mut changed = false;
            if e.to == id
                && let Some(n) = e.to_column.as_deref().and_then(new)
            {
                e.to_column = Some(n);
                changed = true;
            }
            if e.from == id
                && let Some(n) = e.from_column.as_deref().and_then(new)
            {
                e.from_column = Some(n);
                changed = true;
            }
            changed
        });
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
        // Each (node, column) gets the set of statements using it (a bit
        // set); sets flow backwards along value edges until nothing changes.
        // Every set only grows, so this ends, and a column is revisited only
        // when new statements reach it (not once per statement).
        let index: HashMap<&str, usize> = self.g.nodes.iter().enumerate().map(|(i, n)| (n.id.as_str(), i)).collect();
        let words = self.g.statements.len().max(self.stmt + 1).div_ceil(64);
        let mut ids: HashMap<(usize, String), usize> = HashMap::new();
        let mut sets: Vec<Vec<u64>> = Vec::new();
        let mut preds: Vec<Vec<usize>> = Vec::new();
        let mut id = |n: usize, c: &str, sets: &mut Vec<Vec<u64>>, preds: &mut Vec<Vec<usize>>| -> usize {
            *ids.entry((n, c.to_string())).or_insert_with(|| {
                sets.push(vec![0; words]);
                preds.push(vec![]);
                sets.len() - 1
            })
        };
        let set_bit = |set: &mut Vec<u64>, s: usize| set[s / 64] |= 1 << (s % 64);
        for (n, c, s) in &self.uses {
            let k = id(*n, c, &mut sets, &mut preds);
            set_bit(&mut sets[k], *s);
        }
        for e in &self.g.edges {
            let (Some(&f), Some(&t)) = (index.get(e.from.as_str()), index.get(e.to.as_str())) else { continue };
            let Some(fc) = e.from_column.as_deref() else { continue };
            let from = id(f, fc, &mut sets, &mut preds);
            match e.to_column.as_deref() {
                Some(tc) => {
                    let to = id(t, tc, &mut sets, &mut preds);
                    preds[to].push(from);
                }
                None => set_bit(&mut sets[from], e.statement.unwrap_or(0)),
            }
        }
        for (i, n) in self.g.nodes.iter().enumerate() {
            if n.kind == NodeKind::Result {
                for c in &n.columns {
                    let k = id(i, &c.name, &mut sets, &mut preds);
                    set_bit(&mut sets[k], n.statement.unwrap_or(0));
                }
            }
        }
        let mut todo: Vec<usize> = (0..sets.len()).filter(|&k| sets[k].iter().any(|w| *w != 0)).collect();
        while let Some(k) = todo.pop() {
            let from = sets[k].clone();
            for &p in &preds[k] {
                let mut changed = false;
                for (w, bits) in sets[p].iter_mut().zip(&from) {
                    changed |= *w | bits != *w;
                    *w |= bits;
                }
                if changed {
                    todo.push(p);
                }
            }
        }
        let several = self.g.statements.len() > 1;
        for (i, n) in self.g.nodes.iter_mut().enumerate() {
            if n.kind == NodeKind::Table {
                for c in &mut n.columns {
                    let set = ids.get(&(i, c.name.clone())).map(|&k| &sets[k]);
                    let stmts: Vec<usize> = set.map(|s| (0..words * 64).filter(|b| s[b / 64] & (1 << (b % 64)) != 0).collect()).unwrap_or_default();
                    c.used = !stmts.is_empty();
                    // Only scripts of several statements need to tell them apart.
                    c.used_by = if several { stmts } else { vec![] };
                }
            }
        }
    }

    /// `CREATE … IF NOT EXISTS` of a table the catalog knows: it stays as it is.
    fn exists_already(&self, name: &ObjectName, if_not_exists: bool) -> bool {
        if_not_exists && self.lookup(&name_parts(name)).0.is_some()
    }

    /// Edges added since `at` from a node into itself (`SET a = a + 1`,
    /// `WHERE` on the target's own columns): not lineage.
    fn drop_self_edges(&mut self, at: usize) {
        let tail: Vec<Edge> = self.g.edges.drain(at..).collect();
        for e in tail {
            if e.from == e.to {
                self.edge_keys.remove(&edge_key(&e));
            } else {
                self.g.edges.push(e);
            }
        }
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
