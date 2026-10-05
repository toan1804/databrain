//! Query outputs: every result set gets a short handle (`r12`) and optional
//! name (`revenue`), provenance (SQL, connection, limit), pinning and a
//! memory budget. Outputs are queryable from DuckDB sessions as
//! `results.<handle|name>`; `results.<name>__1` is the previous version of a
//! named output.
//!
//! Lifecycle:
//! - the outputs of a tab's latest run are *active* (shown in the UI) and are
//!   never evicted;
//! - older outputs are *recent*: kept in memory until the budget is exceeded,
//!   then the least recently used are evicted (metadata stays, data is freed);
//! - *pinned* outputs are never evicted and, when a snapshot directory is set,
//!   are saved as Parquet and restored on the next start;
//! - each tab's latest (active) outputs are saved when the app quits
//!   ([`OutputRegistry::persist_active`], up to [`AUTO_MAX_BYTES`] each) and
//!   restored with the tab, so a reopened tab still shows its last result.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use databrain_connector_core::arrow::array::{Array, ArrayRef, RecordBatch, StringArray};
use databrain_connector_core::arrow::compute::cast;
use databrain_connector_core::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use databrain_connector_core::external::{ExternalTable, ExternalTables, RESULTS_SCHEMA};
use databrain_connector_core::{ConnectorKind, quote_ident};
use databrain_result_store::{ColumnMeta, ResultInfo, ResultStore, ViewSpec, display};
use databrain_workspace::{Origin, OutputRecord, Workspace, now_ms};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::{EngineError, Result};

/// Versions kept per name (current + previous ones).
/// Rows per batch when an output is saved or read by SQL.
const SNAPSHOT_CHUNK: usize = 65_536;
const KEEP_VERSIONS: usize = 5;
/// Metadata entries kept for evicted outputs.
const MAX_ENTRIES: usize = 400;
const DEFAULT_BUDGET: usize = 1024 * 1024 * 1024;
/// Largest active output saved on quit (in-memory size).
pub const AUTO_MAX_BYTES: usize = 64 * 1024 * 1024;
/// Total size of active outputs saved on quit.
pub const AUTO_TOTAL_BYTES: usize = 512 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputState {
    /// Data in memory.
    Live,
    /// Pinned snapshot on disk, loaded on first use.
    OnDisk,
    /// Data freed; re-run the SQL to get it back.
    Evicted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputInfo {
    pub handle: String,
    /// Set on the current version of a named output.
    pub name: Option<String>,
    /// For older versions: the name and how many versions back (`revenue`, 1).
    pub version_of: Option<(String, usize)>,
    pub result_id: String,
    pub connection_id: String,
    pub connection_name: String,
    pub kind: ConnectorKind,
    pub sql: String,
    pub tab_id: String,
    pub statement_index: usize,
    pub created_at: i64,
    pub rows: usize,
    pub columns: Vec<ColumnMeta>,
    /// Cut at the row limit: more rows exist at the source.
    pub truncated: bool,
    pub row_limit: Option<usize>,
    pub bytes: usize,
    pub pinned: bool,
    pub state: OutputState,
    pub origin: Origin,
    pub last_used: i64,
    /// Latest run of its tab (shown in the UI).
    #[serde(default)]
    pub active: bool,
}

impl OutputInfo {
    /// Preferred SQL reference, e.g. `results.revenue` or `results.r12`.
    pub fn reference(&self) -> String {
        let t = match (&self.name, &self.version_of) {
            (Some(n), _) => n.clone(),
            (None, Some((n, k))) => format!("{n}__{k}"),
            _ => self.handle.clone(),
        };
        format!("{RESULTS_SCHEMA}.{}", q_min(&t))
    }
}

/// DuckDB identifier, quoted only when needed.
pub fn q_min(s: &str) -> String {
    const RESERVED: &[&str] = &["select", "from", "where", "group", "order", "by", "table", "join", "union", "all", "as", "on", "and", "or", "not", "null", "limit", "case", "when", "end", "desc", "asc"];
    let plain = s.chars().enumerate().all(|(i, c)| c == '_' || c.is_ascii_lowercase() || (i > 0 && c.is_ascii_digit())) && !s.is_empty();
    if plain && !RESERVED.contains(&s) { s.to_string() } else { quote_ident(ConnectorKind::Duckdb, s) }
}

/// Everything needed to register a new output.
pub struct NewOutput {
    pub result: ResultInfo,
    pub connection_id: String,
    pub connection_name: String,
    pub kind: ConnectorKind,
    pub sql: String,
    pub tab_id: String,
    pub statement_index: usize,
    pub row_limit: Option<usize>,
    pub origin: Origin,
}

#[derive(Default)]
struct Inner {
    by_handle: HashMap<String, OutputInfo>,
    by_result: HashMap<String, String>,
    /// lowercase name → handles, newest first.
    names: HashMap<String, Vec<String>>,
    /// tab → handles of its latest run.
    active: HashMap<String, Vec<String>>,
    seq: u64,
}

pub struct OutputRegistry {
    inner: Mutex<Inner>,
    results: Arc<ResultStore>,
    workspace: Arc<Workspace>,
    snapshot_dir: Mutex<Option<PathBuf>>,
    budget: Mutex<usize>,
}

/// Name rules: identifier-like, not `r<digits>`, no `__` (version suffix).
pub fn validate_name(name: &str) -> Result<()> {
    let ok_chars = name.chars().enumerate().all(|(i, c)| c == '_' || c.is_ascii_alphanumeric() && (i > 0 || !c.is_ascii_digit()));
    if name.is_empty() || name.len() > 63 || !ok_chars {
        return Err(EngineError::new("invalid", "output names use letters, digits and _ (max 63), starting with a letter"));
    }
    if name.contains("__") {
        return Err(EngineError::new("invalid", "output names cannot contain \"__\" (used for versions, e.g. revenue__1)"));
    }
    if is_handle(name) {
        return Err(EngineError::new("invalid", "names like r12 are reserved for output handles"));
    }
    Ok(())
}

fn is_handle(s: &str) -> bool {
    s.len() > 1 && (s.starts_with('r') || s.starts_with('R')) && s[1..].chars().all(|c| c.is_ascii_digit())
}

impl OutputRegistry {
    pub fn new(results: Arc<ResultStore>, workspace: Arc<Workspace>) -> Arc<Self> {
        let seq = workspace.get_setting("outputs_seq").ok().flatten().and_then(|v| v.as_u64()).unwrap_or(0);
        Arc::new(Self { inner: Mutex::new(Inner { seq, ..Default::default() }), results, workspace, snapshot_dir: Mutex::new(None), budget: Mutex::new(DEFAULT_BUDGET) })
    }

    pub fn set_budget(&self, bytes: usize) {
        *self.budget.lock() = bytes.max(16 * 1024 * 1024);
        self.enforce_budget();
    }

    /// Enable snapshots of pinned outputs and restore the saved ones.
    pub fn set_snapshot_dir(&self, dir: PathBuf) {
        *self.snapshot_dir.lock() = Some(dir);
        let Ok(records) = self.workspace.list_outputs() else { return };
        let open_tabs: HashSet<String> = self.workspace.list_tabs().map(|t| t.into_iter().map(|t| t.id).collect()).unwrap_or_default();
        let mut g = self.inner.lock();
        for rec in records {
            let Ok(mut info) = serde_json::from_value::<OutputInfo>(rec.meta.clone()) else { continue };
            let on_disk = rec.snapshot_path.as_deref().is_some_and(|p| std::path::Path::new(p).is_file());
            // Saved on quit as a tab's last result: only while the tab exists.
            let tab_output = !info.pinned;
            if !on_disk || (tab_output && !open_tabs.contains(&info.tab_id)) {
                self.drop_record(&rec);
                continue;
            }
            if g.by_handle.contains_key(&info.handle) {
                continue;
            }
            info.state = if self.results.contains(&info.result_id) { OutputState::Live } else { OutputState::OnDisk };
            info.active = tab_output;
            if tab_output {
                g.active.entry(info.tab_id.clone()).or_default().push(info.handle.clone());
            }
            info.version_of = None;
            if let Some(n) = &info.name {
                let key = n.to_ascii_lowercase();
                if g.names.get(&key).is_some_and(|v| !v.is_empty()) {
                    info.name = None; // a newer output took the name in this session
                } else {
                    g.names.insert(key, vec![info.handle.clone()]);
                }
            }
            if let Ok(num) = info.handle[1..].parse::<u64>() {
                g.seq = g.seq.max(num);
            }
            g.by_result.insert(info.result_id.clone(), info.handle.clone());
            g.by_handle.insert(info.handle.clone(), info);
        }
    }

    fn drop_record(&self, rec: &OutputRecord) {
        let _ = self.workspace.delete_output(&rec.result_id);
        if let Some(p) = &rec.snapshot_path {
            let _ = std::fs::remove_file(p);
        }
    }

    /// Save each tab's latest outputs (called when the app quits) so they
    /// are shown again after a restart, and drop saved tab outputs that are
    /// no longer current. Pinned outputs are handled by [`Self::set_pinned`].
    /// Returns the number of outputs written.
    pub fn persist_active(&self) -> usize {
        let Some(_) = self.snapshot_dir.lock().clone() else { return 0 };
        let mut active: Vec<OutputInfo> = {
            let g = self.inner.lock();
            g.active.values().flatten().filter_map(|h| g.by_handle.get(h)).filter(|o| !o.pinned).cloned().collect()
        };
        active.sort_by_key(|o| std::cmp::Reverse(o.last_used));
        let saved: HashMap<String, OutputRecord> = self.workspace.list_outputs().unwrap_or_default().into_iter().map(|r| (r.result_id.clone(), r)).collect();
        let mut keep: HashSet<String> = HashSet::new();
        let (mut total, mut written) = (0usize, 0usize);
        for o in active {
            let bytes = if o.state == OutputState::Live { self.results.bytes_of(&o.result_id) } else { o.bytes };
            if bytes > AUTO_MAX_BYTES || total + bytes > AUTO_TOTAL_BYTES {
                continue;
            }
            let Some(path) = self.snapshot_path(&o.result_id) else { continue };
            let have_file = saved.contains_key(&o.result_id) && path.is_file();
            if !have_file {
                if o.state != OutputState::Live {
                    continue;
                }
                let Ok(rs) = self.results.get(&o.result_id) else { continue };
                let (schema, batches) = {
                    let mut g = rs.lock();
                    let Ok(b) = g.view_batches(&ViewSpec::default(), SNAPSHOT_CHUNK) else { continue };
                    (g.schema(), b)
                };
                if databrain_export::write_snapshot(&path, schema, &batches).is_err() {
                    continue;
                }
                written += 1;
            }
            let _ = self.workspace.save_output(&OutputRecord {
                result_id: o.result_id.clone(),
                handle: o.handle.clone(),
                name: o.name.clone(),
                connection_id: Some(o.connection_id.clone()),
                meta: serde_json::to_value(&o).unwrap_or_default(),
                snapshot_path: Some(path.to_string_lossy().into_owned()),
                created_at: o.created_at,
            });
            total += bytes;
            keep.insert(o.result_id);
        }
        // Previously saved tab outputs that are no longer a tab's latest.
        for (id, rec) in saved {
            let pinned = serde_json::from_value::<OutputInfo>(rec.meta.clone()).map(|i| i.pinned).unwrap_or(true);
            if !pinned && !keep.contains(&id) {
                self.drop_record(&rec);
            }
        }
        written
    }

    fn snapshot_path(&self, result_id: &str) -> Option<PathBuf> {
        let safe: String = result_id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
        self.snapshot_dir.lock().as_ref().map(|d| d.join(format!("{safe}.parquet")))
    }

    // ------------------------------------------------------------ lifecycle

    /// A new run of `tab` starts: its previous outputs become recent.
    pub fn begin_run(&self, tab: &str) {
        let mut g = self.inner.lock();
        if let Some(prev) = g.active.remove(tab) {
            for h in prev {
                if let Some(o) = g.by_handle.get_mut(&h) {
                    o.active = false;
                }
            }
        }
    }

    pub fn add(&self, n: NewOutput) -> OutputInfo {
        let now = now_ms();
        let info = {
            let mut g = self.inner.lock();
            g.seq += 1;
            let handle = format!("r{}", g.seq);
            let info = OutputInfo {
                handle: handle.clone(),
                name: None,
                version_of: None,
                result_id: n.result.id.clone(),
                connection_id: n.connection_id,
                connection_name: n.connection_name,
                kind: n.kind,
                sql: n.sql,
                tab_id: n.tab_id.clone(),
                statement_index: n.statement_index,
                created_at: now,
                rows: n.result.total_rows,
                columns: n.result.columns.clone(),
                truncated: n.result.truncated,
                row_limit: n.row_limit,
                bytes: n.result.bytes,
                pinned: false,
                state: OutputState::Live,
                origin: n.origin,
                last_used: now,
                active: true,
            };
            g.by_result.insert(info.result_id.clone(), handle.clone());
            g.active.entry(n.tab_id).or_default().push(handle.clone());
            g.by_handle.insert(handle, info.clone());
            let seq = g.seq;
            drop(g);
            let _ = self.workspace.set_setting("outputs_seq", &serde_json::json!(seq));
            info
        };
        self.enforce_budget();
        info
    }

    /// Tab closed: free its unnamed, unpinned outputs.
    pub fn close_tab(&self, tab: &str) {
        let freed: Vec<String> = {
            let mut g = self.inner.lock();
            g.active.remove(tab);
            let mut freed = Vec::new();
            for o in g.by_handle.values_mut() {
                if o.tab_id == tab {
                    o.active = false;
                    if !o.pinned && o.name.is_none() && o.state == OutputState::Live {
                        o.state = OutputState::Evicted;
                        freed.push(o.result_id.clone());
                    }
                }
            }
            freed
        };
        for id in freed {
            self.results.remove(&id);
        }
        self.trim_entries();
    }

    /// Free data of the least recently used, inactive, unpinned outputs until
    /// the budget is respected.
    pub fn enforce_budget(&self) {
        let budget = *self.budget.lock();
        let mut freed = Vec::new();
        {
            let mut g = self.inner.lock();
            let mut total: usize = g.by_handle.values().filter(|o| o.state == OutputState::Live).map(|o| o.bytes).sum();
            if total > budget {
                let mut cands: Vec<(i64, String)> = g
                    .by_handle
                    .values()
                    .filter(|o| o.state == OutputState::Live && !o.pinned && !o.active)
                    .map(|o| (o.last_used, o.handle.clone()))
                    .collect();
                cands.sort();
                for (_, h) in cands {
                    if total <= budget {
                        break;
                    }
                    if let Some(o) = g.by_handle.get_mut(&h) {
                        o.state = OutputState::Evicted;
                        total = total.saturating_sub(o.bytes);
                        freed.push(o.result_id.clone());
                    }
                }
            }
        }
        for id in freed {
            self.results.remove(&id);
        }
        self.trim_entries();
    }

    fn trim_entries(&self) {
        let mut g = self.inner.lock();
        if g.by_handle.len() <= MAX_ENTRIES {
            return;
        }
        let mut evicted: Vec<(i64, String)> = g.by_handle.values().filter(|o| o.state == OutputState::Evicted).map(|o| (o.created_at, o.handle.clone())).collect();
        evicted.sort();
        let excess = g.by_handle.len() - MAX_ENTRIES;
        for (_, h) in evicted.into_iter().take(excess) {
            if let Some(o) = g.by_handle.remove(&h) {
                g.by_result.remove(&o.result_id);
                for v in g.names.values_mut() {
                    v.retain(|x| x != &h);
                }
            }
        }
    }

    // ------------------------------------------------------------ naming

    /// Give `handle` the name `name` (moving it from its current holder,
    /// which becomes `name__1`). `None` removes the name.
    pub fn set_name(&self, handle: &str, name: Option<&str>) -> Result<OutputInfo> {
        if let Some(n) = name {
            validate_name(n)?;
        }
        let mut drop_ids = Vec::new();
        let out = {
            let mut g = self.inner.lock();
            let g = &mut *g;
            if !g.by_handle.contains_key(handle) {
                return Err(EngineError::new("not_found", format!("output {handle} not found")));
            }
            // Detach from any name it currently holds.
            let old = g.by_handle.get(handle).and_then(|o| o.name.clone().or_else(|| o.version_of.as_ref().map(|v| v.0.clone())));
            if let Some(old) = old {
                if let Some(list) = g.names.get_mut(&old.to_ascii_lowercase()) {
                    list.retain(|h| h != handle);
                }
                relabel(g, &old);
            }
            if let Some(n) = name {
                let key = n.to_ascii_lowercase();
                let list = g.names.entry(key.clone()).or_default();
                list.insert(0, handle.to_string());
                while list.len() > KEEP_VERSIONS {
                    let h = list.pop().unwrap_or_default();
                    if let Some(o) = g.by_handle.get_mut(&h) {
                        o.version_of = None;
                        if !o.pinned && !o.active && o.state == OutputState::Live {
                            o.state = OutputState::Evicted;
                            drop_ids.push(o.result_id.clone());
                        }
                    }
                }
                // Canonical display case is the one just used.
                if let Some(o) = g.by_handle.get_mut(handle) {
                    o.name = Some(n.to_string());
                }
                relabel(g, n);
            } else if let Some(o) = g.by_handle.get_mut(handle) {
                o.name = None;
                o.version_of = None;
            }
            g.by_handle.get(handle).cloned().unwrap()
        };
        for id in drop_ids {
            self.results.remove(&id);
        }
        self.persist_meta(&out);
        Ok(out)
    }

    /// Drop an output: frees its data, deletes its saved snapshot (pinned or
    /// a tab's saved last result) and forgets the handle. Older versions of
    /// the same name move up (`revenue__2` becomes `revenue__1`). Returns
    /// the dropped output.
    pub fn remove(&self, reference: &str) -> Result<OutputInfo> {
        let o = self.resolve(reference).ok_or_else(|| EngineError::new("not_found", format!("output {reference} not found")))?;
        {
            let mut g = self.inner.lock();
            let g = &mut *g;
            g.by_handle.remove(&o.handle);
            g.by_result.remove(&o.result_id);
            for list in g.active.values_mut() {
                list.retain(|h| h != &o.handle);
            }
            g.active.retain(|_, v| !v.is_empty());
            if let Some(base) = o.name.clone().or_else(|| o.version_of.as_ref().map(|v| v.0.clone())) {
                let key = base.to_ascii_lowercase();
                let next = g.names.get_mut(&key).map(|list| {
                    list.retain(|h| h != &o.handle);
                    list.first().cloned()
                });
                // Dropping the current version: the previous one becomes current.
                if o.name.is_some() {
                    if let Some(Some(h)) = &next {
                        if let Some(x) = g.by_handle.get_mut(h) {
                            x.name = Some(base.clone());
                            x.version_of = None;
                        }
                    }
                }
                if g.names.get(&key).is_some_and(Vec::is_empty) {
                    g.names.remove(&key);
                } else {
                    relabel(g, &base);
                }
            }
        }
        self.results.remove(&o.result_id);
        if let Some(rec) = self.workspace.list_outputs().ok().and_then(|l| l.into_iter().find(|r| r.result_id == o.result_id)) {
            self.drop_record(&rec);
        } else if let Some(p) = self.snapshot_path(&o.result_id) {
            let _ = std::fs::remove_file(p);
        }
        // The promoted version's saved metadata (if pinned) follows.
        if let Some(n) = &o.name {
            if let Some(cur) = self.resolve(n) {
                self.persist_meta(&cur);
            }
        }
        Ok(o)
    }

    /// Drop every output that is not pinned and not a tab's latest result.
    /// Returns how many were dropped.
    pub fn remove_unpinned(&self) -> usize {
        let handles: Vec<String> = self.inner.lock().by_handle.values().filter(|o| !o.pinned && !o.active).map(|o| o.handle.clone()).collect();
        handles.iter().filter(|h| self.remove(h).is_ok()).count()
    }

    // ------------------------------------------------------------ pinning

    /// Pin/unpin. Pinning writes a Parquet snapshot when a snapshot directory
    /// is configured (blocking; call from a blocking context for big data).
    pub fn set_pinned(&self, handle: &str, pinned: bool) -> Result<OutputInfo> {
        let info = self.get(handle).ok_or_else(|| EngineError::new("not_found", format!("output {handle} not found")))?;
        if pinned && info.state == OutputState::Evicted {
            return Err(EngineError::new("invalid", format!("{handle} was freed from memory; re-run its query to pin it")));
        }
        if !pinned && info.state == OutputState::OnDisk {
            self.ensure_loaded(handle)?; // keep the data for this session
        }
        self.update(handle, |o| o.pinned = pinned);
        let info = self.get(handle).unwrap_or(info);
        if pinned {
            if let Some(path) = self.snapshot_path(&info.result_id) {
                let rs = self.results.get(&info.result_id).map_err(|e| EngineError::new("not_found", e.to_string()))?;
                let (schema, batches) = {
                    let mut g = rs.lock();
                    (g.schema(), g.view_batches(&ViewSpec::default(), SNAPSHOT_CHUNK).map_err(|e| EngineError::new("internal", e.to_string()))?)
                };
                databrain_export::write_snapshot(&path, schema, &batches).map_err(|e| EngineError::new("internal", format!("cannot save {handle}: {e}")))?;
                self.workspace.save_output(&OutputRecord {
                    result_id: info.result_id.clone(),
                    handle: info.handle.clone(),
                    name: info.name.clone(),
                    connection_id: Some(info.connection_id.clone()),
                    meta: serde_json::to_value(&info).unwrap_or_default(),
                    snapshot_path: Some(path.to_string_lossy().into_owned()),
                    created_at: info.created_at,
                })?;
            }
        } else {
            let _ = self.workspace.delete_output(&info.result_id);
            if let Some(path) = self.snapshot_path(&info.result_id) {
                let _ = std::fs::remove_file(path);
            }
            self.enforce_budget();
        }
        Ok(self.get(handle).unwrap_or(info))
    }

    fn persist_meta(&self, info: &OutputInfo) {
        if !info.pinned {
            return;
        }
        if let Ok(list) = self.workspace.list_outputs() {
            if let Some(mut rec) = list.into_iter().find(|r| r.result_id == info.result_id) {
                rec.name = info.name.clone();
                rec.meta = serde_json::to_value(info).unwrap_or_default();
                let _ = self.workspace.save_output(&rec);
            }
        }
    }

    // ------------------------------------------------------------ lookup

    fn update(&self, handle: &str, f: impl FnOnce(&mut OutputInfo)) {
        if let Some(o) = self.inner.lock().by_handle.get_mut(handle) {
            f(o);
        }
    }

    pub fn get(&self, handle: &str) -> Option<OutputInfo> {
        let g = self.inner.lock();
        let mut o = g.by_handle.get(handle).cloned()?;
        if o.state == OutputState::Live {
            o.bytes = self.results.bytes_of(&o.result_id);
        }
        Some(o)
    }

    pub fn by_result(&self, result_id: &str) -> Option<OutputInfo> {
        let h = self.inner.lock().by_result.get(result_id).cloned()?;
        self.get(&h)
    }

    /// Resolve `r12`, `R12`, `revenue`, `@revenue`, `revenue__1`,
    /// `results.revenue` or a raw result id.
    pub fn resolve(&self, reference: &str) -> Option<OutputInfo> {
        let r = reference.trim().trim_start_matches('@');
        let r = r.strip_prefix("results.").or_else(|| r.strip_prefix("RESULTS.")).unwrap_or(r).trim_matches('"');
        let g = self.inner.lock();
        let handle = if is_handle(r) {
            Some(r.to_ascii_lowercase())
        } else if let Some(h) = g.by_result.get(r) {
            Some(h.clone())
        } else {
            let (base, back) = match r.rsplit_once("__") {
                Some((b, k)) if !b.is_empty() && k.chars().all(|c| c.is_ascii_digit()) && !k.is_empty() => (b, k.parse::<usize>().unwrap_or(0)),
                _ => (r, 0),
            };
            g.names.get(&base.to_ascii_lowercase()).and_then(|l| l.get(back)).cloned()
        }?;
        drop(g);
        self.get(&handle)
    }

    /// Resolve and make sure the data is in memory (loads snapshots).
    pub fn ensure_loaded(&self, reference: &str) -> Result<OutputInfo> {
        let o = self.resolve(reference).ok_or_else(|| EngineError::new("not_found", format!("output {reference} not found")))?;
        match o.state {
            OutputState::Live => {}
            OutputState::Evicted => {
                return Err(EngineError::new("not_found", format!("{} was freed from memory to save space. Re-run its query to recreate it.", o.handle)));
            }
            OutputState::OnDisk => {
                let path = self.snapshot_path(&o.result_id).ok_or_else(|| EngineError::new("not_found", "snapshots are disabled"))?;
                let (schema, batches) = databrain_export::read_snapshot(&path).map_err(|e| EngineError::new("internal", format!("cannot read snapshot of {}: {e}", o.handle)))?;
                self.results.insert_complete(o.result_id.clone(), schema, batches, o.truncated);
                self.update(&o.handle, |x| x.state = OutputState::Live);
            }
        }
        let now = now_ms();
        self.update(&o.handle, |x| x.last_used = now);
        let o = self.get(&o.handle).unwrap_or(o);
        if o.state == OutputState::Live {
            self.enforce_budget();
        }
        Ok(o)
    }

    /// Newest first.
    pub fn list(&self) -> Vec<OutputInfo> {
        let handles: Vec<String> = self.inner.lock().by_handle.keys().cloned().collect();
        let mut v: Vec<OutputInfo> = handles.iter().filter_map(|h| self.get(h)).collect();
        v.sort_by(|a, b| b.created_at.cmp(&a.created_at).then_with(|| handle_num(&b.handle).cmp(&handle_num(&a.handle))));
        v
    }

    pub fn for_tab(&self, tab: &str) -> Vec<OutputInfo> {
        let hs = self.inner.lock().active.get(tab).cloned().unwrap_or_default();
        hs.iter().filter_map(|h| self.get(h)).collect()
    }

    /// Outputs referenced by `sql` (`results.<x>`), unknown names skipped.
    pub fn referenced(&self, sql: &str) -> Vec<OutputInfo> {
        databrain_connector_core::external::referenced_results(sql).iter().filter_map(|r| self.resolve(r)).collect()
    }

    /// Referenceable table names (handles + names + versions).
    pub fn table_names(&self) -> Vec<String> {
        let g = self.inner.lock();
        let mut v: Vec<String> = g.by_handle.values().filter(|o| o.state != OutputState::Evicted).map(|o| o.handle.clone()).collect();
        for list in g.names.values() {
            for (i, h) in list.iter().enumerate() {
                if let Some(o) = g.by_handle.get(h).filter(|o| o.state != OutputState::Evicted) {
                    let n = o.name.clone().or_else(|| o.version_of.as_ref().map(|x| x.0.clone())).unwrap_or_default();
                    v.push(if i == 0 { n } else { format!("{n}__{i}") });
                }
            }
        }
        v.sort();
        v
    }
}

fn handle_num(h: &str) -> u64 {
    h.get(1..).and_then(|n| n.parse().ok()).unwrap_or(0)
}

/// Recompute `name` / `version_of` for every handle in a name's list.
fn relabel(g: &mut Inner, name: &str) {
    let key = name.to_ascii_lowercase();
    let list = g.names.get(&key).cloned().unwrap_or_default();
    let display = list.first().and_then(|h| g.by_handle.get(h)).and_then(|o| o.name.clone()).unwrap_or_else(|| name.to_string());
    for (i, h) in list.iter().enumerate() {
        if let Some(o) = g.by_handle.get_mut(h) {
            if i == 0 {
                o.name = Some(display.clone());
                o.version_of = None;
            } else {
                o.name = None;
                o.version_of = Some((display.clone(), i));
            }
        }
    }
    if list.is_empty() {
        g.names.remove(&key);
    }
}

// ------------------------------------------------------------------ DuckDB bridge

/// Exposes outputs to DuckDB sessions as `results.<name>`.
pub struct OutputTables(pub Arc<OutputRegistry>);

impl ExternalTables for OutputTables {
    fn resolve(&self, name: &str) -> std::result::Result<ExternalTable, String> {
        let o = self.0.ensure_loaded(name).map_err(|e| e.message)?;
        let rs = self.0.results.get(&o.result_id).map_err(|e| e.to_string())?;
        // Chunks with the result's own types (a single combined batch can need
        // 64-bit offsets, which DuckDB's appender would cast back and overflow).
        let (schema, chunks) = {
            let mut g = rs.lock();
            (g.schema(), g.view_batches(&ViewSpec::default(), SNAPSHOT_CHUNK).map_err(|e| e.to_string())?)
        };
        let (schema, batches) = sql_ready_all(&schema, &chunks).map_err(|e| e.to_string())?;
        let notice = o.truncated.then(|| {
            format!(
                "results.{name} ({}) holds only the first {} rows{} — totals over it may be incomplete. Re-run its query with a higher row limit for complete data.",
                o.handle,
                o.rows,
                o.row_limit.map(|l| format!(" (row limit {l})")).unwrap_or_default()
            )
        });
        Ok(ExternalTable { name: name.to_string(), version_key: o.result_id, schema, batches, notice })
    }

    fn names(&self) -> Vec<String> {
        self.0.table_names()
    }

    fn catalog(&self) -> Vec<databrain_connector_core::external::ExternalInfo> {
        let mut out = Vec::new();
        for name in self.0.table_names() {
            let Some(o) = self.0.resolve(&name) else { continue };
            let what = if name == o.handle { "output".to_string() } else if o.name.as_deref() == Some(name.as_str()) { format!("output {}", o.handle) } else { format!("older version, {}", o.handle) };
            out.push(databrain_connector_core::external::ExternalInfo {
                columns: o.columns.iter().map(|c| (c.name.clone(), c.db_type.clone().unwrap_or_else(|| c.data_type.clone()))).collect(),
                rows: Some(o.rows as i64),
                comment: Some(format!("{what} · {} · {}{}", o.connection_name, crate::outputs::one_line(&o.sql, 120), if o.truncated { " · capped at the row limit" } else { "" })),
                name,
            });
        }
        out
    }
}

pub(crate) fn one_line(s: &str, max: usize) -> String {
    let t: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.chars().count() <= max { t } else { format!("{}…", t.chars().take(max - 1).collect::<String>()) }
}

/// Outputs as DuckDB reads them: decimals kept as text become DECIMAL
/// (precision/scale from every value of every batch) or DOUBLE.
pub fn sql_ready_all(schema: &SchemaRef, batches: &[RecordBatch]) -> std::result::Result<(SchemaRef, Vec<RecordBatch>), databrain_connector_core::arrow::error::ArrowError> {
    let mut fields = Vec::with_capacity(schema.fields().len());
    for (i, f) in schema.fields().iter().enumerate() {
        let dt = if display::is_decimal_text(f) {
            decimal_type(batches.iter().map(|b| b.column(i).as_ref()))
        } else {
            f.data_type().clone()
        };
        fields.push(Field::new(f.name(), dt, true));
    }
    let out_schema = Arc::new(Schema::new(fields));
    let mut out = Vec::with_capacity(batches.len());
    for b in batches {
        let mut cols: Vec<ArrayRef> = Vec::with_capacity(b.num_columns());
        for (i, (f, col)) in schema.fields().iter().zip(b.columns()).enumerate() {
            let target = out_schema.field(i).data_type();
            cols.push(if display::is_decimal_text(f) {
                cast(col, target)? // the type fits every value of every batch
            } else {
                col.clone()
            });
        }
        out.push(RecordBatch::try_new(out_schema.clone(), cols)?);
    }
    Ok((out_schema, out))
}

/// [`sql_ready_all`] for one batch.
pub fn sql_ready(schema: &SchemaRef, batch: &RecordBatch) -> std::result::Result<(SchemaRef, RecordBatch), databrain_connector_core::arrow::error::ArrowError> {
    let (s, mut b) = sql_ready_all(schema, std::slice::from_ref(batch))?;
    Ok((s, b.remove(0)))
}

fn decimal_type<'a>(cols: impl Iterator<Item = &'a dyn Array>) -> DataType {
    let (mut int_digits, mut scale) = (1usize, 0usize);
    for col in cols {
        let Some(s) = col.as_any().downcast_ref::<StringArray>() else { return DataType::Float64 };
        for v in s.iter().flatten() {
            let v = v.trim().trim_start_matches(['-', '+']);
            if v.contains(['e', 'E']) || v.eq_ignore_ascii_case("nan") || v.contains("inf") {
                return DataType::Float64;
            }
            let (i, f) = v.split_once('.').unwrap_or((v, ""));
            int_digits = int_digits.max(i.trim_start_matches('0').len().max(1));
            scale = scale.max(f.len());
        }
    }
    let p = int_digits + scale;
    if p > 38 { DataType::Float64 } else { DataType::Decimal128(p.max(1) as u8, scale as i8) }
}

// ------------------------------------------------------------------ diff

/// DuckDB SQL comparing two outputs. With key columns: rows `added`,
/// `removed` and `changed` (with `<col> (before)` for changed values).
/// Without keys: whole-row differences (multiset).
/// Pair the columns of two outputs as (before, after) names: user pairs
/// first (`extra`, e.g. `val_dt` ↔ `value_date`), then equal names, then
/// names equal ignoring case (`test` ↔ `Test`). Each column is used once.
pub fn column_pairs(before: &OutputInfo, after: &OutputInfo, extra: &[(String, String)]) -> Result<Vec<(String, String)>> {
    let bnames: Vec<&str> = before.columns.iter().map(|c| c.name.as_str()).collect();
    let anames: Vec<&str> = after.columns.iter().map(|c| c.name.as_str()).collect();
    let mut used_b: HashSet<String> = HashSet::new();
    let mut used_a: HashSet<String> = HashSet::new();
    let mut pairs: Vec<(String, String)> = Vec::new();
    for (bc, ac) in extra {
        if !bnames.contains(&bc.as_str()) {
            return Err(EngineError::new("invalid", format!("column {bc} is not in {}", before.handle)));
        }
        if !anames.contains(&ac.as_str()) {
            return Err(EngineError::new("invalid", format!("column {ac} is not in {}", after.handle)));
        }
        if used_b.contains(bc) || used_a.contains(ac) {
            return Err(EngineError::new("invalid", format!("column {bc} or {ac} is matched twice")));
        }
        used_b.insert(bc.clone());
        used_a.insert(ac.clone());
        pairs.push((bc.clone(), ac.clone()));
    }
    for exact in [true, false] {
        for ac in &anames {
            if used_a.contains(*ac) {
                continue;
            }
            let hit = bnames.iter().find(|bc| !used_b.contains(**bc) && if exact { *bc == ac } else { bc.eq_ignore_ascii_case(ac) });
            if let Some(bc) = hit {
                used_b.insert(bc.to_string());
                used_a.insert(ac.to_string());
                pairs.push((bc.to_string(), ac.to_string()));
            }
        }
    }
    // Keep the after output's column order.
    pairs.sort_by_key(|(_, a)| anames.iter().position(|x| x == a).unwrap_or(usize::MAX));
    Ok(pairs)
}

pub fn diff_sql(before: &OutputInfo, after: &OutputInfo, keys: &[String]) -> Result<String> {
    diff_sql_mapped(before, after, keys, &[])
}

/// Diff SQL over matched columns (see [`column_pairs`]). Before-side columns
/// are renamed to the after-side names; `keys` use after-side names (or the
/// before-side name of a pair, or any case).
pub fn diff_sql_mapped(before: &OutputInfo, after: &OutputInfo, keys: &[String], extra: &[(String, String)]) -> Result<String> {
    let q = |s: &str| q_min(s);
    let pairs = column_pairs(before, after, extra)?;
    if pairs.is_empty() {
        return Err(EngineError::new("invalid", format!("{} and {} have no matching columns; match some columns by hand", before.handle, after.handle)));
    }
    let common: Vec<&str> = pairs.iter().map(|(_, a)| a.as_str()).collect();
    let mut key_names: Vec<String> = Vec::new();
    for k in keys {
        let hit = pairs.iter().find(|(b, a)| a == k || b == k).or_else(|| pairs.iter().find(|(b, a)| a.eq_ignore_ascii_case(k) || b.eq_ignore_ascii_case(k)));
        match hit {
            Some((_, a)) => key_names.push(a.clone()),
            None => return Err(EngineError::new("invalid", format!("key column {k} is not matched in both outputs"))),
        }
    }
    let keys = &key_names;
    let (a, b) = (before.reference(), after.reference());
    let cols = common.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ");
    let a_cols = pairs.iter().map(|(bc, ac)| if bc == ac { q(ac) } else { format!("{} AS {}", q(bc), q(ac)) }).collect::<Vec<_>>().join(", ");
    let renamed: Vec<String> = pairs.iter().filter(|(bc, ac)| bc != ac).map(|(bc, ac)| format!("{bc} → {ac}")).collect();
    let mut header = format!("-- Differences from {} (before) to {} (after)\n", before.handle, after.handle);
    if !renamed.is_empty() {
        header.push_str(&format!("-- Matched columns: {}\n", renamed.join(", ")));
    }
    if keys.is_empty() {
        return Ok(format!(
            "{header}WITH a AS (SELECT {a_cols} FROM {a}), b AS (SELECT {cols} FROM {b})\n\
             SELECT 'removed' AS _change, * FROM (SELECT * FROM a EXCEPT ALL SELECT * FROM b)\n\
             UNION ALL\n\
             SELECT 'added' AS _change, * FROM (SELECT * FROM b EXCEPT ALL SELECT * FROM a)\n\
             ORDER BY ALL;"
        ));
    }
    let using = keys.iter().map(|k| q(k)).collect::<Vec<_>>().join(", ");
    let values: Vec<&str> = common.iter().copied().filter(|c| !keys.iter().any(|k| k == c)).collect();
    let key_list = keys.iter().map(|k| q(k)).collect::<Vec<_>>().join(", ");
    let changed_cols = values
        .iter()
        .map(|c| format!("b.{0} AS {0}, a.{0} AS {1}", q(c), q(&format!("{c} (before)"))))
        .collect::<Vec<_>>();
    let changed_where = if values.is_empty() {
        "false".to_string()
    } else {
        values.iter().map(|c| format!("b.{0} IS DISTINCT FROM a.{0}", q(c))).collect::<Vec<_>>().join("\n   OR ")
    };
    let changed_select = if changed_cols.is_empty() { key_list.clone() } else { format!("{key_list}, {}", changed_cols.join(", ")) };
    Ok(format!(
        "{header}WITH a AS (SELECT {a_cols} FROM {a}), b AS (SELECT {cols} FROM {b})\n\
         SELECT 'added' AS _change, * FROM b ANTI JOIN a USING ({using})\n\
         UNION ALL BY NAME\n\
         SELECT 'removed' AS _change, * FROM a ANTI JOIN b USING ({using})\n\
         UNION ALL BY NAME\n\
         SELECT 'changed' AS _change, {changed_select}\n\
         FROM b JOIN a USING ({using})\n\
         WHERE {changed_where}\n\
         ORDER BY _change, {key_list};"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use databrain_connector_core::arrow::array::Int64Array;

    fn reg() -> Arc<OutputRegistry> {
        OutputRegistry::new(Arc::new(ResultStore::new()), Arc::new(Workspace::open_in_memory().unwrap()))
    }

    fn add(r: &OutputRegistry, tab: &str, rows: i64) -> OutputInfo {
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, true)]));
        let batch = RecordBatch::try_new(schema.clone(), vec![Arc::new(Int64Array::from((0..rows).collect::<Vec<_>>()))]).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let rs = r.results.insert_complete(id, schema, vec![batch], false);
        let info = rs.lock().info();
        r.add(NewOutput {
            result: info,
            connection_id: "c".into(),
            connection_name: "C".into(),
            kind: ConnectorKind::Sqlite,
            sql: "select".into(),
            tab_id: tab.into(),
            statement_index: 0,
            row_limit: None,
            origin: Origin::User,
        })
    }

    #[test]
    fn handles_names_and_versions() {
        let r = reg();
        let a = add(&r, "t", 3);
        assert_eq!(a.handle, "r1");
        r.set_name("r1", Some("Revenue")).unwrap();
        r.begin_run("t");
        let b = add(&r, "t", 4);
        r.set_name(&b.handle, Some("revenue")).unwrap();
        assert_eq!(r.resolve("revenue").unwrap().handle, "r2");
        assert_eq!(r.resolve("@REVENUE__1").unwrap().handle, "r1");
        assert_eq!(r.resolve("results.r1").unwrap().version_of, Some(("revenue".into(), 1)));
        assert_eq!(r.get("r1").unwrap().reference(), "results.revenue__1");
        assert_eq!(r.table_names(), vec!["r1", "r2", "revenue", "revenue__1"]);
        // Removing the current name promotes the previous version.
        r.set_name("r2", None).unwrap();
        assert_eq!(r.resolve("revenue").unwrap().handle, "r1");
        assert!(validate_name("r7").is_err() && validate_name("a__b").is_err() && validate_name("9a").is_err());
        assert!(validate_name("sales_2024").is_ok());
    }

    #[test]
    fn budget_evicts_inactive_unpinned_first() {
        let r = reg();
        let a = add(&r, "t", 50_000);
        r.begin_run("t");
        let b = add(&r, "t", 50_000);
        r.set_pinned(&a.handle, false).unwrap();
        r.set_budget(1); // clamps to 16 MB: both fit
        assert!(r.get("r1").unwrap().state == OutputState::Live);
        *r.budget.lock() = b.bytes; // only room for one
        r.enforce_budget();
        assert_eq!(r.get("r1").unwrap().state, OutputState::Evicted); // inactive → freed
        assert_eq!(r.get("r2").unwrap().state, OutputState::Live); // active → kept
        assert!(r.ensure_loaded("r1").unwrap_err().message.contains("Re-run"));
        r.close_tab("t");
        assert_eq!(r.get("r2").unwrap().state, OutputState::Evicted);
    }

    #[test]
    fn pinned_snapshots_restore() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Arc::new(Workspace::open_in_memory().unwrap());
        let r = OutputRegistry::new(Arc::new(ResultStore::new()), ws.clone());
        r.set_snapshot_dir(dir.path().to_path_buf());
        let a = add(&r, "t", 10);
        r.set_name(&a.handle, Some("keep")).unwrap();
        r.set_pinned(&a.handle, true).unwrap();
        // "Restart": new registry and result store over the same workspace.
        let r2 = OutputRegistry::new(Arc::new(ResultStore::new()), ws.clone());
        r2.set_snapshot_dir(dir.path().to_path_buf());
        let o = r2.resolve("keep").unwrap();
        assert_eq!((o.handle.as_str(), o.state, o.pinned), ("r1", OutputState::OnDisk, true));
        let o = r2.ensure_loaded("keep").unwrap();
        assert_eq!(o.state, OutputState::Live);
        assert_eq!(r2.results.get(&o.result_id).unwrap().lock().total_rows(), 10);
        // Handles keep counting after a restart.
        assert_eq!(add(&r2, "t", 1).handle, "r2");
        r2.set_pinned("r1", false).unwrap();
        assert!(ws.list_outputs().unwrap().is_empty());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn remove_drops_data_snapshot_and_promotes_versions() {
        let dir = tempfile::tempdir().unwrap();
        let r = OutputRegistry::new(Arc::new(ResultStore::new()), Arc::new(Workspace::open_in_memory().unwrap()));
        r.set_snapshot_dir(dir.path().to_path_buf());
        let a = add(&r, "t", 1);
        r.set_name(&a.handle, Some("rev")).unwrap();
        r.begin_run("t");
        let b = add(&r, "t", 2);
        r.set_name(&b.handle, Some("rev")).unwrap();
        r.begin_run("t");
        let c = add(&r, "t", 3);
        r.set_name(&c.handle, Some("rev")).unwrap(); // rev=r3, rev__1=r2, rev__2=r1
        r.set_pinned(&c.handle, true).unwrap();
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);

        // Drop a middle version: the older one moves up.
        r.remove("rev__1").unwrap();
        assert_eq!(r.resolve("rev__1").unwrap().handle, a.handle);
        assert!(r.get(&b.handle).is_none() && !r.results.contains(&b.result_id));
        // Drop the current (pinned) version: snapshot deleted, previous becomes current.
        r.remove("rev").unwrap();
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        assert_eq!(r.resolve("rev").unwrap().handle, a.handle);
        assert_eq!(r.get(&a.handle).unwrap().name.as_deref(), Some("rev"));
        assert!(r.for_tab("t").is_empty(), "the tab's latest output was dropped");
        assert!(r.remove("r99").is_err());

        // Bulk: keeps pinned and the tabs' latest outputs.
        r.begin_run("u");
        let old = add(&r, "u", 1);
        r.begin_run("u");
        let latest = add(&r, "u", 1);
        let keep = add(&r, "v", 1);
        r.set_pinned(&keep.handle, true).unwrap();
        r.begin_run("v");
        assert_eq!(r.remove_unpinned(), 2, "r1 (rev) and the old run of u");
        assert!(r.get(&old.handle).is_none() && r.get(&latest.handle).is_some() && r.get(&keep.handle).is_some());
    }

    #[test]
    fn tab_outputs_survive_restart() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Arc::new(Workspace::open(dir.path().join("w.db")).unwrap());
        let tab = |id: &str| databrain_workspace::TabState { id: id.into(), title: id.into(), sql: String::new(), connection_id: None, saved_query_id: None, notebook_id: None, output_ref: None };
        ws.save_tabs(&[tab("t1"), tab("t2")]).unwrap();
        let snaps = dir.path().join("outputs");
        {
            let r = OutputRegistry::new(Arc::new(ResultStore::new()), ws.clone());
            r.set_snapshot_dir(snaps.clone());
            add(&r, "t1", 3); // r1: replaced by r2 below
            r.begin_run("t1");
            add(&r, "t1", 5); // r2: latest of t1
            add(&r, "t2", 7); // r3: latest of t2
            let p = add(&r, "gone", 2); // r4: tab closed before quit
            r.set_pinned(&p.handle, true).unwrap();
            assert_eq!(r.persist_active(), 2);
            assert_eq!(r.persist_active(), 0, "unchanged outputs are not rewritten");
        }
        // Restart; tab t2 was closed meanwhile.
        ws.save_tabs(&[tab("t1")]).unwrap();
        let store = Arc::new(ResultStore::new());
        let r = OutputRegistry::new(store, ws.clone());
        r.set_snapshot_dir(snaps.clone());
        let t1 = r.for_tab("t1");
        assert_eq!(t1.iter().map(|o| (o.handle.as_str(), o.state, o.active, o.pinned)).collect::<Vec<_>>(), vec![("r2", OutputState::OnDisk, true, false)]);
        assert!(r.for_tab("t2").is_empty());
        assert!(r.get("r4").unwrap().pinned, "pinned output kept although its tab is gone");
        assert_eq!(r.ensure_loaded("r2").unwrap().rows, 5);
        // New run of t1, quit: the old snapshot is dropped.
        r.begin_run("t1");
        add(&r, "t1", 1);
        r.persist_active();
        let files = std::fs::read_dir(&snaps).unwrap().count();
        assert_eq!(files, 2, "r4 (pinned) + new t1 output");
        assert_eq!(add(&r, "x", 1).handle, "r6", "handles keep counting after restart");
    }

    #[test]
    fn decimal_text_becomes_decimal() {
        let mut md = std::collections::HashMap::new();
        md.insert(databrain_connector_core::value::META_DB_TYPE.to_string(), "numeric".to_string());
        let schema: SchemaRef = Arc::new(Schema::new(vec![Field::new("amount", DataType::Utf8, true).with_metadata(md)]));
        let batch = RecordBatch::try_new(schema.clone(), vec![Arc::new(StringArray::from(vec![Some("12.50"), None, Some("-3")]))]).unwrap();
        let (s, b) = sql_ready(&schema, &batch).unwrap();
        assert_eq!(s.field(0).data_type(), &DataType::Decimal128(4, 2));
        assert_eq!(b.column(0).null_count(), 1);
    }

    /// Chunks of one output get one DECIMAL type fitting every chunk.
    #[test]
    fn decimal_type_spans_chunks() {
        let mut md = std::collections::HashMap::new();
        md.insert(databrain_connector_core::value::META_DB_TYPE.to_string(), "numeric".to_string());
        let schema: SchemaRef = Arc::new(Schema::new(vec![Field::new("amount", DataType::Utf8, true).with_metadata(md)]));
        let b1 = RecordBatch::try_new(schema.clone(), vec![Arc::new(StringArray::from(vec!["12.5"]))]).unwrap();
        let b2 = RecordBatch::try_new(schema.clone(), vec![Arc::new(StringArray::from(vec!["12345.125"]))]).unwrap();
        let (s, bs) = sql_ready_all(&schema, &[b1, b2]).unwrap();
        assert_eq!(s.field(0).data_type(), &DataType::Decimal128(8, 3));
        assert!(bs.iter().all(|b| b.schema() == s));
    }
}
