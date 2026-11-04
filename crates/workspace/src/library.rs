//! Folders and saved queries (the library), with their origin and row mapping.

use rusqlite::{OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

use crate::{Error, Result, Workspace, new_id, now_ms};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    #[default]
    User,
    Ai,
    Mcp,
    /// A job step (manual or scheduled run).
    Job,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::User => "user",
            Origin::Ai => "ai",
            Origin::Mcp => "mcp",
            Origin::Job => "job",
        }
    }
    pub(crate) fn parse(s: &str) -> Self {
        match s {
            "ai" => Origin::Ai,
            "mcp" => Origin::Mcp,
            "job" => Origin::Job,
            _ => Origin::User,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum FolderKind {
    Connections,
    Queries,
    Notebooks,
}

impl FolderKind {
    fn as_str(self) -> &'static str {
        match self {
            FolderKind::Connections => "connections",
            FolderKind::Queries => "queries",
            FolderKind::Notebooks => "notebooks",
        }
    }
    pub(crate) fn parse(s: &str) -> Self {
        match s {
            "connections" => FolderKind::Connections,
            "notebooks" => FolderKind::Notebooks,
            _ => FolderKind::Queries,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
#[ts(optional_fields = nullable)]
pub struct Folder {
    pub id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    pub name: String,
    pub kind: FolderKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
#[ts(optional_fields = nullable)]
pub struct SavedQuery {
    pub id: String,
    pub name: String,
    pub sql: String,
    #[serde(default)]
    pub connection_id: Option<String>,
    #[serde(default)]
    pub folder_id: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Offered to the AI as a few-shot example for its connection.
    #[serde(default)]
    #[ts(as = "Option<bool>", optional)]
    pub ai_example: bool,
    #[serde(default)]
    #[ts(as = "f64")]
    pub created_at: i64,
    #[serde(default)]
    #[ts(as = "f64")]
    pub updated_at: i64,
}

impl Workspace {
    // ---------------------------------------------------------------- folders

    pub fn list_folders(&self, kind: FolderKind) -> Result<Vec<Folder>> {
        let c = self.conn.lock();
        let mut stmt = c.prepare("SELECT id, parent_id, name, kind FROM folders WHERE kind = ?1 ORDER BY name COLLATE NOCASE")?;
        let rows = stmt
            .query_map([kind.as_str()], |r| {
                Ok(Folder { id: r.get(0)?, parent_id: r.get(1)?, name: r.get(2)?, kind: FolderKind::parse(&r.get::<_, String>(3)?) })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn save_folder(&self, mut f: Folder) -> Result<Folder> {
        if f.name.trim().is_empty() {
            return Err(Error::Invalid("folder name is required".into()));
        }
        if f.id.is_empty() {
            f.id = new_id();
        }
        if f.parent_id.as_deref() == Some(f.id.as_str()) {
            return Err(Error::Invalid("a folder cannot be its own parent".into()));
        }
        let c = self.conn.lock();
        c.execute(
            "INSERT INTO folders (id, parent_id, name, kind) VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(id) DO UPDATE SET parent_id = excluded.parent_id, name = excluded.name",
            params![f.id, f.parent_id, f.name.trim(), f.kind.as_str()],
        )?;
        f.name = f.name.trim().to_string();
        Ok(f)
    }

    /// Move a connection, saved query or notebook into a folder (`None` = root).
    pub fn move_to_folder(&self, kind: FolderKind, item_id: &str, folder_id: Option<&str>) -> Result<()> {
        let c = self.conn.lock();
        let table = match kind {
            FolderKind::Connections => "connections",
            FolderKind::Queries => "saved_queries",
            FolderKind::Notebooks => "notebooks",
        };
        let n = c.execute(&format!("UPDATE {table} SET folder_id = ?1 WHERE id = ?2"), params![folder_id, item_id])?;
        if n == 0 {
            return Err(Error::NotFound(item_id.to_string()));
        }
        Ok(())
    }

    /// Deletes the folder and its sub-folders; contained items move to the root.
    pub fn delete_folder(&self, id: &str) -> Result<()> {
        let c = self.conn.lock();
        c.execute("DELETE FROM folders WHERE id = ?1", [id])?;
        Ok(())
    }

    // ---------------------------------------------------------------- saved queries

    pub fn list_saved_queries(&self, search: Option<&str>) -> Result<Vec<SavedQuery>> {
        let c = self.conn.lock();
        let base = "SELECT id, name, sql, connection_id, folder_id, description, tags_json, created_at, updated_at, ai_example FROM saved_queries";
        let rows = match search.map(str::trim).filter(|s| !s.is_empty()) {
            Some(s) => {
                let pat = format!("%{}%", s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
                let mut stmt = c.prepare(&format!(
                    "{base} WHERE name LIKE ?1 ESCAPE '\\' OR sql LIKE ?1 ESCAPE '\\' \
                     OR description LIKE ?1 ESCAPE '\\' OR tags_json LIKE ?1 ESCAPE '\\' \
                     ORDER BY updated_at DESC"
                ))?;
                stmt.query_map([pat], saved_row)?.collect::<rusqlite::Result<Vec<_>>>()?
            }
            None => {
                let mut stmt = c.prepare(&format!("{base} ORDER BY name COLLATE NOCASE"))?;
                stmt.query_map([], saved_row)?.collect::<rusqlite::Result<Vec<_>>>()?
            }
        };
        Ok(rows)
    }

    pub fn get_saved_query(&self, id: &str) -> Result<SavedQuery> {
        let c = self.conn.lock();
        c.query_row(
            "SELECT id, name, sql, connection_id, folder_id, description, tags_json, created_at, updated_at, ai_example \
             FROM saved_queries WHERE id = ?1",
            [id],
            saved_row,
        )
        .optional()?
        .ok_or_else(|| Error::NotFound(format!("saved query {id}")))
    }

    pub fn save_query(&self, mut q: SavedQuery) -> Result<SavedQuery> {
        if q.name.trim().is_empty() {
            return Err(Error::Invalid("query name is required".into()));
        }
        let now = now_ms();
        if q.id.is_empty() {
            q.id = new_id();
            q.created_at = now;
        }
        q.updated_at = now;
        q.name = q.name.trim().to_string();
        q.tags = q.tags.into_iter().map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect();
        let c = self.conn.lock();
        c.execute(
            "INSERT INTO saved_queries (id, name, sql, connection_id, folder_id, description, tags_json, created_at, updated_at, ai_example) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
             ON CONFLICT(id) DO UPDATE SET name = excluded.name, sql = excluded.sql, \
               connection_id = excluded.connection_id, folder_id = excluded.folder_id, \
               description = excluded.description, tags_json = excluded.tags_json, updated_at = excluded.updated_at, \
               ai_example = excluded.ai_example",
            params![
                q.id,
                q.name,
                q.sql,
                q.connection_id,
                q.folder_id,
                q.description,
                serde_json::to_string(&q.tags)?,
                if q.created_at == 0 { now } else { q.created_at },
                q.updated_at,
                q.ai_example,
            ],
        )?;
        drop(c);
        self.get_saved_query(&q.id)
    }

    pub fn delete_saved_query(&self, id: &str) -> Result<()> {
        self.conn.lock().execute("DELETE FROM saved_queries WHERE id = ?1", [id])?;
        Ok(())
    }
}

fn saved_row(r: &Row<'_>) -> rusqlite::Result<SavedQuery> {
    let tags: String = r.get(6)?;
    Ok(SavedQuery {
        id: r.get(0)?,
        name: r.get(1)?,
        sql: r.get(2)?,
        connection_id: r.get(3)?,
        folder_id: r.get(4)?,
        description: r.get(5)?,
        tags: serde_json::from_str(&tags).unwrap_or_default(),
        created_at: r.get(7)?,
        updated_at: r.get(8)?,
        ai_example: r.get(9)?,
    })
}
