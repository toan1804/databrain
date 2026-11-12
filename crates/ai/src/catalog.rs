//! The AI's view of a connection's tables, and the only reader of table
//! facts in this crate: the workspace's [`AiCatalog`] (the user's AI scope
//! over the explorer cache) with the connection's table access rules
//! (`read_allow` / `read_deny`) applied on top. A table outside either reads
//! as missing, from every method.
//!
//! The test at the end of this file fails if code in this crate reads table
//! facts any other way.

use databrain_connector_core::ForeignKey;
use databrain_workspace::{AiCatalog, ConnectionProfile, KnHit, KnObject, Workspace};

use crate::access;
use crate::types::Result;

pub struct AiTables<'a> {
    cat: AiCatalog<'a>,
    profile: ConnectionProfile,
}

impl<'a> AiTables<'a> {
    pub fn new(ws: &'a Workspace, connection_id: &'a str) -> Result<Self> {
        Ok(Self { cat: ws.ai_catalog(connection_id), profile: ws.get_connection(connection_id)? })
    }

    pub fn with_profile(ws: &'a Workspace, profile: &'a ConnectionProfile) -> Self {
        Self { cat: ws.ai_catalog(&profile.id), profile: profile.clone() }
    }

    fn rules(&self) -> bool {
        access::has_rules(&self.profile.ai_policy)
    }

    /// The access rules let the AI read `schema.name`.
    pub fn readable(&self, full_name: &str) -> bool {
        !self.rules() || access::check_read_table(&self.profile, full_name).is_ok()
    }

    /// The table the key points at, `schema.table`.
    fn target(o: &KnObject, fk: &ForeignKey) -> String {
        let schema = if fk.ref_schema.is_empty() { o.schema.as_str() } else { fk.ref_schema.as_str() };
        format!("{schema}.{}", fk.ref_table)
    }

    /// `o` without foreign keys to tables the AI may not use: a key would
    /// name them.
    fn scrub(&self, mut o: KnObject) -> Result<KnObject> {
        let mut keep = Vec::with_capacity(o.foreign_keys.len());
        for fk in std::mem::take(&mut o.foreign_keys) {
            let t = Self::target(&o, &fk);
            if self.readable(&t) && self.cat.get(&t)?.is_some() {
                keep.push(fk);
            }
        }
        o.foreign_keys = keep;
        Ok(o)
    }

    pub fn get(&self, reference: &str) -> Result<Option<KnObject>> {
        match self.cat.get(reference)?.filter(|o| self.readable(&o.full_name())) {
            Some(o) => Ok(Some(self.scrub(o)?)),
            None => Ok(None),
        }
    }

    pub fn resolve_path(&self, path: &str) -> Result<Option<(KnObject, Option<String>)>> {
        match self.cat.resolve_path(path)?.filter(|(o, _)| self.readable(&o.full_name())) {
            Some((o, c)) => Ok(Some((self.scrub(o)?, c))),
            None => Ok(None),
        }
    }

    pub fn objects(&self) -> Result<Vec<KnObject>> {
        let all: Vec<KnObject> = self.cat.objects()?.into_iter().filter(|o| self.readable(&o.full_name())).collect();
        // One pass: keys kept only when their target is among these. A key
        // may name the schema without its catalog (`sales.t` for
        // `main.sales.t`), as `get` accepts: every shorter form counts.
        let mut names: std::collections::HashSet<String> = std::collections::HashSet::new();
        for o in &all {
            let full = o.full_name().to_lowercase();
            let parts: Vec<&str> = full.split('.').collect();
            for i in 0..parts.len().saturating_sub(1) {
                names.insert(parts[i..].join("."));
            }
        }
        Ok(all
            .into_iter()
            .map(|mut o| {
                let fks = std::mem::take(&mut o.foreign_keys);
                o.foreign_keys = fks.into_iter().filter(|fk| names.contains(&Self::target(&o, fk).to_lowercase())).collect();
                o
            })
            .collect())
    }

    pub fn count(&self) -> Result<i64> {
        if self.rules() { Ok(self.objects()?.len() as i64) } else { Ok(self.cat.count()?) }
    }

    /// Tables in `schemas` the AI may use.
    pub fn count_in(&self, schemas: &[String]) -> Result<usize> {
        if self.rules() { Ok(self.objects()?.iter().filter(|o| schemas.iter().any(|s| s == &o.schema)).count()) } else { Ok(self.cat.count_in(schemas)?) }
    }

    /// A page of the tables the AI may use (Knowledge page). With access
    /// rules, tables they deny are left out of the page and the total.
    pub fn page(&self, filter: &str, offset: usize, limit: usize) -> Result<(Vec<KnObject>, usize)> {
        if !self.rules() {
            return Ok(self.cat.page(filter, offset, limit)?);
        }
        let (all, _) = self.cat.page(filter, 0, usize::MAX >> 1)?;
        let kept: Vec<KnObject> = all.into_iter().filter(|o| self.readable(&o.full_name())).collect();
        let total = kept.len();
        Ok((kept.into_iter().skip(offset).take(limit).collect(), total))
    }

    /// Search hits: notes, and the tables the AI may use.
    pub fn search(&self, text: &str, limit: usize) -> Result<Vec<KnHit>> {
        let hits = self.cat.search(text, limit)?;
        Ok(if self.rules() { hits.into_iter().filter(|h| h.source != "object" || self.readable(&h.reference)).collect() } else { hits })
    }

    /// A note the AI may see: a glossary note, or one whose every target
    /// table (and column) is one the AI may use. A note about a table
    /// outside the scope would name it.
    pub fn note_visible(&self, target: Option<&str>) -> bool {
        match target.map(str::trim).filter(|t| !t.is_empty()) {
            None => true,
            Some(t) => databrain_workspace::split_target(t).iter().all(|(_, p)| self.resolve_path(p).ok().flatten().is_some()),
        }
    }

    pub fn column_names(&self, schema: &str, name: &str) -> Result<Option<Vec<String>>> {
        if !self.readable(&format!("{schema}.{name}")) {
            return Ok(None);
        }
        Ok(self.cat.column_names(schema, name)?)
    }

    pub fn foreign_keys_to(&self, table: &str) -> Result<Option<Vec<(String, String, ForeignKey)>>> {
        Ok(self.cat.foreign_keys_to(table)?.map(|v| v.into_iter().filter(|(s, n, _)| self.readable(&format!("{s}.{n}"))).collect()))
    }
}

#[cfg(test)]
mod boundary {
    /// AI code reads table facts only through [`super::AiTables`]: none of
    /// the unfiltered readers (the index's or the explorer cache's) appears
    /// anywhere else in `crates/ai/src`.
    #[test]
    fn ai_code_reads_tables_only_through_ai_tables() {
        const FORBIDDEN: &[&str] = &[
            ".kn_get(",
            ".kn_objects(",
            ".kn_count(",
            ".kn_search(",
            ".kn_column_names(",
            ".kn_list_objects(",
            ".kn_foreign_keys_to(",
            ".kn_resolve_path(",
            ".kn_complete(",
            ".kn_objects_in(",
            ".ai_catalog(",
            ".meta_table(",
            ".meta_columns(",
            ".meta_column_names(",
            ".meta_object(",
            ".meta_listing(",
            ".meta_get(",
            ".meta_resolve_path(",
            ".tables_named(",
            ".meta_foreign_keys_to(",
            ".kn_notes(",
        ];
        // Where they are used on purpose, each one checked against the AI's
        // scope (`file:reader`): index_tables' refusal check, note tools
        // that filter with `note_visible`, a note target checked first,
        // learning joins.
        const ALLOWED: &[(&str, &str)] = &[
            ("knowledge.rs", ".meta_get("),
            ("knowledge.rs", ".meta_resolve_path("),
            ("knowledge.rs", ".kn_notes("),
            ("notes.rs", ".kn_notes("),
            ("schema.rs", ".kn_notes("),
            // Joins learned from the user's own history; shown only when
            // both tables pass AiTables (knowledge.rs).
            ("learn.rs", ".tables_named("),
        ];
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut bad = vec![];
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                    continue;
                }
                if p.extension().is_none_or(|x| x != "rs") || p.ends_with("catalog.rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&p).unwrap();
                for (i, line) in text.lines().enumerate() {
                    let file = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    if let Some(f) = FORBIDDEN.iter().find(|f| line.contains(**f) && !ALLOWED.contains(&(file, **f))) {
                        bad.push(format!("{}:{}: {f}", p.display(), i + 1));
                    }
                }
            }
        }
        assert!(bad.is_empty(), "read table facts through catalog::AiTables:\n{}", bad.join("\n"));
    }
}
