//! Metadata cache: every table list, search result and column list the app
//! fetches (explorer, ⌘P, completion) is kept here, so completion and search
//! can answer locally at once — without building the AI knowledge index and
//! across restarts. Full schema listings replace that schema's rows; searches
//! only add rows.

use databrain_connector_core::{ColumnInfo, DbObject, ObjectKind};
use rusqlite::{OptionalExtension, params};

use crate::{Result, Workspace, now_ms};

fn kind_str(k: ObjectKind) -> String {
    serde_json::to_value(k).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_else(|| "table".into())
}

impl Workspace {
    /// A complete listing of `schema` (explorer): replaces its cached objects.
    pub fn meta_put_schema(&self, connection_id: &str, schema: &str, objects: &[DbObject]) -> Result<()> {
        let mut c = self.conn.lock();
        let tx = c.transaction()?;
        tx.execute("DELETE FROM meta_objects WHERE connection_id = ?1 AND schema_name = ?2", params![connection_id, schema])?;
        Self::meta_insert(&tx, connection_id, objects)?;
        tx.commit()?;
        Ok(())
    }

    /// Objects seen in a search (partial): added or refreshed.
    pub fn meta_add_objects(&self, connection_id: &str, objects: &[DbObject]) -> Result<()> {
        if objects.is_empty() {
            return Ok(());
        }
        let mut c = self.conn.lock();
        let tx = c.transaction()?;
        Self::meta_insert(&tx, connection_id, objects)?;
        tx.commit()?;
        Ok(())
    }

    fn meta_insert(tx: &rusqlite::Transaction<'_>, connection_id: &str, objects: &[DbObject]) -> Result<()> {
        let now = now_ms();
        let mut st = tx.prepare_cached(
            "INSERT INTO meta_objects (connection_id, schema_name, name, kind, comment, row_estimate, seen_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT (connection_id, schema_name, name) DO UPDATE SET kind = excluded.kind, comment = excluded.comment, \
             row_estimate = coalesce(excluded.row_estimate, row_estimate), seen_at = excluded.seen_at",
        )?;
        for o in objects.iter().filter(|o| o.kind.is_relation()) {
            st.execute(params![connection_id, o.schema, o.name, kind_str(o.kind), o.comment, o.row_estimate, now])?;
        }
        Ok(())
    }

    /// Columns of a table (from a describe).
    pub fn meta_put_columns(&self, connection_id: &str, schema: &str, table: &str, columns: &[ColumnInfo]) -> Result<()> {
        let c = self.conn.lock();
        c.execute(
            "INSERT INTO meta_columns (connection_id, schema_name, table_name, columns_json, seen_at) VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT (connection_id, schema_name, table_name) DO UPDATE SET columns_json = excluded.columns_json, seen_at = excluded.seen_at",
            params![connection_id, schema, table, serde_json::to_string(columns).unwrap_or_else(|_| "[]".into()), now_ms()],
        )?;
        Ok(())
    }

    /// Cached column names of a table (exact schema id, any-case name).
    pub fn meta_column_names(&self, connection_id: &str, schema: &str, table: &str) -> Result<Option<Vec<String>>> {
        let c = self.conn.lock();
        let json: Option<String> = c
            .query_row(
                "SELECT columns_json FROM meta_columns WHERE connection_id = ?1 AND schema_name = ?2 AND lower(table_name) = lower(?3)",
                params![connection_id, schema, table],
                |r| r.get(0),
            )
            .optional()?;
        Ok(json.map(|j| serde_json::from_str::<Vec<ColumnInfo>>(&j).unwrap_or_default().into_iter().map(|c| c.name).collect()))
    }

    /// Number of cached objects of a connection.
    pub fn meta_count(&self, connection_id: &str) -> Result<i64> {
        Ok(self.conn.lock().query_row("SELECT count(*) FROM meta_objects WHERE connection_id = ?1", [connection_id], |r| r.get(0))?)
    }

    pub fn meta_clear(&self, connection_id: &str) -> Result<()> {
        let c = self.conn.lock();
        c.execute("DELETE FROM meta_objects WHERE connection_id = ?1", [connection_id])?;
        c.execute("DELETE FROM meta_columns WHERE connection_id = ?1", [connection_id])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(schema: &str, name: &str, kind: ObjectKind) -> DbObject {
        DbObject { schema: schema.into(), name: name.into(), kind, comment: None, row_estimate: None }
    }

    #[test]
    fn listings_replace_searches_add() {
        let ws = Workspace::open_in_memory().unwrap();
        ws.meta_put_schema("c", "s", &[o("s", "orders", ObjectKind::Table), o("s", "old", ObjectKind::Table), o("s", "f", ObjectKind::Function)]).unwrap();
        assert_eq!(ws.meta_count("c").unwrap(), 2, "functions are not cached");
        ws.meta_add_objects("c", &[o("t", "orders_archive", ObjectKind::View)]).unwrap();
        // A new listing of `s` drops tables that are gone, keeps other schemas.
        ws.meta_put_schema("c", "s", &[o("s", "orders", ObjectKind::Table)]).unwrap();
        let names: Vec<String> = ws.kn_complete("c", None, "ord", 10).unwrap().into_iter().map(|r| format!("{}.{} {}", r.0, r.1, r.2)).collect();
        assert_eq!(names, vec!["s.orders table", "t.orders_archive view"]);
        assert!(ws.kn_complete("c", None, "old", 10).unwrap().is_empty());

        let cols = vec![ColumnInfo { name: "id".into(), data_type: "int".into(), nullable: false, is_primary_key: true, default: None, comment: None }];
        ws.meta_put_columns("c", "s", "orders", &cols).unwrap();
        assert_eq!(ws.kn_column_names("c", "s", "ORDERS").unwrap().unwrap(), vec!["id"]);
        ws.delete_connection_cache_for_test("c");
        assert_eq!(ws.meta_count("c").unwrap(), 0);
        assert!(ws.kn_column_names("c", "s", "orders").unwrap().is_none());
    }

    impl Workspace {
        fn delete_connection_cache_for_test(&self, id: &str) {
            self.meta_clear(id).unwrap();
        }
    }
}
