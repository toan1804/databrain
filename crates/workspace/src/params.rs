//! Named sets of `:name` parameter values ("Daily", "Tenant A"): applied to
//! a query tab or notebook, or given to a job, whose steps' parameters get
//! the set's values on every run.

use std::collections::BTreeMap;

use databrain_connector_core::sql::ParamValue;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::{Error, Result, Workspace, new_id, now_ms};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
pub struct ParamSet {
    #[serde(default)]
    pub id: String,
    pub name: String,
    /// `:name` → value.
    #[serde(default)]
    pub values: BTreeMap<String, ParamValue>,
    #[serde(default)]
    #[ts(as = "f64")]
    pub updated_at: i64,
}

impl Workspace {
    pub fn list_param_sets(&self) -> Result<Vec<ParamSet>> {
        let c = self.conn.lock();
        let mut st = c.prepare("SELECT id, name, values_json, updated_at FROM param_sets ORDER BY name COLLATE NOCASE")?;
        let rows = st
            .query_map([], |r| {
                Ok(ParamSet {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    values: serde_json::from_str(&r.get::<_, String>(2)?).unwrap_or_default(),
                    updated_at: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// A set by id, or by name (any case).
    pub fn find_param_set(&self, id_or_name: &str) -> Result<Option<ParamSet>> {
        let key = id_or_name.trim();
        Ok(self.list_param_sets()?.into_iter().find(|s| s.id == key || s.name.eq_ignore_ascii_case(key)))
    }

    /// Insert (empty id) or update. Names are unique, ignoring case.
    pub fn save_param_set(&self, mut set: ParamSet) -> Result<ParamSet> {
        set.name = set.name.trim().to_string();
        if set.name.is_empty() {
            return Err(Error::Invalid("Name the parameter set".into()));
        }
        if let Some(other) = self.find_param_set(&set.name)?.filter(|o| o.id != set.id) {
            return Err(Error::Invalid(format!("A parameter set named \"{}\" already exists", other.name)));
        }
        if set.id.is_empty() {
            set.id = new_id();
        }
        set.updated_at = now_ms();
        let values = serde_json::to_string(&set.values).map_err(|e| Error::Invalid(e.to_string()))?;
        self.conn.lock().execute(
            "INSERT INTO param_sets (id, name, values_json, updated_at) VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(id) DO UPDATE SET name = excluded.name, values_json = excluded.values_json, updated_at = excluded.updated_at",
            params![set.id, set.name, values, set.updated_at],
        )?;
        Ok(set)
    }

    pub fn delete_param_set(&self, id: &str) -> Result<()> {
        self.conn.lock().execute("DELETE FROM param_sets WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Name of the set `id` (for messages); `None` when it was deleted.
    pub fn param_set_name(&self, id: &str) -> Result<Option<String>> {
        Ok(self.conn.lock().query_row("SELECT name FROM param_sets WHERE id = ?1", [id], |r| r.get(0)).optional()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> ParamValue {
        ParamValue { value: s.into(), raw: false }
    }

    #[test]
    fn saves_finds_and_keeps_names_unique() {
        let ws = Workspace::open_in_memory().unwrap();
        let daily = ws
            .save_param_set(ParamSet { id: String::new(), name: " Daily ".into(), values: [("day".to_string(), v("2026-11-01"))].into(), updated_at: 0 })
            .unwrap();
        assert_eq!(daily.name, "Daily");
        assert_eq!(ws.find_param_set("daily").unwrap().unwrap().id, daily.id);
        assert_eq!(ws.find_param_set(&daily.id).unwrap().unwrap().values["day"].value, "2026-11-01");
        let e = ws.save_param_set(ParamSet { id: String::new(), name: "DAILY".into(), values: Default::default(), updated_at: 0 }).unwrap_err();
        assert!(e.to_string().contains("already exists"), "{e}");
        let mut changed = daily.clone();
        changed.values.insert("tenant".into(), v("acme"));
        ws.save_param_set(changed).unwrap();
        assert_eq!(ws.list_param_sets().unwrap()[0].values.len(), 2);
        ws.delete_param_set(&daily.id).unwrap();
        assert!(ws.list_param_sets().unwrap().is_empty());
        assert!(ws.param_set_name(&daily.id).unwrap().is_none());
    }
}
