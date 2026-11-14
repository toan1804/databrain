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

use databrain_connector_core::external::RESULTS_SCHEMA;
use databrain_connector_core::{ConnectorKind, quote_ident};
use databrain_result_store::{ColumnMeta, ResultInfo, ResultStore, ViewSpec};
use databrain_workspace::{Origin, OutputRecord, Workspace, now_ms};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::{EngineError, Result};

/// Versions kept per name (current + previous ones).
/// Rows per batch when an output is saved or read by SQL.
pub(super) const SNAPSHOT_CHUNK: usize = 65_536;
const KEEP_VERSIONS: usize = 5;
/// Metadata entries kept for evicted outputs.
const MAX_ENTRIES: usize = 400;
/// Budget when the computer's memory is unknown.
const DEFAULT_BUDGET: usize = 1024 * 1024 * 1024;
const MB: u64 = 1024 * 1024;
const GB: u64 = 1024 * MB;

/// The budget for a computer with `total` bytes of memory: a quarter of it,
/// between 512 MB and 16 GB, so results don't push other programs into swap
/// on a small machine and do use a big one (1 GB when unknown).
pub fn auto_budget(total: Option<u64>) -> usize {
    match total {
        Some(t) if t > 0 => (t / 4).clamp(512 * MB, 16 * GB) as usize,
        _ => DEFAULT_BUDGET,
    }
}

/// Free memory a result leaves the computer: a tenth of it, at least 500 MB.
pub fn free_floor(total: Option<u64>) -> u64 {
    total.map_or(0, |t| t / 10).max(500 * MB)
}

/// `1.5 GB`, `512 MB`.
/// Why a result on disk can't be pinned.
pub fn too_large_to_pin(o: &OutputInfo) -> String {
    format!(
        "Too large to pin: {} ({} rows) is larger than memory, so its rows are on disk, and pinning saves a copy of every row. Export it to keep it, or pin a smaller result (aggregate or filter it with SQL on {}).",
        o.handle,
        o.rows,
        o.reference()
    )
}

pub fn size_text(bytes: usize) -> String {
    let b = bytes as u64;
    if b >= GB {
        let g = b as f64 / GB as f64;
        if (g - g.round()).abs() < 0.05 { format!("{} GB", g.round()) } else { format!("{g:.1} GB") }
    } else {
        format!("{} MB", b.div_ceil(MB))
    }
}

/// Reads the computer's free (available) memory in bytes; None when unknown.
pub type FreeMemoryProbe = Arc<dyn Fn() -> Option<u64> + Send + Sync>;

/// How often the free memory is read while results load.
const FREE_CHECK_EVERY: std::time::Duration = std::time::Duration::from_millis(200);

struct FreeCheck {
    probe: FreeMemoryProbe,
    floor: u64,
    /// When it was last read, and what it read.
    last: Option<(std::time::Instant, Option<u64>)>,
}
/// Largest active output saved on quit (in-memory size).
pub const AUTO_MAX_BYTES: usize = 64 * 1024 * 1024;
/// Total size of active outputs saved on quit.
pub const AUTO_TOTAL_BYTES: usize = 512 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum OutputState {
    /// Data in memory.
    Live,
    /// Pinned snapshot on disk, loaded on first use.
    OnDisk,
    /// Data freed; re-run the SQL to get it back.
    Evicted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
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
    #[ts(as = "f64")]
    pub created_at: i64,
    pub rows: usize,
    pub columns: Vec<ColumnMeta>,
    /// Cut at the row limit: more rows exist at the source.
    pub truncated: bool,
    pub row_limit: Option<usize>,
    pub bytes: usize,
    pub pinned: bool,
    /// Larger than memory: its rows are on disk (a live result), so it
    /// can't be pinned.
    #[serde(default)]
    pub spilled: bool,
    pub state: OutputState,
    pub origin: Origin,
    #[ts(as = "f64")]
    pub last_used: i64,
    /// Latest run of its tab, or kept from an earlier one (shown in the UI).
    #[serde(default)]
    pub active: bool,
    /// Kept in its tab's results when the tab runs again (the user's
    /// "Keep"): it stays active, so it isn't freed while it is shown.
    #[serde(default)]
    pub kept: bool,
    /// Database session the statement ran on, when not `tab_id` (a
    /// notebook's cells share `nb:{notebook}`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub session_key: String,
}

impl OutputInfo {
    /// The session the statement ran on.
    pub fn session(&self) -> &str {
        if self.session_key.is_empty() { &self.tab_id } else { &self.session_key }
    }
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
    const RESERVED: &[&str] = &[
        "select", "from", "where", "group", "order", "by", "table", "join", "union", "all", "as", "on", "and", "or", "not", "null", "limit", "case", "when",
        "end", "desc", "asc",
    ];
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
    /// Session it ran on (empty: `tab_id`).
    pub session_key: String,
}

#[derive(Default)]
struct Inner {
    by_handle: HashMap<String, OutputInfo>,
    by_result: HashMap<String, String>,
    /// lowercase name → handles, newest first.
    names: HashMap<String, Vec<String>>,
    /// tab → handles of its latest run, and kept ones from earlier runs.
    active: HashMap<String, Vec<String>>,
    /// Kept outputs carried over from an earlier run of their tab: letting
    /// go of one makes it inactive at once (one of the latest run stays
    /// active until the tab runs again).
    carried: HashSet<String>,
    seq: u64,
}

pub struct OutputRegistry {
    inner: Mutex<Inner>,
    pub(super) results: Arc<ResultStore>,
    workspace: Arc<Workspace>,
    snapshot_dir: Mutex<Option<PathBuf>>,
    budget: Mutex<usize>,
    free: Mutex<Option<FreeCheck>>,
    disk: Mutex<Option<FreeCheck>>,
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
        Arc::new(Self {
            inner: Mutex::new(Inner { seq, ..Default::default() }),
            results,
            workspace,
            snapshot_dir: Mutex::new(None),
            budget: Mutex::new(DEFAULT_BUDGET),
            free: Mutex::new(None),
            disk: Mutex::new(None),
        })
    }

    /// The memory outputs may use; one result never keeps more.
    pub fn budget(&self) -> usize {
        *self.budget.lock()
    }

    pub fn set_budget(&self, bytes: usize) {
        *self.budget.lock() = bytes.max(16 * 1024 * 1024);
        self.enforce_budget();
    }

    /// Bytes the outputs in memory use now.
    pub fn used(&self) -> usize {
        self.inner.lock().by_handle.values().filter(|o| o.state == OutputState::Live).map(|o| o.bytes).sum()
    }

    /// Stop loading a result when the computer's free memory (read by
    /// `probe`) drops below `floor` bytes; None turns the check off.
    pub fn set_free_memory_probe(&self, probe: Option<FreeMemoryProbe>, floor: u64) {
        *self.free.lock() = probe.map(|probe| FreeCheck { probe, floor, last: None });
    }

    /// Stop writing a result to disk when the free space where results
    /// spill (read by `probe`) drops below `floor` bytes; None: no check.
    pub fn set_free_disk_probe(&self, probe: Option<FreeMemoryProbe>, floor: u64) {
        *self.disk.lock() = probe.map(|probe| FreeCheck { probe, floor, last: None });
    }

    /// Free disk space where results spill, when below the floor.
    pub fn low_disk(&self) -> Option<u64> {
        Self::check(&self.disk)
    }

    /// The computer's free memory when it is below the floor (read at most
    /// every 200 ms; between reads the last answer holds).
    pub fn low_memory(&self) -> Option<u64> {
        Self::check(&self.free)
    }

    fn check(slot: &Mutex<Option<FreeCheck>>) -> Option<u64> {
        Self::read(slot, 1)
    }

    /// The free amount when it's below `floor / divisor` (read at most
    /// every 200 ms; between reads the last reading holds).
    fn read(slot: &Mutex<Option<FreeCheck>>, divisor: u64) -> Option<u64> {
        let mut g = slot.lock();
        let c = g.as_mut()?;
        let free = match c.last {
            Some((at, free)) if at.elapsed() < FREE_CHECK_EVERY => free,
            _ => {
                let free = (c.probe)();
                c.last = Some((std::time::Instant::now(), free));
                free
            }
        };
        let floor = c.floor / divisor.max(1);
        free.filter(|f| *f < floor)
    }

    /// The computer's free memory when it is below half the floor: a result
    /// already on disk holds little memory (a file's rows and a few pages),
    /// so it stops only then.
    pub fn critically_low_memory(&self) -> Option<u64> {
        Self::read(&self.free, 2)
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
                // Shown apart from the tab's last results after a restart.
                if info.kept {
                    g.carried.insert(info.handle.clone());
                }
            } else {
                info.kept = false;
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

    /// A new run of `tab` starts: its previous outputs become recent, except
    /// kept ones, which stay active (and shown) next to the new results.
    pub fn begin_run(&self, tab: &str) {
        let mut g = self.inner.lock();
        let g = &mut *g;
        let Some(prev) = g.active.remove(tab) else { return };
        let mut kept = Vec::new();
        for h in prev {
            if let Some(o) = g.by_handle.get_mut(&h) {
                if o.kept && o.state != OutputState::Evicted {
                    g.carried.insert(h.clone());
                    kept.push(h);
                } else {
                    o.active = false;
                }
            }
        }
        if !kept.is_empty() {
            g.active.insert(tab.to_string(), kept);
        }
    }

    /// Keep an output in its tab's results across runs, or let go of it.
    /// A kept output is active: never freed for the memory budget, and
    /// saved with the tab's results on quit.
    pub fn set_kept(&self, handle: &str, kept: bool) -> Result<OutputInfo> {
        {
            let mut g = self.inner.lock();
            let g = &mut *g;
            let o = g.by_handle.get_mut(handle).ok_or_else(|| EngineError::new("not_found", format!("output {handle} not found")))?;
            if kept && (o.state == OutputState::Evicted || !o.active) {
                return Err(EngineError::new("invalid", format!("{handle} isn't one of its tab's results any more; run its query again to keep it")));
            }
            o.kept = kept;
            if !kept && g.carried.remove(handle) {
                o.active = false;
                let tab = o.tab_id.clone();
                if let Some(list) = g.active.get_mut(&tab) {
                    list.retain(|h| h != handle);
                }
                g.active.retain(|_, v| !v.is_empty());
            }
        }
        if !kept {
            self.enforce_budget();
        }
        self.get(handle).ok_or_else(|| EngineError::new("not_found", format!("output {handle} not found")))
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
                spilled: n.result.on_disk,
                kept: false,
                state: OutputState::Live,
                origin: n.origin,
                last_used: now,
                active: true,
                session_key: if n.session_key == n.tab_id { String::new() } else { n.session_key },
            };
            g.by_result.insert(info.result_id.clone(), handle.clone());
            g.active.entry(n.tab_id).or_default().push(handle.clone());
            g.by_handle.insert(handle, info.clone());
            let seq = g.seq;
            drop(g);
            let _ = crate::blocking(|| self.workspace.set_setting("outputs_seq", &serde_json::json!(seq)));
            info
        };
        self.enforce_budget();
        info
    }

    /// Tab closed: free its unnamed, unpinned outputs.
    pub fn close_tab(&self, tab: &str) {
        let freed: Vec<String> = {
            let mut g = self.inner.lock();
            let g = &mut *g;
            g.active.remove(tab);
            let mut freed = Vec::new();
            for o in g.by_handle.values_mut() {
                if o.tab_id == tab {
                    o.active = false;
                    o.kept = false;
                    g.carried.remove(&o.handle);
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
                let mut cands: Vec<(i64, String)> =
                    g.by_handle.values().filter(|o| o.state == OutputState::Live && !o.pinned && !o.active).map(|o| (o.last_used, o.handle.clone())).collect();
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
        let mut evicted: Vec<(i64, String)> =
            g.by_handle.values().filter(|o| o.state == OutputState::Evicted).map(|o| (o.created_at, o.handle.clone())).collect();
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
            let tab = self.inner.lock().by_handle.get(handle).map(|o| o.tab_id.clone()).unwrap_or_default();
            self.check_owner(n, &tab)?;
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

    /// Names of job steps belong to them: only runs of that step (run key
    /// `tab`) may produce `results.<name>`.
    pub fn check_owner(&self, name: &str, tab: &str) -> Result<()> {
        match self.workspace.job_step_owner(name) {
            Ok(Some(owner)) if owner.tab() != tab => Err(EngineError::new(
                "invalid",
                format!("results.{name} is the output of {}; only that step can produce it. Choose another name.", owner.describe()),
            )),
            _ => Ok(()),
        }
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
            g.carried.remove(&o.handle);
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
                if o.name.is_some()
                    && let Some(Some(h)) = &next
                    && let Some(x) = g.by_handle.get_mut(h)
                {
                    x.name = Some(base.clone());
                    x.version_of = None;
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
        if let Some(n) = &o.name
            && let Some(cur) = self.resolve(n)
        {
            self.persist_meta(&cur);
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
        if pinned && info.pinned {
            return Ok(info); // already saved: nothing to write (or to undo)
        }
        if pinned && info.spilled {
            return Err(EngineError::new("invalid", too_large_to_pin(&info)));
        }
        if info.state == OutputState::OnDisk {
            // Unpinning: keep the data for this session. Pinning a restored
            // result: its rows are on disk only; read them to save again.
            self.ensure_loaded(handle)?;
        }
        self.update(handle, |o| o.pinned = pinned);
        let info = self.get(handle).unwrap_or(info);
        if pinned {
            if let Some(path) = self.snapshot_path(&info.result_id) {
                // A file there before (a restored result's) is not ours to
                // delete if the save fails.
                let existed = path.exists();
                let save = || -> Result<()> {
                    let rs = self.results.get(&info.result_id).map_err(|e| EngineError::new("not_found", e.to_string()))?;
                    if rs.lock().is_on_disk() {
                        return Err(EngineError::new("invalid", too_large_to_pin(&info)));
                    }
                    let (schema, batches) = {
                        let mut g = rs.lock();
                        (g.schema(), g.view_batches(&ViewSpec::default(), SNAPSHOT_CHUNK).map_err(|e| EngineError::new("internal", e.to_string()))?)
                    };
                    databrain_export::write_snapshot(&path, schema, &batches)
                        .map_err(|e| EngineError::new("internal", format!("cannot save {handle}: {e}")))?;
                    self.workspace.save_output(&OutputRecord {
                        result_id: info.result_id.clone(),
                        handle: info.handle.clone(),
                        name: info.name.clone(),
                        connection_id: Some(info.connection_id.clone()),
                        meta: serde_json::to_value(&info).unwrap_or_default(),
                        snapshot_path: Some(path.to_string_lossy().into_owned()),
                        created_at: info.created_at,
                    })?;
                    Ok(())
                };
                // Not saved: not pinned either (it would be lost on restart
                // while the list says it's kept).
                if let Err(e) = save() {
                    self.update(handle, |o| o.pinned = false);
                    if !existed {
                        let _ = std::fs::remove_file(&path);
                    }
                    return Err(e);
                }
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
        if let Ok(list) = self.workspace.list_outputs()
            && let Some(mut rec) = list.into_iter().find(|r| r.result_id == info.result_id)
        {
            rec.name = info.name.clone();
            rec.meta = serde_json::to_value(info).unwrap_or_default();
            let _ = self.workspace.save_output(&rec);
        }
    }

    // ------------------------------------------------------------ lookup

    fn update(&self, handle: &str, f: impl FnOnce(&mut OutputInfo)) {
        if let Some(o) = self.inner.lock().by_handle.get_mut(handle) {
            f(o);
        }
    }

    pub fn get(&self, handle: &str) -> Option<OutputInfo> {
        // The registry isn't held while the result's size is read: a long
        // export or sort of it holds the result's lock.
        let mut o = self.inner.lock().by_handle.get(handle).cloned()?;
        if o.state == OutputState::Live {
            (o.bytes, o.spilled) = self.results.size_of(&o.result_id);
        }
        Some(o)
    }

    /// Free a result: its output too when it has one (so no output is
    /// listed whose rows are gone). Only this registry removes results that
    /// may have an output; other crates call this, not `ResultStore::remove`.
    /// A pinned output keeps its saved copy: only its rows in memory go
    /// (it loads again from disk on use).
    pub fn release_result(&self, result_id: &str) -> bool {
        match self.by_result(result_id) {
            Some(o) if o.pinned && self.snapshot_path(result_id).is_some_and(|p| p.exists()) => {
                self.update(&o.handle, |x| x.state = OutputState::OnDisk);
                self.results.remove(result_id)
            }
            Some(o) => self.remove(&o.handle).is_ok(),
            None => self.results.remove(result_id),
        }
    }

    /// A deleted history entry's result: its output goes too (rows and saved
    /// copy), unless it's pinned or shown in a tab; a result without an
    /// output just frees its rows. Whether anything went.
    pub fn drop_history_result(&self, result_id: &str) -> bool {
        match self.by_result(result_id) {
            Some(o) if !o.pinned && !o.active => self.remove(&o.handle).is_ok(),
            Some(_) => false,
            None => self.results.remove(result_id),
        }
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
                let (schema, batches) =
                    databrain_export::read_snapshot(&path).map_err(|e| EngineError::new("internal", format!("cannot read snapshot of {}: {e}", o.handle)))?;
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use databrain_connector_core::arrow::array::Int64Array;
    use databrain_connector_core::arrow::array::RecordBatch;
    use databrain_connector_core::arrow::datatypes::{DataType, Field, Schema};

    pub(crate) fn reg() -> Arc<OutputRegistry> {
        OutputRegistry::new(Arc::new(ResultStore::new()), Arc::new(Workspace::open_in_memory().unwrap()))
    }

    pub(crate) fn add(r: &OutputRegistry, tab: &str, rows: i64) -> OutputInfo {
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
            session_key: String::new(),
        })
    }

    /// A kept result stays in its tab's results across runs (active, so
    /// never freed for the budget) until let go; others drop off as before.
    #[test]
    fn kept_outputs_stay_in_their_tab_across_runs() {
        let r = reg();
        r.begin_run("t");
        let keep = add(&r, "t", 3);
        let other = add(&r, "t", 2);
        assert!(r.set_kept(&keep.handle, true).unwrap().kept);
        r.begin_run("t");
        let new = add(&r, "t", 1);
        let shown = |r: &OutputRegistry| r.for_tab("t").into_iter().map(|o| o.handle).collect::<Vec<_>>();
        assert_eq!(shown(&r), [keep.handle.clone(), new.handle.clone()]);
        assert!(r.get(&keep.handle).unwrap().active);
        assert!(!r.get(&other.handle).unwrap().active, "not kept: recent, as before");
        // Still kept after another run.
        r.begin_run("t");
        assert_eq!(shown(&r), std::slice::from_ref(&keep.handle));
        // A result that isn't shown any more can't be kept.
        assert!(r.set_kept(&other.handle, true).is_err());
        // Letting go of a carried one: inactive at once, so the budget may free it.
        let o = r.set_kept(&keep.handle, false).unwrap();
        assert!(!o.kept && !o.active);
        assert!(shown(&r).is_empty());
        // Letting go of one from the latest run: shown until the next run.
        r.begin_run("t");
        let latest = add(&r, "t", 1);
        r.set_kept(&latest.handle, true).unwrap();
        assert!(r.set_kept(&latest.handle, false).unwrap().active);
        r.begin_run("t");
        assert!(!r.get(&latest.handle).unwrap().active);
        // Closing the tab frees kept ones too.
        let k = add(&r, "t", 1);
        r.set_kept(&k.handle, true).unwrap();
        r.close_tab("t");
        assert_eq!(r.get(&k.handle).unwrap().state, OutputState::Evicted);
    }

    /// A deleted history entry's result goes with its output, unless the
    /// output is pinned or shown in a tab; one without an output just frees.
    #[test]
    fn history_results_go_through_the_registry() {
        let r = reg();
        let shown = add(&r, "t1", 1);
        assert!(!r.drop_history_result(&shown.result_id), "shown in its tab: kept");
        r.begin_run("t1");
        let pinned = add(&r, "t1", 1);
        r.update(&pinned.handle, |o| o.pinned = true);
        r.begin_run("t1");
        assert!(r.get(&shown.handle).is_some_and(|o| !o.active));
        assert!(r.drop_history_result(&shown.result_id), "older and not pinned: goes");
        assert!(r.get(&shown.handle).is_none() && !r.results.contains(&shown.result_id));
        assert!(!r.drop_history_result(&pinned.result_id), "pinned: kept");
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, true)]));
        r.results.insert_complete("plain", schema, vec![], false);
        assert!(r.drop_history_result("plain") && !r.results.contains("plain"));
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
        // Pinning again writes nothing and keeps the saved copy.
        r2.set_pinned("r1", true).unwrap();
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        // Freeing a pinned result frees its memory only: it loads again.
        assert!(r2.release_result(&o.result_id));
        assert_eq!(r2.get("r1").unwrap().state, OutputState::OnDisk);
        assert_eq!(r2.ensure_loaded("r1").unwrap().state, OutputState::Live);
        assert_eq!(ws.list_outputs().unwrap().len(), 1);
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
        let tab = |id: &str| databrain_workspace::TabState {
            id: id.into(),
            title: id.into(),
            sql: String::new(),
            connection_id: None,
            saved_query_id: None,
            notebook_id: None,
            output_ref: None,
            auto_commit: None,
            file_path: None,
        };
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
        // Pinning a restored result before it is read: read, then saved.
        let pinned = r.set_pinned("r2", true).unwrap();
        assert!(pinned.pinned && r.snapshot_path(&pinned.result_id).unwrap().exists(), "{pinned:?}");
        r.set_pinned("r2", false).unwrap();
        assert_eq!(r.ensure_loaded("r2").unwrap().rows, 5);
        // New run of t1, quit: the old snapshot is dropped.
        r.begin_run("t1");
        add(&r, "t1", 1);
        r.persist_active();
        let files = std::fs::read_dir(&snaps).unwrap().count();
        assert_eq!(files, 2, "r4 (pinned) + new t1 output");
        assert_eq!(add(&r, "x", 1).handle, "r6", "handles keep counting after restart");
    }
}
