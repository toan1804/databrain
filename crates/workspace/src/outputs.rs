//! Persisted query outputs: pinned outputs survive restarts (their data is a
//! Parquet snapshot at `snapshot_path`); `meta` is the engine's output info.

use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::{Result, Workspace};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputRecord {
    pub result_id: String,
    pub handle: String,
    pub name: Option<String>,
    pub connection_id: Option<String>,
    pub meta: serde_json::Value,
    pub snapshot_path: Option<String>,
    pub created_at: i64,
}

impl Workspace {
    pub fn save_output(&self, o: &OutputRecord) -> Result<()> {
        self.conn.lock().execute(
            "INSERT INTO outputs (result_id, handle, name, connection_id, meta_json, snapshot_path, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(result_id) DO UPDATE SET handle = excluded.handle, name = excluded.name,
               connection_id = excluded.connection_id, meta_json = excluded.meta_json, snapshot_path = excluded.snapshot_path",
            params![o.result_id, o.handle, o.name, o.connection_id, o.meta.to_string(), o.snapshot_path, o.created_at],
        )?;
        Ok(())
    }

    pub fn delete_output(&self, result_id: &str) -> Result<()> {
        self.conn.lock().execute("DELETE FROM outputs WHERE result_id = ?1", [result_id])?;
        Ok(())
    }

    pub fn list_outputs(&self) -> Result<Vec<OutputRecord>> {
        let c = self.conn.lock();
        let mut stmt = c.prepare("SELECT result_id, handle, name, connection_id, meta_json, snapshot_path, created_at FROM outputs ORDER BY created_at")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(OutputRecord {
                    result_id: r.get(0)?,
                    handle: r.get(1)?,
                    name: r.get(2)?,
                    connection_id: r.get(3)?,
                    meta: serde_json::from_str(&r.get::<_, String>(4)?).unwrap_or_default(),
                    snapshot_path: r.get(5)?,
                    created_at: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_records_roundtrip() {
        let ws = Workspace::open_in_memory().unwrap();
        let mut o = OutputRecord {
            result_id: "job:0".into(),
            handle: "r1".into(),
            name: Some("revenue".into()),
            connection_id: None,
            meta: serde_json::json!({"rows": 3}),
            snapshot_path: Some("/tmp/x.parquet".into()),
            created_at: 5,
        };
        ws.save_output(&o).unwrap();
        o.name = None;
        ws.save_output(&o).unwrap();
        let l = ws.list_outputs().unwrap();
        assert_eq!(l, vec![o.clone()]);
        ws.delete_output("job:0").unwrap();
        assert!(ws.list_outputs().unwrap().is_empty());
    }
}
