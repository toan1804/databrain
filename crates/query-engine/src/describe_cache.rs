//! One description of a table at a time, kept a short while: opening a
//! table from the explorer describes it twice (the tree's click loads its
//! columns, the double-click opens its tab), and each description is
//! several catalog queries (on Databricks and Snowflake, a warehouse's).
//! A second caller waits for the first and shares its answer; for
//! [`DESCRIBE_KEEP`] after, callers get it without asking the server.
//! Refresh asks again (`fresh`), and DDL run on the connection, a
//! reconnect or a disconnect forget its descriptions.

use super::*;

/// How long a description is reused.
pub const DESCRIBE_KEEP: Duration = Duration::from_secs(30);

type Key = (String, String, String);

/// A description in progress or kept, behind a lock its first caller holds
/// while it asks the server.
type Slot = Arc<tokio::sync::Mutex<Option<(Instant, ObjectDetail)>>>;

/// A combined query's part's column classes, by (connection, the part's SQL
/// with its parameters bound): pushdown asks for them only once.
type PartTypes = HashMap<(String, String), Vec<(String, crate::pushdown::TypeClass)>>;

/// Parts' column classes kept at most (an arbitrary one goes past it).
const PART_TYPES_KEEP: usize = 256;

#[derive(Default)]
pub(crate) struct DescribeCache {
    slots: Mutex<HashMap<Key, Slot>>,
    part_types: Mutex<PartTypes>,
    /// Entries past which old ones are dropped (doubles when they are all
    /// young, so the sweep isn't run on every call while indexing).
    prune_at: Mutex<usize>,
}

impl DescribeCache {
    fn slot(&self, key: Key) -> Slot {
        let mut g = self.slots.lock();
        let mut at = self.prune_at.lock();
        if g.len() > (*at).max(256) {
            // Old ones and ones never filled go; one being made stays.
            g.retain(|_, s| s.try_lock().map_or(true, |v| v.as_ref().is_some_and(|(t, _)| t.elapsed() < DESCRIBE_KEEP)));
            *at = (g.len() * 2).max(256);
        }
        g.entry(key).or_default().clone()
    }

    /// Forget a connection's descriptions (DDL ran, reconnect, disconnect).
    pub(crate) fn forget(&self, connection_id: &str) {
        self.slots.lock().retain(|(c, _, _), _| c != connection_id);
        self.part_types.lock().retain(|(c, _), _| c != connection_id);
    }

    pub(crate) fn part_types(&self, connection_id: &str, sql: &str) -> Option<Vec<(String, crate::pushdown::TypeClass)>> {
        self.part_types.lock().get(&(connection_id.to_string(), sql.to_string())).cloned()
    }

    pub(crate) fn keep_part_types(&self, connection_id: &str, sql: &str, types: Vec<(String, crate::pushdown::TypeClass)>) {
        let mut g = self.part_types.lock();
        if g.len() >= PART_TYPES_KEEP
            && let Some(k) = g.keys().next().cloned()
        {
            g.remove(&k);
        }
        g.insert((connection_id.to_string(), sql.to_string()), types);
    }
}

impl QueryEngine {
    /// [`Self::describe_fresh`], reusing a description made in the last
    /// [`DESCRIBE_KEEP`] or still being made.
    pub async fn describe(&self, connection_id: &str, schema: &str, name: &str) -> Result<ObjectDetail> {
        self.describe_with(connection_id, schema, name, false).await
    }

    /// The table described by the server now (Refresh).
    pub async fn describe_fresh(&self, connection_id: &str, schema: &str, name: &str) -> Result<ObjectDetail> {
        self.describe_with(connection_id, schema, name, true).await
    }

    async fn describe_with(&self, connection_id: &str, schema: &str, name: &str, fresh: bool) -> Result<ObjectDetail> {
        self.describe_reused(connection_id, schema, name, fresh).await.map(|(d, _)| d)
    }

    /// The description, and whether it was reused (not asked of the server
    /// by this call). Always asked anew: `fresh`, a table whose cached
    /// columns are stale (the explorer saw it change; a kept description
    /// would be the old one, and would be saved over the stale mark), and
    /// DuckDB's query outputs (`results.main`, which runs on other
    /// connections redefine).
    pub(crate) async fn describe_reused(&self, connection_id: &str, schema: &str, name: &str, fresh: bool) -> Result<(ObjectDetail, bool)> {
        // Marked stale, not merely uncached (that is any first description).
        let stale = || self.workspace.meta_table(connection_id, schema, name).ok().flatten().is_some_and(|t| t.stale);
        let fresh = fresh || schema == "results.main" || stale();
        let slot = self.describes.slot((connection_id.to_string(), schema.to_string(), name.to_string()));
        let mut held = slot.lock().await;
        if !fresh && let Some((_, d)) = held.as_ref().filter(|(at, _)| at.elapsed() < DESCRIBE_KEEP) {
            return Ok((d.clone(), true));
        }
        let (schema_s, name_s) = (schema.to_string(), name.to_string());
        let d = self
            .with_meta(connection_id, |s| {
                let (schema, name) = (schema_s.clone(), name_s.clone());
                async move { s.describe(&schema, &name).await }
            })
            .await;
        // An error isn't kept: the next caller asks again.
        *held = d.as_ref().ok().map(|d| (Instant::now(), d.clone()));
        d.map(|d| (d, false))
    }

    /// Forget the connection's table descriptions (see [`DescribeCache`]).
    pub fn forget_descriptions(&self, connection_id: &str) {
        self.describes.forget(connection_id);
    }

    pub(crate) fn kept_part_types(&self, connection_id: &str, sql: &str) -> Option<Vec<(String, crate::pushdown::TypeClass)>> {
        self.describes.part_types(connection_id, sql)
    }

    pub(crate) fn keep_part_types(&self, connection_id: &str, sql: &str, types: Vec<(String, crate::pushdown::TypeClass)>) {
        self.describes.keep_part_types(connection_id, sql, types)
    }

    /// Something run on the connection may have changed its tables (DDL,
    /// a call or block, a COMMIT, a load that creates a table): its kept
    /// descriptions go, and the explorer cache's tables read until now
    /// count as stale (saved, so a restart or a description saved later
    /// from a read before it doesn't make them current), so neither the
    /// explorer nor a table tab shows them as current. The same for the
    /// other connections to the same database (another user, a read-only
    /// one, the same file).
    pub fn tables_may_have_changed(&self, connection_id: &str) {
        let ids = self.same_database(connection_id);
        for id in &ids {
            self.forget_descriptions(id);
        }
        if let Err(e) = self.workspace.meta_tables_may_have_changed(&ids, databrain_workspace::now_ms()) {
            tracing::warn!(error = %e, "could not mark cached tables stale");
        }
    }

    /// The connection and the others that point at the same database: the
    /// same kind, server (host and port) and database, or the same file.
    fn same_database(&self, connection_id: &str) -> Vec<String> {
        let all = self.workspace.list_connections().unwrap_or_default();
        let Some(me) = all.iter().find(|p| p.id == connection_id) else { return vec![connection_id.to_string()] };
        let target = |c: &databrain_connector_core::ConnectionConfig| (c.kind, c.host.clone(), c.port, c.database.clone(), c.file_path.clone());
        let mine = target(&me.config);
        let known = mine.1.is_some() || mine.4.is_some();
        all.iter().filter(|p| p.id == connection_id || (known && target(&p.config) == mine)).map(|p| p.id.clone()).collect()
    }
}
