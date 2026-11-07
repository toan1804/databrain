use sqlparser::ast::Ident;
use sqlparser::tokenizer::Span;

use super::*;
use crate::*;

impl<'a> Builder<'a> {
    pub(super) fn find_source(&self, sources: &[Source], qual: &[String]) -> Option<Source> {
        let q = qual.last()?.to_lowercase();
        let full = qual.join(".").to_lowercase();
        sources
            .iter()
            .find(|s| s.alias == q)
            .or_else(|| {
                sources.iter().find(|s| {
                    let name = self.g.nodes[s.node].name.to_lowercase();
                    name == full || name.ends_with(&format!(".{full}"))
                })
            })
            .cloned()
    }

    /// The source column a reference means: (node, column name).
    pub(super) fn resolve(&mut self, sources: &[Source], outer: &[Source], parts: &[Ident]) -> Option<(usize, String)> {
        self.resolve_source(sources, outer, parts).map(|(_, n, c)| (n, c))
    }

    /// [`Self::resolve`], also returning the source (alias) the column was found in.
    pub(super) fn resolve_source(&mut self, sources: &[Source], outer: &[Source], parts: &[Ident]) -> Option<(Source, usize, String)> {
        let span = self.range(Span::union_iter(parts.iter().map(|i| i.span)));
        let written = parts.iter().map(|i| i.value.as_str()).collect::<Vec<_>>().join(".");
        let col = parts.last()?.value.clone();
        if parts.len() >= 2 {
            let qual: Vec<String> = parts[..parts.len() - 1].iter().map(|i| i.value.clone()).collect();
            for scope in [sources, outer] {
                if let Some(s) = self.find_source(scope, &qual) {
                    return self.column_of(&s, &col, &written, span).map(|(n, c)| (s, n, c));
                }
            }
            // `col.field` (struct / JSON access) on an unqualified column.
            if let Some(r) = self.resolve_source(sources, outer, &parts[..1]) {
                return Some(r);
            }
            self.warn(format!("Unknown table or alias in {written}"), span);
            return None;
        }
        for scope in [sources, outer] {
            let with: Vec<&Source> = scope.iter().filter(|s| self.col_index(s.node, &col).is_some()).collect();
            if with.len() > 1 {
                let names: Vec<&str> = with.iter().map(|s| s.alias.as_str()).collect();
                self.warn(format!("Column {col} is ambiguous (in {})", names.join(", ")), span);
            }
            if let Some(s) = with.first() {
                let s = (*s).clone();
                return self.column_of(&s, &col, &written, span).map(|(n, c)| (s, n, c));
            }
            // Only tables with unknown columns could have it.
            let unknown: Vec<&Source> = scope.iter().filter(|s| self.g.nodes[s.node].kind == NodeKind::Table && !self.g.nodes[s.node].columns_known).collect();
            if unknown.len() == 1 {
                let s = unknown[0].clone();
                return self.column_of(&s, &col, &written, span).map(|(n, c)| (s, n, c));
            }
            if unknown.len() > 1 {
                self.warn(format!("Can't tell which table {col} belongs to (columns not cached); qualify it as alias.{col}"), span);
                return None;
            }
        }
        self.warn(format!("Unknown column {written}"), span);
        None
    }

    pub(super) fn column_of(&mut self, s: &Source, col: &str, written: &str, span: Option<Range>) -> Option<(usize, String)> {
        let node = &self.g.nodes[s.node];
        if self.col_index(s.node, col).is_some() || (node.kind == NodeKind::Table && !node.columns_known) {
            return Some((s.node, self.ensure_col(s.node, col)));
        }
        if node.kind == NodeKind::Table {
            let name = node.name.clone();
            self.warn(format!("{written}: {name} has no column {col} (in the cached schema)"), span);
            return Some((s.node, self.ensure_col(s.node, col)));
        }
        let name = node.name.clone();
        self.warn(format!("{written}: {name} has no column {col}"), span);
        None
    }
}
