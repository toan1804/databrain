//! Notebooks: ordered SQL / Markdown cells bound to a connection.
//!
//! Cells are stored as one JSON document. Results are not persisted (they
//! live in the in-memory result store); only the last run's summary is kept
//! so a reopened notebook shows what ran.

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::{Error, Result, Workspace, new_id, now_ms};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CellKind {
    #[default]
    Sql,
    Markdown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CellRunSummary {
    #[serde(default)]
    pub finished_at: i64,
    #[serde(default)]
    pub duration_ms: i64,
    #[serde(default)]
    pub rows: Option<i64>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotebookCell {
    pub id: String,
    #[serde(default)]
    pub kind: CellKind,
    #[serde(default)]
    pub source: String,
    /// Optional per-cell connection override (else the notebook's).
    #[serde(default)]
    pub connection_id: Option<String>,
    #[serde(default)]
    pub collapsed: bool,
    #[serde(default)]
    pub last_run: Option<CellRunSummary>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notebook {
    #[serde(default)]
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub connection_id: Option<String>,
    #[serde(default)]
    pub folder_id: Option<String>,
    #[serde(default)]
    pub cells: Vec<NotebookCell>,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: i64,
}

/// Listing entry (without cells).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotebookSummary {
    pub id: String,
    pub name: String,
    pub connection_id: Option<String>,
    pub folder_id: Option<String>,
    pub cell_count: i64,
    pub updated_at: i64,
}

impl Workspace {
    pub fn list_notebooks(&self) -> Result<Vec<NotebookSummary>> {
        let c = self.conn.lock();
        let mut stmt = c.prepare(
            "SELECT id, name, connection_id, folder_id, json_array_length(cells_json), updated_at FROM notebooks ORDER BY name COLLATE NOCASE",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(NotebookSummary {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    connection_id: r.get(2)?,
                    folder_id: r.get(3)?,
                    cell_count: r.get(4)?,
                    updated_at: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn get_notebook(&self, id: &str) -> Result<Notebook> {
        let c = self.conn.lock();
        c.query_row(
            "SELECT id, name, connection_id, folder_id, cells_json, created_at, updated_at FROM notebooks WHERE id = ?1",
            [id],
            |r| {
                Ok(Notebook {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    connection_id: r.get(2)?,
                    folder_id: r.get(3)?,
                    cells: serde_json::from_str(&r.get::<_, String>(4)?).unwrap_or_default(),
                    created_at: r.get(5)?,
                    updated_at: r.get(6)?,
                })
            },
        )
        .optional()?
        .ok_or_else(|| Error::NotFound(format!("notebook {id}")))
    }

    /// Insert (empty id) or update. Cells without ids get one.
    pub fn save_notebook(&self, mut nb: Notebook) -> Result<Notebook> {
        if nb.name.trim().is_empty() {
            return Err(Error::Invalid("notebook name is required".into()));
        }
        for cell in &mut nb.cells {
            if cell.id.is_empty() {
                cell.id = new_id();
            }
        }
        let now = now_ms();
        if nb.id.is_empty() {
            nb.id = new_id();
            nb.created_at = now;
        }
        nb.updated_at = now;
        let cells = serde_json::to_string(&nb.cells).map_err(|e| Error::Invalid(e.to_string()))?;
        let c = self.conn.lock();
        c.execute(
            "INSERT INTO notebooks (id, name, connection_id, folder_id, cells_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET name = excluded.name, connection_id = excluded.connection_id,
               folder_id = excluded.folder_id, cells_json = excluded.cells_json, updated_at = excluded.updated_at",
            params![nb.id, nb.name.trim(), nb.connection_id, nb.folder_id, cells, nb.created_at, nb.updated_at],
        )?;
        drop(c);
        // created_at is preserved on conflict; re-read it for the caller.
        self.get_notebook(&nb.id)
    }

    pub fn delete_notebook(&self, id: &str) -> Result<()> {
        self.conn.lock().execute("DELETE FROM notebooks WHERE id = ?1", [id])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FolderKind;

    #[test]
    fn notebook_crud_and_folders() {
        let ws = Workspace::open_in_memory().unwrap();
        let nb = ws
            .save_notebook(Notebook {
                id: String::new(),
                name: "Revenue".into(),
                connection_id: None,
                folder_id: None,
                cells: vec![
                    NotebookCell { id: String::new(), kind: CellKind::Markdown, source: "# Revenue".into(), connection_id: None, collapsed: false, last_run: None },
                    NotebookCell { id: String::new(), kind: CellKind::Sql, source: "select 1".into(), connection_id: None, collapsed: false, last_run: None },
                ],
                created_at: 0,
                updated_at: 0,
            })
            .unwrap();
        assert!(nb.cells.iter().all(|c| !c.id.is_empty()));
        let list = ws.list_notebooks().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].cell_count, 2);

        let mut edited = nb.clone();
        edited.cells[1].last_run = Some(CellRunSummary { finished_at: 1, duration_ms: 5, rows: Some(1), error: None });
        edited.cells.swap(0, 1);
        let saved = ws.save_notebook(edited).unwrap();
        assert_eq!(saved.created_at, nb.created_at);
        assert_eq!(saved.cells[0].kind, CellKind::Sql);
        assert_eq!(saved.cells[0].last_run.as_ref().unwrap().rows, Some(1));

        let f = ws
            .save_folder(crate::Folder { id: String::new(), parent_id: None, name: "Analyses".into(), kind: FolderKind::Notebooks })
            .unwrap();
        ws.move_to_folder(FolderKind::Notebooks, &nb.id, Some(&f.id)).unwrap();
        assert_eq!(ws.get_notebook(&nb.id).unwrap().folder_id, Some(f.id.clone()));
        ws.delete_folder(&f.id).unwrap();
        assert_eq!(ws.get_notebook(&nb.id).unwrap().folder_id, None);

        assert!(ws.save_notebook(Notebook { name: " ".into(), ..nb.clone() }).is_err());
        ws.delete_notebook(&nb.id).unwrap();
        assert!(ws.get_notebook(&nb.id).is_err());
    }
}
