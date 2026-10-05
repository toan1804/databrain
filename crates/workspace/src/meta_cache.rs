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
        // Tables/views and routines (completion); sequences and others are not needed.
        for o in objects.iter().filter(|o| o.kind.is_relation() || o.kind.is_routine()) {
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

    /// A table/view seen while browsing (explorer, ⌘P, completion), by
    /// `schema.name` or bare `name` (case-insensitive; the schema may be the
    /// last part of a `catalog.schema` id). Returns (schema, name).
    pub fn meta_get(&self, connection_id: &str, reference: &str) -> Result<Option<(String, String)>> {
        let (schema, name) = match reference.rsplit_once('.') {
            Some((s, n)) => (Some(s.replace(['"', '`', '[', ']'], "")), n),
            None => (None, reference),
        };
        let name = name.trim_matches(|c| c == '"' || c == '`' || c == '[' || c == ']');
        Ok(self
            .conn
            .lock()
            .query_row(
                "SELECT schema_name, name FROM meta_objects \
                 WHERE connection_id = ?1 AND lower(name) = lower(?2) AND kind NOT IN ('function', 'procedure', 'package', 'sequence', 'other') \
                   AND (?3 IS NULL OR lower(schema_name) = lower(?3) OR lower(schema_name) LIKE '%.' || lower(?3)) \
                 ORDER BY length(schema_name) LIMIT 1",
                params![connection_id, name, schema],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    /// `[catalog.]schema.table[.column]` or `table[.column]` from the cache.
    /// `Ok(None)` when the table, or a column that was asked for, is not
    /// cached (columns are cached only for tables that were opened).
    pub fn meta_resolve_path(&self, connection_id: &str, path: &str) -> Result<Option<String>> {
        if let Some((s, n)) = self.meta_get(connection_id, path)? {
            return Ok(Some(format!("{s}.{n}")));
        }
        let Some((table, col)) = path.rsplit_once('.') else { return Ok(None) };
        let Some((s, n)) = self.meta_get(connection_id, table)? else { return Ok(None) };
        let cols = self.meta_column_names(connection_id, &s, &n)?.unwrap_or_default();
        Ok(cols.into_iter().find(|c| c.eq_ignore_ascii_case(col)).map(|c| format!("{s}.{n}.{c}")))
    }

    /// Cached functions/procedures/packages whose name contains `query`, in
    /// `schema` when given; prefix matches and short names first.
    pub fn meta_complete_routines(&self, connection_id: &str, schema: Option<&str>, query: &str, limit: usize) -> Result<Vec<DbObject>> {
        let q = query.trim().to_lowercase().replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        let c = self.conn.lock();
        let mut st = c.prepare_cached(
            "SELECT schema_name, name, kind, comment FROM meta_objects \
             WHERE connection_id = ?1 AND kind IN ('function', 'procedure', 'package') AND (?2 IS NULL OR lower(schema_name) = lower(?2)) \
               AND lower(name) LIKE '%' || ?3 || '%' ESCAPE '\\' \
             ORDER BY lower(name) NOT LIKE ?3 || '%' ESCAPE '\\', length(name), name LIMIT ?4",
        )?;
        let rows = st
            .query_map(params![connection_id, schema, q, limit as i64], |r| {
                let kind: String = r.get(2)?;
                Ok(DbObject {
                    schema: r.get(0)?,
                    name: r.get(1)?,
                    kind: serde_json::from_value(serde_json::Value::String(kind)).unwrap_or(ObjectKind::Function),
                    comment: r.get(3)?,
                    row_estimate: None,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
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
        assert_eq!(ws.meta_count("c").unwrap(), 3, "tables and routines are cached");
        // Routines are for routine completion only, never tables.
        assert_eq!(ws.meta_complete_routines("c", None, "", 10).unwrap().iter().map(|o| o.name.as_str()).collect::<Vec<_>>(), vec!["f"]);
        ws.meta_add_objects("c", &[o("t", "orders_archive", ObjectKind::View)]).unwrap();
        // A new listing of `s` drops tables that are gone, keeps other schemas.
        ws.meta_put_schema("c", "s", &[o("s", "orders", ObjectKind::Table)]).unwrap();
        let names: Vec<String> = ws.kn_complete("c", None, "ord", 10).unwrap().into_iter().map(|r| format!("{}.{} {}", r.0, r.1, r.2)).collect();
        assert_eq!(names, vec!["s.orders table", "t.orders_archive view"]);
        assert!(ws.kn_complete("c", None, "old", 10).unwrap().is_empty());

        let cols = vec![ColumnInfo { name: "id".into(), data_type: "int".into(), nullable: false, is_primary_key: true, default: None, comment: None }];
        ws.meta_put_columns("c", "s", "orders", &cols).unwrap();
        assert_eq!(ws.kn_column_names("c", "s", "ORDERS").unwrap().unwrap(), vec!["id"]);
        // Note targets: table, schema.table, catalog.schema ids, cached columns.
        assert_eq!(ws.meta_resolve_path("c", "ORDERS").unwrap().as_deref(), Some("s.orders"));
        assert_eq!(ws.meta_resolve_path("c", "s.orders.ID").unwrap().as_deref(), Some("s.orders.id"));
        assert!(ws.meta_resolve_path("c", "s.orders.nope").unwrap().is_none());
        assert!(ws.meta_resolve_path("c", "x.orders").unwrap().is_none());
        assert!(ws.meta_resolve_path("c", "f").unwrap().is_none(), "functions are not tables");
        ws.meta_add_objects("c", &[o("main.sales", "items", ObjectKind::Table)]).unwrap();
        assert_eq!(ws.meta_resolve_path("c", "sales.items").unwrap().as_deref(), Some("main.sales.items"));
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
