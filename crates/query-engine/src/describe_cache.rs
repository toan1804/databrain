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

#[derive(Default)]
pub(crate) struct DescribeCache {
    slots: Mutex<HashMap<Key, Slot>>,
    /// When DataBrain last ran something on a connection that may have
    /// changed its tables (ms): a cached description saved before that
    /// isn't current, even if its stale mark was written over meanwhile.
    changed_at: Mutex<HashMap<String, i64>>,
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

    /// Something run on the connection may have changed its tables (DDL,
    /// a call or block, a load that creates a table): its kept
    /// descriptions go, and the explorer cache's tables of it count as
    /// stale, so neither the explorer nor a table tab shows them as current.
    pub fn tables_may_have_changed(&self, connection_id: &str) {
        self.forget_descriptions(connection_id);
        self.describes.changed_at.lock().insert(connection_id.to_string(), databrain_workspace::now_ms());
        if let Err(e) = self.workspace.meta_columns_may_have_changed(connection_id) {
            tracing::warn!(error = %e, "could not mark cached tables stale");
        }
    }

    /// When [`Self::tables_may_have_changed`] last ran for the connection
    /// in this process (ms), if it did.
    pub fn tables_changed_at(&self, connection_id: &str) -> Option<i64> {
        self.describes.changed_at.lock().get(connection_id).copied()
    }
}
