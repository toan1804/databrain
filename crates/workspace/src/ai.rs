//! Storage for AI providers, chat sessions, messages and the tool audit log.

use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::{AI_HISTORY_RETENTION_SETTING, Error, Result, Workspace, new_id, now_ms};

/// A configured AI provider. `config` is interpreted by the AI crate
/// (base URL, auth method, default model, ...). Never contains secrets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
#[ts(optional_fields = nullable)]
pub struct AiProviderRecord {
    pub id: String,
    #[ts(type = "import(\"./ProviderKind\").ProviderKind")]
    pub kind: String,
    pub name: String,
    #[serde(default)]
    #[ts(type = "import(\"./ProviderConfig\").ProviderConfig")]
    pub config: serde_json::Value,
    #[serde(default)]
    #[ts(as = "f64")]
    pub created_at: i64,
    #[serde(default)]
    #[ts(as = "f64")]
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
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
    #[ts(as = "f64")]
    pub created_at: i64,
    #[serde(default)]
    #[ts(as = "f64")]
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
pub struct AiMessageRecord {
    #[ts(as = "f64")]
    pub id: i64,
    pub session_id: String,
    pub role: String,
    pub content: serde_json::Value,
    #[ts(as = "Option<f64>")]
    pub tokens_in: Option<i64>,
    #[ts(as = "Option<f64>")]
    pub tokens_out: Option<i64>,
    #[ts(as = "f64")]
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
pub struct AuditEntry {
    #[ts(as = "f64")]
    pub id: i64,
    pub session_id: Option<String>,
    pub connection_id: Option<String>,
    pub tool: String,
    pub args: serde_json::Value,
    /// `allowed`, `approved`, `denied`, `blocked`, `error`.
    pub decision: String,
    pub summary: Option<String>,
    #[ts(as = "f64")]
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
        self.list_ai_providers()?.into_iter().find(|p| p.id == id).ok_or_else(|| Error::NotFound(format!("AI provider {id}")))
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
        let rows = stmt.query_map(params![connection_id, limit as i64], session_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn get_ai_session(&self, id: &str) -> Result<AiSessionRecord> {
        let c = self.conn.lock();
        c.query_row("SELECT id, title, connection_id, provider_id, model, created_at, updated_at FROM ai_sessions WHERE id = ?1", [id], session_row)
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
        // The title is made from the first question.
        s.title = databrain_connector_core::sql::redact_text(&s.title);
        self.conn.lock().execute(
            "INSERT INTO ai_sessions (id, title, connection_id, provider_id, model, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT(id) DO UPDATE SET title = excluded.title, connection_id = excluded.connection_id, \
               provider_id = excluded.provider_id, model = excluded.model, updated_at = excluded.updated_at",
            params![s.id, s.title, s.connection_id, s.provider_id, s.model, if s.created_at == 0 { now } else { s.created_at }, s.updated_at],
        )?;
        Ok(s)
    }

    pub fn delete_ai_session(&self, id: &str) -> Result<()> {
        let c = self.conn.lock();
        c.execute("DELETE FROM ai_sessions WHERE id = ?1", [id])?;
        // Kiro's own session id for it.
        c.execute("DELETE FROM settings WHERE key = ?1", [format!("kiro_session:{id}")])?;
        Ok(())
    }

    /// Days of AI conversations kept (Settings → AI); `None` = keep them all.
    pub fn ai_history_retention_days(&self) -> Option<u32> {
        match self.get_setting(AI_HISTORY_RETENTION_SETTING).ok().flatten() {
            None => Some(crate::HISTORY_RETENTION_DEFAULT_DAYS),
            Some(v) => v.as_u64().map(|d| d as u32).filter(|d| *d > 0),
        }
    }

    /// Delete conversations not used for longer than the retention setting
    /// (their messages go with them). Returns how many went.
    pub fn prune_ai_history(&self) -> Result<usize> {
        let Some(days) = self.ai_history_retention_days() else { return Ok(0) };
        let cutoff = now_ms() - days as i64 * 86_400_000;
        let ids: Vec<String> = {
            let c = self.conn.lock();
            let mut st = c.prepare("SELECT id FROM ai_sessions WHERE updated_at < ?1")?;
            st.query_map([cutoff], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        for id in &ids {
            self.delete_ai_session(id)?;
        }
        Ok(ids.len())
    }

    /// Add a message to a conversation. Passwords and keys in it (SQL the
    /// assistant ran or proposed, a connection string in a question, a tool
    /// result quoting them) are replaced by `***` first, as in the audit log:
    /// the kept transcript is also what the next turn sends the model.
    pub fn add_ai_message(&self, session_id: &str, role: &str, content: &serde_json::Value, tokens: (Option<i64>, Option<i64>)) -> Result<i64> {
        let c = self.conn.lock();
        let now = now_ms();
        c.execute(
            "INSERT INTO ai_messages (session_id, role, content_json, tokens_in, tokens_out, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![session_id, role, serde_json::to_string(&redact_json(content))?, tokens.0, tokens.1, now],
        )?;
        let id = c.last_insert_rowid();
        c.execute("UPDATE ai_sessions SET updated_at = ?1 WHERE id = ?2", params![now, session_id])?;
        Ok(id)
    }

    /// Tokens (input, output) reported for a provider since `since_ms`: its
    /// conversations (messages of deleted conversations are not counted) and
    /// its calls outside them ([`Self::record_ai_usage`]).
    pub fn ai_token_totals(&self, provider_id: &str, since_ms: i64) -> Result<(i64, i64)> {
        let c = self.conn.lock();
        Ok(c.query_row(
            "SELECT COALESCE(SUM(i), 0), COALESCE(SUM(o), 0) FROM ( \
               SELECT m.tokens_in AS i, m.tokens_out AS o FROM ai_messages m JOIN ai_sessions s ON s.id = m.session_id \
               WHERE s.provider_id = ?1 AND m.created_at >= ?2 \
               UNION ALL \
               SELECT tokens_in, tokens_out FROM ai_usage WHERE provider_id = ?1 AND created_at >= ?2)",
            params![provider_id, since_ms],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }

    /// Tokens of an AI call outside a conversation (`feature`: `inline`,
    /// `test_data`), so the provider's usage counts it.
    pub fn record_ai_usage(&self, provider_id: &str, feature: &str, tokens_in: u32, tokens_out: u32) -> Result<()> {
        if tokens_in == 0 && tokens_out == 0 {
            return Ok(());
        }
        self.conn.lock().execute(
            "INSERT INTO ai_usage (provider_id, feature, tokens_in, tokens_out, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![provider_id, feature, tokens_in, tokens_out, now_ms()],
        )?;
        Ok(())
    }

    pub fn list_ai_messages(&self, session_id: &str) -> Result<Vec<AiMessageRecord>> {
        let c = self.conn.lock();
        let mut stmt =
            c.prepare("SELECT id, session_id, role, content_json, tokens_in, tokens_out, created_at FROM ai_messages WHERE session_id = ?1 ORDER BY id")?;
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
        let summary = summary.map(databrain_connector_core::sql::redact_text);
        self.conn.lock().execute(
            "INSERT INTO ai_audit (session_id, connection_id, tool, args_json, decision, summary, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![session_id, connection_id, tool, serde_json::to_string(&redact_json(args))?, decision, summary, now_ms()],
        )?;
        Ok(())
    }

    pub fn list_audit(&self, limit: u32) -> Result<Vec<AuditEntry>> {
        let c = self.conn.lock();
        let mut stmt =
            c.prepare("SELECT id, session_id, connection_id, tool, args_json, decision, summary, created_at FROM ai_audit ORDER BY id DESC LIMIT ?1")?;
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

/// Every string in `v` without passwords and keys ([`redact_text`](databrain_connector_core::sql::redact_text)).
pub(crate) fn redact_json(v: &serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::String(s) => serde_json::Value::String(databrain_connector_core::sql::redact_text(s)),
        serde_json::Value::Array(a) => serde_json::Value::Array(a.iter().map(redact_json).collect()),
        serde_json::Value::Object(o) => serde_json::Value::Object(o.iter().map(|(k, v)| (k.clone(), redact_json(v))).collect()),
        other => other.clone(),
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

#[cfg(test)]
mod tests {
    use crate::{AiSessionRecord, Workspace};

    #[test]
    fn totals_tokens_per_provider() {
        let ws = Workspace::open_in_memory().unwrap();
        let s = |p: &str| {
            ws.save_ai_session(AiSessionRecord {
                id: String::new(),
                title: "t".into(),
                connection_id: None,
                provider_id: Some(p.into()),
                model: None,
                created_at: 0,
                updated_at: 0,
            })
            .unwrap()
        };
        let (a, b) = (s("p1"), s("p2"));
        ws.add_ai_message(&a.id, "assistant", &serde_json::json!({}), (Some(100), Some(20))).unwrap();
        ws.add_ai_message(&a.id, "user", &serde_json::json!({}), (None, None)).unwrap();
        ws.add_ai_message(&b.id, "assistant", &serde_json::json!({}), (Some(5), Some(5))).unwrap();
        assert_eq!(ws.ai_token_totals("p1", 0).unwrap(), (100, 20));
        assert_eq!(ws.ai_token_totals("p1", i64::MAX).unwrap(), (0, 0));
        // Calls outside conversations (inline completion, test data) count too.
        ws.record_ai_usage("p1", "test_data", 40, 900).unwrap();
        ws.record_ai_usage("p1", "inline", 0, 0).unwrap();
        ws.record_ai_usage("p3", "inline", 7, 1).unwrap();
        assert_eq!(ws.ai_token_totals("p1", 0).unwrap(), (140, 920));
        assert_eq!(ws.ai_token_totals("p3", 0).unwrap(), (7, 1), "a provider without conversations");
        assert_eq!(ws.ai_token_totals("p1", i64::MAX).unwrap(), (0, 0));
        let n: i64 = ws.conn.lock().query_row("SELECT COUNT(*) FROM ai_usage", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 2, "nothing recorded for a call without tokens");
        // Retention: conversations idle for longer than the setting are deleted with their messages.
        assert_eq!(ws.ai_history_retention_days(), Some(30));
        ws.conn.lock().execute("UPDATE ai_sessions SET updated_at = ?1 WHERE id = ?2", rusqlite::params![crate::now_ms() - 31 * 86_400_000, a.id]).unwrap();
        ws.set_setting(crate::AI_HISTORY_RETENTION_SETTING, &serde_json::json!(0)).unwrap();
        assert_eq!(ws.prune_ai_history().unwrap(), 0, "0 = keep everything");
        ws.set_setting(crate::AI_HISTORY_RETENTION_SETTING, &serde_json::json!(30)).unwrap();
        assert_eq!(ws.prune_ai_history().unwrap(), 1);
        assert!(ws.list_ai_messages(&a.id).unwrap().is_empty());
        assert_eq!(ws.ai_token_totals("p2", 0).unwrap(), (5, 5), "recent conversation kept");
    }

    /// What a conversation keeps: no passwords or keys in the question, the
    /// title, tool calls' SQL or tool results; the same in the audit log.
    #[test]
    fn transcripts_are_redacted() {
        let ws = Workspace::open_in_memory().unwrap();
        let s = ws
            .save_ai_session(AiSessionRecord {
                id: String::new(),
                title: "use postgres://ann:hunter2@db/app".into(),
                connection_id: None,
                provider_id: None,
                model: None,
                created_at: 0,
                updated_at: 0,
            })
            .unwrap();
        assert_eq!(ws.get_ai_session(&s.id).unwrap().title, "use postgres://ann:***@db/app");
        let user = serde_json::json!({"role": "user", "text": "Connect with postgres://ann:hunter2@db/app and run create user bob identified by 'pw1'"});
        let assistant = serde_json::json!({"role": "assistant", "text": "", "tool_calls": [{"id": "c1", "name": "run_query", "arguments": {"sql": "ALTER ROLE r PASSWORD 'pw2'", "n": 3}}]});
        let tool = serde_json::json!({"role": "tool", "text": "Statement 1 failed: near \"SECRET 'pw3'\"", "tool_call_id": "c1"});
        for (role, m) in [("user", &user), ("assistant", &assistant), ("tool", &tool)] {
            ws.add_ai_message(&s.id, role, m, (None, None)).unwrap();
        }
        let kept: Vec<serde_json::Value> = ws.list_ai_messages(&s.id).unwrap().into_iter().map(|m| m.content).collect();
        let all = serde_json::to_string(&kept).unwrap();
        for secret in ["hunter2", "pw1", "pw2", "pw3"] {
            assert!(!all.contains(secret), "{secret} kept: {all}");
        }
        assert_eq!(kept[0]["text"], "Connect with postgres://ann:***@db/app and run create user bob identified by '***'");
        assert_eq!(kept[1]["tool_calls"][0]["arguments"], serde_json::json!({"sql": "ALTER ROLE r PASSWORD '***'", "n": 3}), "structure and other values kept");
        assert_eq!(kept[2]["tool_call_id"], "c1");
        ws.add_audit(Some(&s.id), None, "run_query", &serde_json::json!({"sql": "attach 'mysql://root:pw4@h/db'"}), "approved", Some("password 'pw5'"))
            .unwrap();
        let a = &ws.list_audit(1).unwrap()[0];
        assert_eq!(a.args["sql"], "attach 'mysql://root:***@h/db'");
        assert_eq!(a.summary.as_deref(), Some("password '***'"));
    }

    /// Conversations kept before redaction are cleaned once when the workspace opens.
    #[test]
    fn redacts_kept_transcripts_once() {
        let dir = std::env::temp_dir().join(format!("databrain-ws-redact-{}", crate::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ws.db");
        let id = {
            let ws = Workspace::open(&path).unwrap();
            let s = ws
                .save_ai_session(AiSessionRecord {
                    id: String::new(),
                    title: "t".into(),
                    connection_id: None,
                    provider_id: None,
                    model: None,
                    created_at: 0,
                    updated_at: 0,
                })
                .unwrap();
            let c = ws.conn.lock();
            // As an older version kept them.
            c.execute("UPDATE ai_sessions SET title = 'about mongodb://u:old1@h' WHERE id = ?1", [&s.id]).unwrap();
            c.execute(
                "INSERT INTO ai_messages (session_id, role, content_json, created_at) VALUES (?1, 'user', ?2, 0)",
                rusqlite::params![s.id, serde_json::json!({"role": "user", "text": "create user u identified by old2"}).to_string()],
            )
            .unwrap();
            c.execute("INSERT INTO ai_messages (session_id, role, content_json, created_at) VALUES (?1, 'tool', 'not json: password ''old3''', 0)", [&s.id])
                .unwrap();
            c.execute(
                "INSERT INTO ai_audit (tool, args_json, decision, summary, created_at) VALUES ('run_query', ?1, 'approved', 'mysql://r:old4@h', 0)",
                [serde_json::json!({"sql": "attach 'pg://a:old5@h'"}).to_string()],
            )
            .unwrap();
            c.execute("DELETE FROM maintenance WHERE name = 'transcripts_redacted_v1'", []).unwrap();
            s.id
        };
        let ws = Workspace::open(&path).unwrap();
        assert_eq!(ws.get_ai_session(&id).unwrap().title, "about mongodb://u:***@h");
        let raw: Vec<String> = ws
            .conn
            .lock()
            .prepare("SELECT content_json FROM ai_messages ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(raw[0].contains("identified by ***") && !raw[0].contains("old2"), "{}", raw[0]);
        assert_eq!(raw[1], "not json: password '***'");
        let a = &ws.list_audit(1).unwrap()[0];
        assert_eq!((a.args["sql"].as_str(), a.summary.as_deref()), (Some("attach 'pg://a:***@h'"), Some("mysql://r:***@h")));
        let done: i64 = ws.conn.lock().query_row("SELECT count(*) FROM maintenance WHERE name = 'transcripts_redacted_v1'", [], |r| r.get(0)).unwrap();
        assert_eq!(done, 1);
        drop(ws);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
