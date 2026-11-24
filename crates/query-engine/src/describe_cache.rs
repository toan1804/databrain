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
}

impl DescribeCache {
    fn slot(&self, key: Key) -> Slot {
        let mut g = self.slots.lock();
        // Ended or old ones go (a session that described many tables).
        if g.len() > 256 {
            g.retain(|_, s| s.try_lock().map_or(true, |v| v.as_ref().is_some_and(|(t, _)| t.elapsed() < DESCRIBE_KEEP)));
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
        let slot = self.describes.slot((connection_id.to_string(), schema.to_string(), name.to_string()));
        let mut held = slot.lock().await;
        if !fresh && let Some((_, d)) = held.as_ref().filter(|(at, _)| at.elapsed() < DESCRIBE_KEEP) {
            return Ok(d.clone());
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
        d
    }

    /// Forget the connection's table descriptions (see [`DescribeCache`]).
    pub fn forget_descriptions(&self, connection_id: &str) {
        self.describes.forget(connection_id);
    }
}
