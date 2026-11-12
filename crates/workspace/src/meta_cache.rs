//! Metadata cache: every catalog/schema list, table list, search result and
//! column list the app fetches (explorer, ⌘P, completion) is kept here, so
//! the explorer shows at once (and offline), and completion and search answer
//! locally — across restarts, without the AI knowledge index.
//!
//! - Complete listings (`listed_at`) replace a schema's rows; searches only add.
//! - Object versions ([`Session::object_versions`]) let a changed schema be
//!   refreshed table by table: [`Workspace::meta_apply_changes`].
//! - Columns of tables whose version changed are marked stale (still shown,
//!   fetched again when opened).
//!
//! [`Session::object_versions`]: databrain_connector_core::Session::object_versions

use std::collections::{HashMap, HashSet};

use databrain_connector_core::{CatalogInfo, ColumnInfo, ConnectionConfig, DbObject, ForeignKey, ObjectKind, SchemaInfo, version_key};
use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use crate::{Result, Workspace, now_ms};

fn kind_str(k: ObjectKind) -> String {
    k.as_str().to_string()
}

fn parse_kind(s: &str) -> ObjectKind {
    serde_json::from_value(serde_json::Value::String(s.to_string())).unwrap_or(ObjectKind::Other)
}

/// A complete cached listing of a schema.
#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct CachedListing {
    pub objects: Vec<DbObject>,
    /// When it was listed (ms).
    #[ts(as = "f64")]
    pub listed_at: i64,
}

/// What DataBrain knows of a table without asking the database: the newer
/// of the explorer cache and the AI index ([`Workspace::meta_table`]).
#[derive(Debug, Clone, PartialEq)]
pub struct CachedTable {
    /// `table`, `view`… when known (the index, or the explorer's listing).
    pub kind: Option<String>,
    pub columns: Vec<ColumnInfo>,
    /// `None`: not known (cached before foreign keys were kept, or from a
    /// source without them); `Some(empty)`: the table has none.
    pub foreign_keys: Option<Vec<ForeignKey>>,
    /// The table changed since (or may have): use it, but read it again.
    pub stale: bool,
}

/// Cached columns of a table; `stale` = its definition changed since (or
/// may have): show them, but describe again when online.
#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct CachedColumns {
    pub columns: Vec<ColumnInfo>,
    pub stale: bool,
}

/// What a connection's explorer cache holds.
#[derive(Debug, Clone, Default, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct ExplorerCacheState {
    /// Last time the cache was checked against the server (ms).
    #[ts(as = "Option<f64>")]
    pub checked_at: Option<i64>,
    #[ts(as = "Option<f64>")]
    pub catalogs_at: Option<i64>,
    #[ts(as = "Option<f64>")]
    pub schemas_at: Option<i64>,
    #[ts(as = "f64")]
    pub listed_schemas: i64,
}

/// Which server a connection points at (kind, address, database, user,
/// options, SSH hop). A change makes the cached explorer meaningless.
fn identity(cfg: &ConnectionConfig) -> String {
    // Session handling (timeouts) doesn't change what the connection points at.
    let options: std::collections::BTreeMap<&String, &String> =
        cfg.options.iter().filter(|(k, _)| !databrain_connector_core::SESSION_OPTIONS.contains(&k.as_str())).collect();
    let mut v = serde_json::json!({
        "kind": cfg.kind, "host": cfg.host, "port": cfg.port, "database": cfg.database,
        "file": cfg.file_path, "auth": cfg.auth, "options": options,
    });
    if let Some(ssh) = &cfg.ssh {
        v["ssh"] = serde_json::json!([ssh.host, ssh.port, ssh.user]);
    }
    v.to_string()
}

/// Columns cached in a schema the explorer never listed in full (seen by
/// completion, lineage, grid edits or search) are never checked by a schema
/// refresh: after this long they count as stale and are read again when
/// shown online.
pub const UNLISTED_COLUMNS_TTL_MS: i64 = 24 * 60 * 60 * 1000;

/// SQL: `m` (a `meta_columns` row) may be out of date.
/// `?4` is the cut-off: `now_ms() - UNLISTED_COLUMNS_TTL_MS`.
const STALE_SQL: &str = "(m.stale OR (m.seen_at < ?4 AND NOT EXISTS (SELECT 1 FROM meta_schemas s WHERE s.connection_id = m.connection_id \
     AND s.schema_name = m.schema_name AND s.listed_at IS NOT NULL)))";

impl Workspace {
    /// A complete listing of `schema` (explorer): replaces its cached objects.
    pub fn meta_put_schema(&self, connection_id: &str, schema: &str, objects: &[DbObject]) -> Result<()> {
        self.meta_put_listing(connection_id, schema, objects, None, None)
    }

    /// A complete listing with the objects' versions (when the engine has
    /// them) and the schema fingerprint it was read at (when known). Columns
    /// of objects whose version changed, or of every object when versions
    /// are unknown and the fingerprint is not the same, become stale; columns
    /// of objects that are gone are dropped.
    pub fn meta_put_listing(
        &self,
        connection_id: &str,
        schema: &str,
        objects: &[DbObject],
        versions: Option<&HashMap<String, String>>,
        fingerprint: Option<&str>,
    ) -> Result<()> {
        let mut c = self.conn.lock();
        let tx = c.transaction()?;
        let old_fp: Option<String> = tx
            .query_row("SELECT listed_fp FROM meta_schemas WHERE connection_id = ?1 AND schema_name = ?2", params![connection_id, schema], |r| r.get(0))
            .optional()?
            .flatten();
        let old_versions = Self::versions_tx(&tx, connection_id, schema)?;
        tx.execute("DELETE FROM meta_objects WHERE connection_id = ?1 AND schema_name = ?2", params![connection_id, schema])?;
        Self::meta_insert(&tx, connection_id, objects, versions)?;
        let names: HashSet<&str> = objects.iter().map(|o| o.name.as_str()).collect();
        let same_fp = fingerprint.is_some() && old_fp.as_deref() == fingerprint;
        let changed: Vec<&str> = match versions {
            _ if same_fp => vec![],
            Some(v) if !old_versions.is_empty() => objects
                .iter()
                .filter(|o| {
                    let k = version_key(o.kind, &o.name);
                    old_versions.get(&k) != v.get(&k)
                })
                .map(|o| o.name.as_str())
                .collect(),
            _ => objects.iter().map(|o| o.name.as_str()).collect(),
        };
        Self::columns_changed(&tx, connection_id, schema, &changed, &names)?;
        Self::mark_listed(&tx, connection_id, schema, fingerprint)?;
        Self::ai_refresh_tx(&tx, connection_id, Some(schema))?;
        tx.commit()?;
        Ok(())
    }

    /// Incremental refresh of a listed schema: `upserts` are the new and
    /// changed objects (complete rows), `removed` the `kind:name` keys that
    /// are gone, `versions` every object's version now.
    pub fn meta_apply_changes(
        &self,
        connection_id: &str,
        schema: &str,
        upserts: &[DbObject],
        removed: &[String],
        versions: &HashMap<String, String>,
        fingerprint: Option<&str>,
    ) -> Result<()> {
        let mut c = self.conn.lock();
        let tx = c.transaction()?;
        {
            let mut del = tx.prepare_cached("DELETE FROM meta_objects WHERE connection_id = ?1 AND schema_name = ?2 AND kind = ?3 AND name = ?4")?;
            for k in removed.iter().chain(upserts.iter().map(|o| version_key(o.kind, &o.name)).collect::<Vec<_>>().iter()) {
                if let Some((kind, name)) = k.split_once(':') {
                    del.execute(params![connection_id, schema, kind, name])?;
                }
            }
        }
        Self::meta_insert(&tx, connection_id, upserts, Some(versions))?;
        // Versions of untouched objects stay; refresh them anyway (cheap) in
        // case a search hit had added one without a version.
        {
            let mut up = tx.prepare_cached("UPDATE meta_objects SET version = ?5 WHERE connection_id = ?1 AND schema_name = ?2 AND kind = ?3 AND name = ?4")?;
            for (k, v) in versions {
                if let Some((kind, name)) = k.split_once(':') {
                    up.execute(params![connection_id, schema, kind, name, v])?;
                }
            }
        }
        let names: HashSet<String> = {
            let mut st = tx.prepare_cached("SELECT name FROM meta_objects WHERE connection_id = ?1 AND schema_name = ?2")?;
            st.query_map(params![connection_id, schema], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<_>>()?
        };
        let changed: Vec<&str> = upserts.iter().map(|o| o.name.as_str()).collect();
        Self::columns_changed(&tx, connection_id, schema, &changed, &names.iter().map(String::as_str).collect())?;
        Self::mark_listed(&tx, connection_id, schema, fingerprint)?;
        Self::ai_refresh_tx(&tx, connection_id, Some(schema))?;
        tx.commit()?;
        Ok(())
    }

    /// Mark columns of `changed` tables stale; drop columns of tables not in `present`.
    fn columns_changed(tx: &rusqlite::Transaction<'_>, connection_id: &str, schema: &str, changed: &[&str], present: &HashSet<&str>) -> Result<()> {
        let mut stale = tx.prepare_cached("UPDATE meta_columns SET stale = 1 WHERE connection_id = ?1 AND schema_name = ?2 AND table_name = ?3")?;
        for n in changed {
            stale.execute(params![connection_id, schema, n])?;
        }
        let cached: Vec<String> = {
            let mut st = tx.prepare_cached("SELECT table_name FROM meta_columns WHERE connection_id = ?1 AND schema_name = ?2")?;
            st.query_map(params![connection_id, schema], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        let mut del = tx.prepare_cached("DELETE FROM meta_columns WHERE connection_id = ?1 AND schema_name = ?2 AND table_name = ?3")?;
        for t in cached.iter().filter(|t| !present.contains(t.as_str())) {
            del.execute(params![connection_id, schema, t])?;
        }
        Ok(())
    }

    fn mark_listed(tx: &rusqlite::Transaction<'_>, connection_id: &str, schema: &str, fingerprint: Option<&str>) -> Result<()> {
        // A schema seen only through its objects (search) is added without a
        // catalog list position; listing it does not make its catalog complete.
        tx.execute(
            "INSERT INTO meta_schemas (connection_id, schema_name, catalog, is_default, pos, listed_at, listed_fp) VALUES (?1, ?2, NULL, 0, 1000000000, ?3, ?4) \
             ON CONFLICT (connection_id, schema_name) DO UPDATE SET listed_at = excluded.listed_at, listed_fp = excluded.listed_fp",
            params![connection_id, schema, now_ms(), fingerprint],
        )?;
        Ok(())
    }

    /// Objects seen in a search (partial): added or refreshed.
    pub fn meta_add_objects(&self, connection_id: &str, objects: &[DbObject]) -> Result<()> {
        if objects.is_empty() {
            return Ok(());
        }
        let mut c = self.conn.lock();
        let tx = c.transaction()?;
        Self::meta_insert(&tx, connection_id, objects, None)?;
        let schemas: HashSet<&str> = objects.iter().map(|o| o.schema.as_str()).collect();
        for s in schemas {
            Self::ai_refresh_tx(&tx, connection_id, Some(s))?;
        }
        tx.commit()?;
        Ok(())
    }

    fn meta_insert(tx: &rusqlite::Transaction<'_>, connection_id: &str, objects: &[DbObject], versions: Option<&HashMap<String, String>>) -> Result<()> {
        let now = now_ms();
        let mut st = tx.prepare_cached(
            "INSERT INTO meta_objects (connection_id, schema_name, name, kind, comment, row_estimate, seen_at, version) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
             ON CONFLICT (connection_id, schema_name, kind, name) DO UPDATE SET comment = excluded.comment, \
             row_estimate = coalesce(excluded.row_estimate, row_estimate), seen_at = excluded.seen_at, version = coalesce(excluded.version, version)",
        )?;
        for o in objects {
            let v = versions.and_then(|m| m.get(&version_key(o.kind, &o.name)));
            st.execute(params![connection_id, o.schema, o.name, kind_str(o.kind), o.comment, o.row_estimate, now, v])?;
        }
        Ok(())
    }

    fn versions_tx(c: &rusqlite::Connection, connection_id: &str, schema: &str) -> Result<HashMap<String, String>> {
        let mut st = c.prepare_cached("SELECT kind, name, version FROM meta_objects WHERE connection_id = ?1 AND schema_name = ?2 AND version IS NOT NULL")?;
        let rows = st
            .query_map(params![connection_id, schema], |r| Ok((format!("{}:{}", r.get::<_, String>(0)?, r.get::<_, String>(1)?), r.get::<_, String>(2)?)))?
            .collect::<rusqlite::Result<HashMap<_, _>>>()?;
        Ok(rows)
    }

    /// Cached `kind:name` → version of a schema's objects (those that have one).
    pub fn meta_versions(&self, connection_id: &str, schema: &str) -> Result<HashMap<String, String>> {
        Self::versions_tx(&self.conn.lock(), connection_id, schema)
    }

    /// The complete cached listing of `schema`, or `None` if it was never listed.
    pub fn meta_listing(&self, connection_id: &str, schema: &str) -> Result<Option<CachedListing>> {
        let c = self.conn.lock();
        let listed_at: Option<i64> = c
            .query_row("SELECT listed_at FROM meta_schemas WHERE connection_id = ?1 AND schema_name = ?2", params![connection_id, schema], |r| r.get(0))
            .optional()?
            .flatten();
        let Some(listed_at) = listed_at else { return Ok(None) };
        let mut st =
            c.prepare_cached("SELECT name, kind, comment, row_estimate FROM meta_objects WHERE connection_id = ?1 AND schema_name = ?2 ORDER BY name")?;
        let objects = st
            .query_map(params![connection_id, schema], |r| {
                Ok(DbObject {
                    schema: schema.to_string(),
                    name: r.get(0)?,
                    kind: parse_kind(&r.get::<_, String>(1)?),
                    comment: r.get(2)?,
                    row_estimate: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Some(CachedListing { objects, listed_at }))
    }

    /// Listed schemas and the fingerprint each was listed at (if known).
    pub fn meta_listed_schemas(&self, connection_id: &str) -> Result<Vec<(String, Option<String>)>> {
        let c = self.conn.lock();
        let mut st =
            c.prepare_cached("SELECT schema_name, listed_fp FROM meta_schemas WHERE connection_id = ?1 AND listed_at IS NOT NULL ORDER BY pos, schema_name")?;
        let rows = st.query_map([connection_id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The schema is unchanged at `fingerprint`: remember it (and when it was checked).
    pub fn meta_set_listed_fp(&self, connection_id: &str, schema: &str, fingerprint: &str) -> Result<()> {
        self.conn.lock().execute(
            "UPDATE meta_schemas SET listed_fp = ?3 WHERE connection_id = ?1 AND schema_name = ?2 AND listed_at IS NOT NULL",
            params![connection_id, schema, fingerprint],
        )?;
        Ok(())
    }

    /// Forget a schema's listing (it is gone).
    pub fn meta_drop_schema(&self, connection_id: &str, schema: &str) -> Result<()> {
        let mut c = self.conn.lock();
        let tx = c.transaction()?;
        Self::drop_schema_tx(&tx, connection_id, schema)?;
        tx.commit()?;
        Ok(())
    }

    fn drop_schema_tx(tx: &rusqlite::Transaction<'_>, connection_id: &str, schema: &str) -> Result<()> {
        for t in ["meta_objects", "meta_columns", "meta_schemas"] {
            tx.execute(&format!("DELETE FROM {t} WHERE connection_id = ?1 AND schema_name = ?2"), params![connection_id, schema])?;
        }
        // Nothing left there for the AI either.
        Self::ai_refresh_tx(tx, connection_id, Some(schema))?;
        Ok(())
    }

    // ------------------------------------------------------------ catalogs / schemas

    /// Cached catalogs (three-level engines), `None` if never listed.
    pub fn meta_catalogs(&self, connection_id: &str) -> Result<Option<Vec<CatalogInfo>>> {
        let c = self.conn.lock();
        let at: Option<i64> =
            c.query_row("SELECT catalogs_at FROM meta_conn_state WHERE connection_id = ?1", [connection_id], |r| r.get(0)).optional()?.flatten();
        if at.is_none() {
            return Ok(None);
        }
        let mut st = c.prepare_cached("SELECT name, is_default FROM meta_catalogs WHERE connection_id = ?1 ORDER BY pos")?;
        let rows = st.query_map([connection_id], |r| Ok(CatalogInfo { name: r.get(0)?, is_default: r.get(1)? }))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Some(rows))
    }

    /// A complete catalog list: replaces the cached one; schemas (and their
    /// objects) of catalogs that are gone are dropped.
    pub fn meta_put_catalogs(&self, connection_id: &str, catalogs: &[CatalogInfo]) -> Result<()> {
        let mut c = self.conn.lock();
        let tx = c.transaction()?;
        let keep: HashSet<&str> = catalogs.iter().map(|c| c.name.as_str()).collect();
        let old: Vec<String> = {
            let mut st = tx.prepare_cached("SELECT name FROM meta_catalogs WHERE connection_id = ?1")?;
            st.query_map([connection_id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        for gone in old.iter().filter(|n| !keep.contains(n.as_str())) {
            let schemas: Vec<String> = {
                let mut st = tx.prepare_cached("SELECT schema_name FROM meta_schemas WHERE connection_id = ?1 AND catalog = ?2")?;
                st.query_map(params![connection_id, gone], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
            };
            for s in schemas {
                Self::drop_schema_tx(&tx, connection_id, &s)?;
            }
            tx.execute("DELETE FROM meta_catalogs WHERE connection_id = ?1 AND name = ?2", params![connection_id, gone])?;
        }
        {
            let mut st = tx.prepare_cached(
                "INSERT INTO meta_catalogs (connection_id, name, is_default, pos) VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT (connection_id, name) DO UPDATE SET is_default = excluded.is_default, pos = excluded.pos",
            )?;
            for (i, cat) in catalogs.iter().enumerate() {
                st.execute(params![connection_id, cat.name, cat.is_default, i as i64])?;
            }
        }
        Self::state_set(&tx, connection_id, "catalogs_at")?;
        tx.commit()?;
        Ok(())
    }

    /// Cached schemas: every schema (`catalog = None`) or one catalog's.
    /// `None` when that list was never listed completely.
    pub fn meta_schemas(&self, connection_id: &str, catalog: Option<&str>) -> Result<Option<Vec<SchemaInfo>>> {
        let c = self.conn.lock();
        let at: Option<i64> = match catalog {
            None => c.query_row("SELECT schemas_at FROM meta_conn_state WHERE connection_id = ?1", [connection_id], |r| r.get(0)).optional()?.flatten(),
            Some(cat) => c
                .query_row("SELECT schemas_at FROM meta_catalogs WHERE connection_id = ?1 AND name = ?2", params![connection_id, cat], |r| r.get(0))
                .optional()?
                .flatten(),
        };
        if at.is_none() {
            return Ok(None);
        }
        let mut st = c.prepare_cached(
            "SELECT schema_name, is_default, catalog FROM meta_schemas WHERE connection_id = ?1 AND pos < 1000000000 \
             AND (?2 IS NULL OR catalog = ?2) ORDER BY pos",
        )?;
        let rows = st
            .query_map(params![connection_id, catalog], |r| Ok(SchemaInfo { name: r.get(0)?, is_default: r.get(1)?, catalog: r.get(2)? }))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Some(rows))
    }

    /// A complete schema list (all, or one catalog's): replaces it; schemas
    /// that are gone lose their cached objects and columns. Listings of the
    /// schemas that stay are kept.
    pub fn meta_put_schemas(&self, connection_id: &str, catalog: Option<&str>, schemas: &[SchemaInfo]) -> Result<()> {
        let mut c = self.conn.lock();
        let tx = c.transaction()?;
        let keep: HashSet<&str> = schemas.iter().map(|s| s.name.as_str()).collect();
        let old: Vec<String> = {
            let mut st =
                tx.prepare_cached("SELECT schema_name FROM meta_schemas WHERE connection_id = ?1 AND pos < 1000000000 AND (?2 IS NULL OR catalog = ?2)")?;
            st.query_map(params![connection_id, catalog], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        for gone in old.iter().filter(|n| !keep.contains(n.as_str())) {
            Self::drop_schema_tx(&tx, connection_id, gone)?;
        }
        {
            let mut st = tx.prepare_cached(
                "INSERT INTO meta_schemas (connection_id, schema_name, catalog, is_default, pos) VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT (connection_id, schema_name) DO UPDATE SET catalog = excluded.catalog, is_default = excluded.is_default, pos = excluded.pos",
            )?;
            for (i, s) in schemas.iter().enumerate() {
                st.execute(params![connection_id, s.name, s.catalog, s.is_default, i as i64])?;
            }
        }
        let now = now_ms();
        match catalog {
            None => {
                Self::state_set(&tx, connection_id, "schemas_at")?;
                // Every catalog's list is complete too.
                let cats: HashSet<&str> = schemas.iter().filter_map(|s| s.catalog.as_deref()).collect();
                for cat in cats {
                    tx.execute("UPDATE meta_catalogs SET schemas_at = ?3 WHERE connection_id = ?1 AND name = ?2", params![connection_id, cat, now])?;
                }
            }
            Some(cat) => {
                tx.execute(
                    "INSERT INTO meta_catalogs (connection_id, name, is_default, pos, schemas_at) VALUES (?1, ?2, 0, 1000000000, ?3) \
                     ON CONFLICT (connection_id, name) DO UPDATE SET schemas_at = excluded.schemas_at",
                    params![connection_id, cat, now],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Catalogs whose schema list is cached.
    pub fn meta_listed_catalogs(&self, connection_id: &str) -> Result<Vec<String>> {
        let c = self.conn.lock();
        let mut st = c.prepare_cached("SELECT name FROM meta_catalogs WHERE connection_id = ?1 AND schemas_at IS NOT NULL ORDER BY pos")?;
        let rows = st.query_map([connection_id], |r| r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn state_set(tx: &rusqlite::Transaction<'_>, connection_id: &str, column: &str) -> Result<()> {
        tx.execute(
            &format!(
                "INSERT INTO meta_conn_state (connection_id, {column}) VALUES (?1, ?2) ON CONFLICT (connection_id) DO UPDATE SET {column} = excluded.{column}"
            ),
            params![connection_id, now_ms()],
        )?;
        Ok(())
    }

    /// The cache was just checked against the server.
    pub fn meta_touch_checked(&self, connection_id: &str) -> Result<()> {
        let mut c = self.conn.lock();
        let tx = c.transaction()?;
        Self::state_set(&tx, connection_id, "checked_at")?;
        tx.commit()?;
        Ok(())
    }

    pub fn meta_state(&self, connection_id: &str) -> Result<ExplorerCacheState> {
        let c = self.conn.lock();
        let mut s: ExplorerCacheState = c
            .query_row("SELECT checked_at, catalogs_at, schemas_at FROM meta_conn_state WHERE connection_id = ?1", [connection_id], |r| {
                Ok(ExplorerCacheState { checked_at: r.get(0)?, catalogs_at: r.get(1)?, schemas_at: r.get(2)?, listed_schemas: 0 })
            })
            .optional()?
            .unwrap_or_default();
        s.listed_schemas =
            c.query_row("SELECT count(*) FROM meta_schemas WHERE connection_id = ?1 AND listed_at IS NOT NULL", [connection_id], |r| r.get(0))?;
        Ok(s)
    }

    /// Clear the cache when the connection now points at another server/user,
    /// and the AI's index of its objects with it (they describe the old
    /// server; notes and learned joins are the user's and stay).
    pub fn meta_check_identity(&self, connection_id: &str, cfg: &ConnectionConfig) -> Result<()> {
        let id = identity(cfg);
        let old: Option<Option<String>> =
            self.conn.lock().query_row("SELECT identity FROM meta_conn_state WHERE connection_id = ?1", [connection_id], |r| r.get(0)).optional()?;
        match old.flatten() {
            Some(o) if o == id => return Ok(()),
            // Cached before identities were recorded: assume it is the same server.
            None => {}
            Some(_) => {
                self.meta_clear(connection_id)?;
                self.kn_clear(connection_id)?;
            }
        }
        self.conn.lock().execute(
            "INSERT INTO meta_conn_state (connection_id, identity) VALUES (?1, ?2) ON CONFLICT (connection_id) DO UPDATE SET identity = excluded.identity",
            params![connection_id, id],
        )?;
        Ok(())
    }

    /// Columns of a table (from a describe).
    pub fn meta_put_columns(&self, connection_id: &str, schema: &str, table: &str, columns: &[ColumnInfo]) -> Result<()> {
        self.meta_put_table(connection_id, schema, table, columns, None)
    }

    /// A table as described by the server: columns, and its foreign keys
    /// when the caller has them (`None` records them as not known).
    pub fn meta_put_table(&self, connection_id: &str, schema: &str, table: &str, columns: &[ColumnInfo], foreign_keys: Option<&[ForeignKey]>) -> Result<()> {
        let fks = foreign_keys.map(|f| serde_json::to_string(f).unwrap_or_else(|_| "[]".into()));
        let mut c = self.conn.lock();
        let tx = c.transaction()?;
        tx.execute(
            "INSERT INTO meta_columns (connection_id, schema_name, table_name, columns_json, seen_at, fks_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT (connection_id, schema_name, table_name) DO UPDATE SET columns_json = excluded.columns_json, seen_at = excluded.seen_at, \
             fks_json = excluded.fks_json, stale = 0",
            params![connection_id, schema, table, serde_json::to_string(columns).unwrap_or_else(|_| "[]".into()), now_ms(), fks],
        )?;
        // The AI's search row of the table, when it may use it.
        Self::ai_refresh_table_tx(&tx, connection_id, schema, table)?;
        tx.commit()?;
        Ok(())
    }

    /// A table as DataBrain knows it without asking the server (exact schema
    /// id, any-case name): the explorer's cached describe or the AI index,
    /// whichever was read from the server last. Foreign keys the newer one
    /// doesn't know come from the other. `stale` when the explorer saw the
    /// table's schema change since (the index can't tell).
    pub fn meta_table(&self, connection_id: &str, schema: &str, name: &str) -> Result<Option<CachedTable>> {
        type Seen = (String, Option<String>, bool, i64, Option<String>);
        let c = self.conn.lock();
        let seen: Option<Seen> = c
            .query_row(
                &format!(
                    "SELECT m.columns_json, m.fks_json, {STALE_SQL}, m.seen_at, \
                       (SELECT o.kind FROM meta_objects o WHERE o.connection_id = m.connection_id AND o.schema_name = m.schema_name \
                          AND o.name = m.table_name ORDER BY o.kind = 'table' DESC LIMIT 1) \
                     FROM meta_columns m WHERE m.connection_id = ?1 AND m.schema_name = ?2 AND lower(m.table_name) = lower(?3)"
                ),
                params![connection_id, schema, name, now_ms() - UNLISTED_COLUMNS_TTL_MS],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?;
        let indexed: Option<(String, String, String, i64)> = c
            .query_row(
                "SELECT kind, columns_json, fks_json, indexed_at FROM kn_objects WHERE connection_id = ?1 AND schema_name = ?2 AND lower(name) = lower(?3)",
                params![connection_id, schema, name],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        drop(c);
        let cols = |j: &str| serde_json::from_str::<Vec<ColumnInfo>>(j).unwrap_or_default();
        let fks = |j: &str| serde_json::from_str::<Vec<ForeignKey>>(j).ok();
        Ok(match (seen, indexed) {
            (None, None) => None,
            (Some((c, f, stale, _, kind)), None) => Some(CachedTable { kind, columns: cols(&c), foreign_keys: f.as_deref().and_then(fks), stale }),
            (None, Some((kind, c, f, _))) => Some(CachedTable { kind: Some(kind), columns: cols(&c), foreign_keys: fks(&f), stale: false }),
            (Some((mc, mf, stale, seen_at, mkind)), Some((kind, kc, kf, indexed_at))) => {
                let (mine, theirs) = (mf.as_deref().and_then(fks), fks(&kf));
                Some(if seen_at >= indexed_at {
                    CachedTable { kind: mkind.or(Some(kind)), columns: cols(&mc), foreign_keys: mine.or(theirs), stale }
                } else {
                    CachedTable { kind: Some(kind), columns: cols(&kc), foreign_keys: theirs.or(mine), stale }
                })
            }
        })
    }

    /// Cached columns of a table (exact schema and name).
    pub fn meta_columns(&self, connection_id: &str, schema: &str, table: &str) -> Result<Option<CachedColumns>> {
        let c = self.conn.lock();
        let row: Option<(String, bool)> = c
            .query_row(
                &format!("SELECT m.columns_json, {STALE_SQL} FROM meta_columns m WHERE m.connection_id = ?1 AND m.schema_name = ?2 AND m.table_name = ?3"),
                params![connection_id, schema, table, now_ms() - UNLISTED_COLUMNS_TTL_MS],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(row.map(|(j, stale)| CachedColumns { columns: serde_json::from_str(&j).unwrap_or_default(), stale }))
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

    /// A cached object of `schema` named `name` (any case): tables and views
    /// before routines, the exact-case name first, a table before a view.
    pub fn meta_object(&self, connection_id: &str, schema: &str, name: &str) -> Result<Option<DbObject>> {
        Ok(self
            .conn
            .lock()
            .query_row(
                "SELECT schema_name, name, kind, comment, row_estimate FROM meta_objects \
                 WHERE connection_id = ?1 AND lower(schema_name) = lower(?2) AND lower(name) = lower(?3) \
                 ORDER BY kind IN ('function', 'procedure', 'package', 'sequence', 'other'), name <> ?3, kind <> 'table' LIMIT 1",
                params![connection_id, schema, name],
                |r| Ok(DbObject { schema: r.get(0)?, name: r.get(1)?, kind: parse_kind(&r.get::<_, String>(2)?), comment: r.get(3)?, row_estimate: r.get(4)? }),
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
        c.execute("DELETE FROM meta_schemas WHERE connection_id = ?1", [connection_id])?;
        c.execute("DELETE FROM meta_catalogs WHERE connection_id = ?1", [connection_id])?;
        c.execute("UPDATE meta_conn_state SET catalogs_at = NULL, schemas_at = NULL, checked_at = NULL WHERE connection_id = ?1", [connection_id])?;
        // The tables the AI could use are gone with them (its scope stays).
        c.execute("DELETE FROM ai_tables WHERE connection_id = ?1", [connection_id])?;
        c.execute("DELETE FROM kn_fts WHERE connection_id = ?1 AND source = 'object'", [connection_id])?;
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

    fn sch(name: &str, cat: Option<&str>) -> SchemaInfo {
        SchemaInfo { name: name.into(), is_default: false, catalog: cat.map(str::to_string) }
    }

    fn cols(names: &[&str]) -> Vec<ColumnInfo> {
        names
            .iter()
            .map(|n| ColumnInfo { name: (*n).into(), data_type: "int".into(), nullable: true, is_primary_key: false, default: None, comment: None })
            .collect()
    }

    #[test]
    fn catalogs_and_schemas_lists() {
        let ws = Workspace::open_in_memory().unwrap();
        assert!(ws.meta_catalogs("c").unwrap().is_none(), "never listed");
        ws.meta_put_catalogs("c", &[CatalogInfo { name: "main".into(), is_default: true }, CatalogInfo { name: "dev".into(), is_default: false }]).unwrap();
        assert_eq!(ws.meta_catalogs("c").unwrap().unwrap().iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["main", "dev"]);
        assert!(ws.meta_schemas("c", Some("dev")).unwrap().is_none());
        ws.meta_put_schemas("c", Some("dev"), &[sch("dev.a", Some("dev")), sch("dev.b", Some("dev"))]).unwrap();
        assert_eq!(ws.meta_schemas("c", Some("dev")).unwrap().unwrap().len(), 2);
        assert!(ws.meta_schemas("c", None).unwrap().is_none(), "the full list was never read");
        ws.meta_put_listing("c", "dev.a", &[o("dev.a", "t", ObjectKind::Table)], None, None).unwrap();
        // A schema of `dev` is gone: its listing goes too, the other stays.
        ws.meta_put_schemas("c", Some("dev"), &[sch("dev.b", Some("dev"))]).unwrap();
        assert!(ws.meta_listing("c", "dev.a").unwrap().is_none());
        ws.meta_put_listing("c", "dev.b", &[o("dev.b", "t", ObjectKind::Table)], None, None).unwrap();
        // Catalog `dev` dropped: its schemas and listings go.
        ws.meta_put_catalogs("c", &[CatalogInfo { name: "main".into(), is_default: true }]).unwrap();
        assert!(ws.meta_listing("c", "dev.b").unwrap().is_none());
        assert!(ws.meta_schemas("c", Some("dev")).unwrap().is_none());
        // A full list makes every catalog's list complete.
        ws.meta_put_schemas("c", None, &[sch("main.x", Some("main")), sch("main.y", Some("main"))]).unwrap();
        assert_eq!(ws.meta_schemas("c", Some("main")).unwrap().unwrap().len(), 2);
        assert_eq!(ws.meta_schemas("c", None).unwrap().unwrap().iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), vec!["main.x", "main.y"]);
        // Searching adds objects in an unlisted schema, which is not a listing.
        ws.meta_add_objects("c", &[o("main.z", "hit", ObjectKind::Table)]).unwrap();
        assert!(ws.meta_listing("c", "main.z").unwrap().is_none());
    }

    #[test]
    fn versions_drive_stale_columns_and_incremental_changes() {
        let ws = Workspace::open_in_memory().unwrap();
        let v = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<HashMap<_, _>>();
        let objs = [o("s", "a", ObjectKind::Table), o("s", "b", ObjectKind::Table), o("s", "a", ObjectKind::Function), o("s", "q", ObjectKind::Sequence)];
        ws.meta_put_listing("c", "s", &objs, Some(&v(&[("table:a", "1"), ("table:b", "1"), ("function:a", "1"), ("sequence:q", "1")])), Some("fp1")).unwrap();
        let l = ws.meta_listing("c", "s").unwrap().unwrap();
        assert_eq!(l.objects.len(), 4, "a table and a function may share a name; sequences are kept");
        assert_eq!(ws.meta_listed_schemas("c").unwrap(), vec![("s".to_string(), Some("fp1".to_string()))]);
        ws.meta_put_columns("c", "s", "a", &cols(&["id"])).unwrap();
        ws.meta_put_columns("c", "s", "b", &cols(&["id"])).unwrap();
        assert!(!ws.meta_columns("c", "s", "a").unwrap().unwrap().stale);

        // `b` changed, `a` dropped as a table, `n` new: only those are fetched.
        ws.meta_apply_changes(
            "c",
            "s",
            &[o("s", "b", ObjectKind::Table), o("s", "n", ObjectKind::View)],
            &["table:a".into()],
            &v(&[("table:b", "2"), ("view:n", "1"), ("function:a", "1"), ("sequence:q", "1")]),
            Some("fp2"),
        )
        .unwrap();
        let names: Vec<String> = ws.meta_listing("c", "s").unwrap().unwrap().objects.iter().map(|o| version_key(o.kind, &o.name)).collect();
        assert_eq!(names, vec!["function:a", "table:b", "view:n", "sequence:q"]);
        assert!(ws.meta_columns("c", "s", "b").unwrap().unwrap().stale, "changed table: columns stale but kept");
        assert_eq!(ws.meta_column_names("c", "s", "b").unwrap().unwrap(), vec!["id"], "still usable offline");
        assert!(ws.meta_columns("c", "s", "a").unwrap().is_some(), "function `a` keeps the name present");
        assert_eq!(ws.meta_versions("c", "s").unwrap()["table:b"], "2");
        ws.meta_put_columns("c", "s", "b", &cols(&["id", "x"])).unwrap();
        assert!(!ws.meta_columns("c", "s", "b").unwrap().unwrap().stale, "describe refreshes");

        // A full listing with versions: only changed objects go stale; gone ones lose columns.
        ws.meta_put_columns("c", "s", "n", &cols(&["v"])).unwrap();
        ws.meta_put_listing(
            "c",
            "s",
            &[o("s", "b", ObjectKind::Table), o("s", "n", ObjectKind::View)],
            Some(&v(&[("table:b", "2"), ("view:n", "9")])),
            Some("fp3"),
        )
        .unwrap();
        assert!(!ws.meta_columns("c", "s", "b").unwrap().unwrap().stale);
        assert!(ws.meta_columns("c", "s", "n").unwrap().unwrap().stale);
        assert!(ws.meta_columns("c", "s", "a").unwrap().is_none(), "no object `a` any more");
        // Without versions, a listing at an unknown fingerprint makes all columns stale.
        ws.meta_put_columns("c", "s", "b", &cols(&["id"])).unwrap();
        ws.meta_put_schema("c", "s", &[o("s", "b", ObjectKind::Table)]).unwrap();
        assert!(ws.meta_columns("c", "s", "b").unwrap().unwrap().stale);
        assert!(ws.meta_versions("c", "s").unwrap().is_empty());
        // Completion only offers relations.
        ws.meta_put_schema("c", "s", &[o("s", "b", ObjectKind::Table), o("s", "q", ObjectKind::Sequence)]).unwrap();
        assert_eq!(ws.kn_complete("c", None, "", 10).unwrap().len(), 1);
    }

    #[test]
    fn a_table_is_known_from_the_newer_of_the_explorer_and_the_index() {
        let ws = Workspace::open_in_memory().unwrap();
        let col = |n: &str| ColumnInfo { name: n.into(), data_type: "int".into(), nullable: true, is_primary_key: false, default: None, comment: None };
        let fk = ForeignKey { columns: vec!["cust".into()], ref_schema: "s".into(), ref_table: "customers".into(), ref_columns: vec!["id".into()] };
        let age = |sql: &str| ws.conn.lock().execute(sql, []).unwrap();
        assert!(ws.meta_table("c", "s", "orders").unwrap().is_none());

        // The explorer described it, foreign keys included.
        ws.meta_put_table("c", "s", "orders", &[col("id"), col("cust")], Some(std::slice::from_ref(&fk))).unwrap();
        let t = ws.meta_table("c", "s", "ORDERS").unwrap().unwrap();
        assert_eq!((t.columns.len(), t.foreign_keys.clone(), t.stale), (2, Some(vec![fk.clone()]), false));
        // Columns cached without them: not known (not "none").
        ws.meta_put_columns("c", "s", "orders", &[col("id"), col("cust")]).unwrap();
        assert_eq!(ws.meta_table("c", "s", "orders").unwrap().unwrap().foreign_keys, None);

        // An index run after it: the index's columns, and its foreign keys.
        age("UPDATE meta_columns SET seen_at = 1");
        let indexed = |cols: Vec<ColumnInfo>, fks: Vec<ForeignKey>| crate::KnObject {
            schema: "s".into(),
            name: "orders".into(),
            kind: "table".into(),
            comment: None,
            row_estimate: None,
            columns: cols,
            foreign_keys: fks,
        };
        ws.kn_upsert_objects("c", &[indexed(vec![col("id"), col("cust"), col("added")], vec![fk.clone()])]).unwrap();
        let t = ws.meta_table("c", "s", "orders").unwrap().unwrap();
        assert_eq!(t.columns.len(), 3, "the newer read wins");
        assert_eq!((t.kind.as_deref(), t.foreign_keys), (Some("table"), Some(vec![fk.clone()])));
        assert_eq!(ws.kn_column_names("c", "s", "orders").unwrap().unwrap(), vec!["id", "cust", "added"]);

        // The explorer described it after the index: its columns win again,
        // and keys it doesn't know come from the index.
        age("UPDATE kn_objects SET indexed_at = 1");
        ws.meta_put_columns("c", "s", "orders", &[col("id")]).unwrap();
        let t = ws.meta_table("c", "s", "orders").unwrap().unwrap();
        assert_eq!((t.columns.len(), t.foreign_keys), (1, Some(vec![fk])));
        assert_eq!(ws.kn_column_names("c", "s", "orders").unwrap().unwrap(), vec!["id"], "not the older index");

        // The schema changed since: still shown, marked stale.
        age("UPDATE meta_columns SET stale = 1");
        assert!(ws.meta_table("c", "s", "orders").unwrap().unwrap().stale);
    }

    #[test]
    fn columns_of_schemas_never_listed_go_stale_after_a_day() {
        let ws = Workspace::open_in_memory().unwrap();
        let col = ColumnInfo { name: "id".into(), data_type: "int".into(), nullable: true, is_primary_key: false, default: None, comment: None };
        // `s` listed in full (refreshes check it); `t` only seen by completion.
        ws.meta_put_schemas("c", None, &[sch("s", None), sch("t", None)]).unwrap();
        ws.meta_put_schema("c", "s", &[o("s", "a", ObjectKind::Table)]).unwrap();
        ws.meta_put_columns("c", "s", "a", std::slice::from_ref(&col)).unwrap();
        ws.meta_put_columns("c", "t", "b", std::slice::from_ref(&col)).unwrap();
        let stale = |s: &str, n: &str| (ws.meta_columns("c", s, n).unwrap().unwrap().stale, ws.meta_table("c", s, n).unwrap().unwrap().stale);
        assert_eq!((stale("s", "a"), stale("t", "b")), ((false, false), (false, false)), "fresh");
        let old = now_ms() - UNLISTED_COLUMNS_TTL_MS - 1000;
        ws.conn.lock().execute("UPDATE meta_columns SET seen_at = ?1", [old]).unwrap();
        assert_eq!(stale("s", "a"), (false, false), "a listed schema's refresh checks it");
        assert_eq!(stale("t", "b"), (true, true), "nothing else would");
        // Described again: fresh.
        ws.meta_put_columns("c", "t", "b", &[col]).unwrap();
        assert_eq!(stale("t", "b"), (false, false));
    }

    #[test]
    fn identity_change_clears_cache() {
        use databrain_auth::AuthMethod;
        let ws = Workspace::open_in_memory().unwrap();
        let mut cfg = ConnectionConfig::new(databrain_connector_core::ConnectorKind::Postgres, AuthMethod::Password { user: "u".into() });
        cfg.host = Some("db1".into());
        ws.meta_check_identity("c", &cfg).unwrap();
        let indexed = crate::KnObject {
            schema: "s".into(),
            name: "t".into(),
            kind: "table".into(),
            comment: None,
            row_estimate: None,
            columns: vec![],
            foreign_keys: vec![],
        };
        ws.kn_upsert_objects("c", std::slice::from_ref(&indexed)).unwrap();
        let note = crate::KnNote {
            id: String::new(),
            connection_id: "c".into(),
            target: Some("s.t".into()),
            body: "t holds test rows".into(),
            author: "me".into(),
            status: crate::NoteStatus::Approved,
            created_at: 0,
            replaces: None,
        };
        ws.kn_save_note(note).unwrap();
        ws.meta_put_schemas("c", None, &[sch("s", None)]).unwrap();
        ws.meta_put_schema("c", "s", &[o("s", "t", ObjectKind::Table)]).unwrap();
        cfg.read_only = true;
        ws.meta_check_identity("c", &cfg).unwrap();
        assert!(ws.meta_listing("c", "s").unwrap().is_some(), "same server");
        cfg.options.insert("connect_timeout_secs".into(), "90".into());
        cfg.options.insert("idle_disconnect_minutes".into(), "0".into());
        ws.meta_check_identity("c", &cfg).unwrap();
        assert!(ws.meta_listing("c", "s").unwrap().is_some(), "session settings don't change the server");
        assert_eq!(ws.kn_count("c").unwrap(), 1, "the index stays for the same server");
        cfg.host = Some("db2".into());
        ws.meta_check_identity("c", &cfg).unwrap();
        assert!(ws.meta_listing("c", "s").unwrap().is_none());
        assert!(ws.meta_schemas("c", None).unwrap().is_none());
        assert_eq!(ws.meta_count("c").unwrap(), 0);
        // The AI index described the old server too; the user's notes stay.
        assert_eq!(ws.kn_count("c").unwrap(), 0);
        assert!(ws.kn_get("c", "s.t").unwrap().is_none());
        assert_eq!(ws.kn_notes("c").unwrap().len(), 1);
    }

    impl Workspace {
        fn delete_connection_cache_for_test(&self, id: &str) {
            self.meta_clear(id).unwrap();
        }
    }
}
