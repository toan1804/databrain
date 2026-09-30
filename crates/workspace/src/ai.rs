//! Storage for AI providers, chat sessions, messages and the tool audit log.

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::{Error, Result, Workspace, new_id, now_ms};

/// A configured AI provider. `config` is interpreted by the AI crate
/// (base URL, auth method, default model, ...). Never contains secrets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AiProviderRecord {
    pub id: String,
    pub kind: String,
    pub name: String,
    #[serde(default)]
    pub config: serde_json::Value,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AiSessionRecord {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub connection_id: Option<String>,
    #[serde(default)]
    pub provider_id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AiMessageRecord {
    pub id: i64,
    pub session_id: String,
    pub role: String,
    pub content: serde_json::Value,
    pub tokens_in: Option<i64>,
    pub tokens_out: Option<i64>,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditEntry {
    pub id: i64,
    pub session_id: Option<String>,
    pub connection_id: Option<String>,
    pub tool: String,
    pub args: serde_json::Value,
    /// `allowed`, `approved`, `denied`, `blocked`, `error`.
    pub decision: String,
    pub summary: Option<String>,
    pub created_at: i64,
}

impl Workspace {
    pub fn list_ai_providers(&self) -> Result<Vec<AiProviderRecord>> {
        let c = self.conn.lock();
        let mut stmt = c.prepare("SELECT id, kind, name, config_json, created_at, updated_at FROM ai_providers ORDER BY created_at")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(AiProviderRecord {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    name: r.get(2)?,
                    config: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default(),
                    created_at: r.get(4)?,
                    updated_at: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn get_ai_provider(&self, id: &str) -> Result<AiProviderRecord> {
        self.list_ai_providers()?
            .into_iter()
            .find(|p| p.id == id)
            .ok_or_else(|| Error::NotFound(format!("AI provider {id}")))
    }

    pub fn save_ai_provider(&self, mut p: AiProviderRecord) -> Result<AiProviderRecord> {
        if p.name.trim().is_empty() {
            return Err(Error::Invalid("provider name is required".into()));
        }
        let now = now_ms();
        if p.id.is_empty() {
            p.id = new_id();
            p.created_at = now;
        }
        p.updated_at = now;
        self.conn.lock().execute(
            "INSERT INTO ai_providers (id, kind, name, config_json, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(id) DO UPDATE SET kind = excluded.kind, name = excluded.name, config_json = excluded.config_json, updated_at = excluded.updated_at",
            params![p.id, p.kind, p.name.trim(), serde_json::to_string(&p.config)?, if p.created_at == 0 { now } else { p.created_at }, p.updated_at],
        )?;
        self.get_ai_provider(&p.id)
    }

    pub fn delete_ai_provider(&self, id: &str) -> Result<()> {
        self.conn.lock().execute("DELETE FROM ai_providers WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn list_ai_sessions(&self, connection_id: Option<&str>, limit: u32) -> Result<Vec<AiSessionRecord>> {
        let c = self.conn.lock();
        let mut stmt = c.prepare(
            "SELECT id, title, connection_id, provider_id, model, created_at, updated_at FROM ai_sessions \
             WHERE (?1 IS NULL OR connection_id = ?1) ORDER BY updated_at DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![connection_id, limit as i64], session_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn get_ai_session(&self, id: &str) -> Result<AiSessionRecord> {
        let c = self.conn.lock();
        c.query_row(
            "SELECT id, title, connection_id, provider_id, model, created_at, updated_at FROM ai_sessions WHERE id = ?1",
            [id],
            session_row,
        )
        .optional()?
        .ok_or_else(|| Error::NotFound(format!("AI session {id}")))
    }

    pub fn save_ai_session(&self, mut s: AiSessionRecord) -> Result<AiSessionRecord> {
        let now = now_ms();
        if s.id.is_empty() {
            s.id = new_id();
            s.created_at = now;
        }
        s.updated_at = now;
        self.conn.lock().execute(
            "INSERT INTO ai_sessions (id, title, connection_id, provider_id, model, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT(id) DO UPDATE SET title = excluded.title, connection_id = excluded.connection_id, \
               provider_id = excluded.provider_id, model = excluded.model, updated_at = excluded.updated_at",
            params![s.id, s.title, s.connection_id, s.provider_id, s.model, if s.created_at == 0 { now } else { s.created_at }, s.updated_at],
        )?;
        Ok(s)
    }

    pub fn delete_ai_session(&self, id: &str) -> Result<()> {
        self.conn.lock().execute("DELETE FROM ai_sessions WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn add_ai_message(
        &self,
        session_id: &str,
        role: &str,
        content: &serde_json::Value,
        tokens: (Option<i64>, Option<i64>),
    ) -> Result<i64> {
        let c = self.conn.lock();
        let now = now_ms();
        c.execute(
            "INSERT INTO ai_messages (session_id, role, content_json, tokens_in, tokens_out, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![session_id, role, serde_json::to_string(content)?, tokens.0, tokens.1, now],
        )?;
        let id = c.last_insert_rowid();
        c.execute("UPDATE ai_sessions SET updated_at = ?1 WHERE id = ?2", params![now, session_id])?;
        Ok(id)
    }

    pub fn list_ai_messages(&self, session_id: &str) -> Result<Vec<AiMessageRecord>> {
        let c = self.conn.lock();
        let mut stmt = c.prepare(
            "SELECT id, session_id, role, content_json, tokens_in, tokens_out, created_at FROM ai_messages WHERE session_id = ?1 ORDER BY id",
        )?;
        let rows = stmt
            .query_map([session_id], |r| {
                Ok(AiMessageRecord {
                    id: r.get(0)?,
                    session_id: r.get(1)?,
                    role: r.get(2)?,
                    content: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default(),
                    tokens_in: r.get(4)?,
                    tokens_out: r.get(5)?,
                    created_at: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn add_audit(
        &self,
        session_id: Option<&str>,
        connection_id: Option<&str>,
        tool: &str,
        args: &serde_json::Value,
        decision: &str,
        summary: Option<&str>,
    ) -> Result<()> {
        self.conn.lock().execute(
            "INSERT INTO ai_audit (session_id, connection_id, tool, args_json, decision, summary, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![session_id, connection_id, tool, serde_json::to_string(args)?, decision, summary, now_ms()],
        )?;
        Ok(())
    }

    pub fn list_audit(&self, limit: u32) -> Result<Vec<AuditEntry>> {
        let c = self.conn.lock();
        let mut stmt = c.prepare(
            "SELECT id, session_id, connection_id, tool, args_json, decision, summary, created_at FROM ai_audit ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map([limit as i64], |r| {
                Ok(AuditEntry {
                    id: r.get(0)?,
                    session_id: r.get(1)?,
                    connection_id: r.get(2)?,
                    tool: r.get(3)?,
                    args: serde_json::from_str(&r.get::<_, String>(4)?).unwrap_or_default(),
                    decision: r.get(5)?,
                    summary: r.get(6)?,
                    created_at: r.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

fn session_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<AiSessionRecord> {
    Ok(AiSessionRecord {
        id: r.get(0)?,
        title: r.get(1)?,
        connection_id: r.get(2)?,
        provider_id: r.get(3)?,
        model: r.get(4)?,
        created_at: r.get(5)?,
        updated_at: r.get(6)?,
    })
}
