//! Schema migrations, the schema-version guard, and one-time cleanups of kept data.

use rusqlite::{Connection, params};

use crate::{Error, Result, Workspace, ai, now_ms};

/// The workspace's schema version, refused when a newer DataBrain wrote it:
/// this one would not know its tables and could drop what it doesn't read
/// when saving. Nothing is changed then.
pub(crate) fn check_schema_version(conn: &Connection) -> Result<i64> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version as usize > MIGRATIONS.len() {
        return Err(Error::Invalid(format!(
            "This workspace was saved by a newer DataBrain (schema version {version}; this version knows up to {}). Update DataBrain to open it; nothing was changed.",
            MIGRATIONS.len()
        )));
    }
    Ok(version)
}

pub(crate) const MIGRATIONS: &[&str] = &[
    // v1
    r#"
    CREATE TABLE folders (
        id TEXT PRIMARY KEY,
        parent_id TEXT REFERENCES folders(id) ON DELETE CASCADE,
        name TEXT NOT NULL,
        kind TEXT NOT NULL
    );
    CREATE TABLE connections (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        kind TEXT NOT NULL,
        config_json TEXT NOT NULL,
        color TEXT,
        env TEXT NOT NULL DEFAULT 'none',
        folder_id TEXT REFERENCES folders(id) ON DELETE SET NULL,
        has_secret INTEGER NOT NULL DEFAULT 0,
        sort_order INTEGER NOT NULL DEFAULT 0,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE TABLE saved_queries (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        sql TEXT NOT NULL,
        connection_id TEXT REFERENCES connections(id) ON DELETE SET NULL,
        folder_id TEXT REFERENCES folders(id) ON DELETE SET NULL,
        description TEXT,
        tags_json TEXT NOT NULL DEFAULT '[]',
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE TABLE query_history (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        connection_id TEXT,
        sql TEXT NOT NULL,
        started_at INTEGER NOT NULL,
        duration_ms INTEGER NOT NULL,
        rows INTEGER,
        status TEXT NOT NULL,
        error TEXT
    );
    CREATE INDEX query_history_started ON query_history(started_at DESC);
    CREATE TABLE tabs (
        id TEXT PRIMARY KEY,
        title TEXT NOT NULL,
        sql TEXT NOT NULL,
        connection_id TEXT,
        saved_query_id TEXT,
        order_idx INTEGER NOT NULL
    );
    CREATE TABLE settings (
        key TEXT PRIMARY KEY,
        value_json TEXT NOT NULL
    );
    "#,
    // v2: AI mode + knowledge
    r#"
    ALTER TABLE connections ADD COLUMN ai_policy_json TEXT;
    ALTER TABLE saved_queries ADD COLUMN ai_example INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE query_history ADD COLUMN origin TEXT NOT NULL DEFAULT 'user';
    CREATE TABLE ai_providers (
        id TEXT PRIMARY KEY,
        kind TEXT NOT NULL,
        name TEXT NOT NULL,
        config_json TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE TABLE ai_sessions (
        id TEXT PRIMARY KEY,
        title TEXT NOT NULL,
        connection_id TEXT,
        provider_id TEXT,
        model TEXT,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE TABLE ai_messages (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        session_id TEXT NOT NULL REFERENCES ai_sessions(id) ON DELETE CASCADE,
        role TEXT NOT NULL,
        content_json TEXT NOT NULL,
        tokens_in INTEGER,
        tokens_out INTEGER,
        created_at INTEGER NOT NULL
    );
    CREATE INDEX ai_messages_session ON ai_messages(session_id, id);
    CREATE TABLE ai_audit (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        session_id TEXT,
        connection_id TEXT,
        tool TEXT NOT NULL,
        args_json TEXT NOT NULL,
        decision TEXT NOT NULL,
        summary TEXT,
        created_at INTEGER NOT NULL
    );
    CREATE TABLE kn_objects (
        connection_id TEXT NOT NULL,
        schema_name TEXT NOT NULL,
        name TEXT NOT NULL,
        kind TEXT NOT NULL,
        comment TEXT,
        row_estimate INTEGER,
        columns_json TEXT NOT NULL,
        fks_json TEXT NOT NULL,
        content_hash TEXT NOT NULL,
        indexed_at INTEGER NOT NULL,
        PRIMARY KEY (connection_id, schema_name, name)
    );
    CREATE TABLE kn_notes (
        id TEXT PRIMARY KEY,
        connection_id TEXT NOT NULL,
        target TEXT,
        body TEXT NOT NULL,
        author TEXT NOT NULL,
        status TEXT NOT NULL,
        created_at INTEGER NOT NULL
    );
    CREATE TABLE kn_state (
        connection_id TEXT PRIMARY KEY,
        indexed_at INTEGER NOT NULL,
        objects INTEGER NOT NULL,
        schemas_json TEXT NOT NULL,
        error TEXT
    );
    CREATE VIRTUAL TABLE kn_fts USING fts5(
        connection_id UNINDEXED, source UNINDEXED, ref UNINDEXED, title, body,
        tokenize = "unicode61 remove_diacritics 2 tokenchars '_'"
    );
    "#,
    // v3: notebooks
    r#"
    CREATE TABLE notebooks (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        connection_id TEXT REFERENCES connections(id) ON DELETE SET NULL,
        folder_id TEXT REFERENCES folders(id) ON DELETE SET NULL,
        cells_json TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );
    ALTER TABLE tabs ADD COLUMN notebook_id TEXT;
    "#,
    // v4: query outputs (handles, pins, snapshots) + links
    r#"
    CREATE TABLE outputs (
        result_id     TEXT PRIMARY KEY,
        handle        TEXT NOT NULL UNIQUE,
        name          TEXT,
        connection_id TEXT,
        meta_json     TEXT NOT NULL,
        snapshot_path TEXT,
        created_at    INTEGER NOT NULL
    );
    ALTER TABLE query_history ADD COLUMN output_handle TEXT;
    ALTER TABLE query_history ADD COLUMN result_id TEXT;
    ALTER TABLE tabs ADD COLUMN output_ref TEXT;
    "#,
    // v5: AI note updates point at the note they replace
    r#"
    ALTER TABLE kn_notes ADD COLUMN replaces TEXT;
    "#,
    // v6: metadata cache (tables and columns seen while browsing, for completion)
    r#"
    CREATE TABLE meta_objects (
        connection_id TEXT NOT NULL,
        schema_name   TEXT NOT NULL,
        name          TEXT NOT NULL,
        kind          TEXT NOT NULL,
        comment       TEXT,
        row_estimate  INTEGER,
        seen_at       INTEGER NOT NULL,
        PRIMARY KEY (connection_id, schema_name, name)
    );
    CREATE TABLE meta_columns (
        connection_id TEXT NOT NULL,
        schema_name   TEXT NOT NULL,
        table_name    TEXT NOT NULL,
        columns_json  TEXT NOT NULL,
        seen_at       INTEGER NOT NULL,
        PRIMARY KEY (connection_id, schema_name, table_name)
    );
    "#,
    // v7: per-schema fingerprints for incremental AI indexing
    r#"
    CREATE TABLE kn_schema_state (
        connection_id TEXT NOT NULL,
        schema_name   TEXT NOT NULL,
        fingerprint   TEXT NOT NULL,
        indexed_at    INTEGER NOT NULL,
        PRIMARY KEY (connection_id, schema_name)
    );
    "#,
    // v8: explorer cache (catalog/schema lists, complete listings, object
    // versions, stale columns) for instant/offline explorer and incremental refresh
    r#"
    CREATE TABLE meta_conn_state (
        connection_id TEXT PRIMARY KEY,
        identity      TEXT,
        catalogs_at   INTEGER,
        schemas_at    INTEGER,
        checked_at    INTEGER
    );
    CREATE TABLE meta_catalogs (
        connection_id TEXT NOT NULL,
        name          TEXT NOT NULL,
        is_default    INTEGER NOT NULL,
        pos           INTEGER NOT NULL,
        schemas_at    INTEGER,
        PRIMARY KEY (connection_id, name)
    );
    CREATE TABLE meta_schemas (
        connection_id TEXT NOT NULL,
        schema_name   TEXT NOT NULL,
        catalog       TEXT,
        is_default    INTEGER NOT NULL,
        pos           INTEGER NOT NULL,
        listed_at     INTEGER,
        listed_fp     TEXT,
        PRIMARY KEY (connection_id, schema_name)
    );
    -- A table and a function may share a name: kind joins the key.
    CREATE TABLE meta_objects_v8 (
        connection_id TEXT NOT NULL,
        schema_name   TEXT NOT NULL,
        name          TEXT NOT NULL,
        kind          TEXT NOT NULL,
        comment       TEXT,
        row_estimate  INTEGER,
        seen_at       INTEGER NOT NULL,
        version       TEXT,
        PRIMARY KEY (connection_id, schema_name, kind, name)
    );
    INSERT INTO meta_objects_v8 (connection_id, schema_name, name, kind, comment, row_estimate, seen_at)
        SELECT connection_id, schema_name, name, kind, comment, row_estimate, seen_at FROM meta_objects;
    DROP TABLE meta_objects;
    ALTER TABLE meta_objects_v8 RENAME TO meta_objects;
    CREATE INDEX meta_objects_name ON meta_objects (connection_id, name);
    ALTER TABLE meta_columns ADD COLUMN stale INTEGER NOT NULL DEFAULT 0;
    "#,
    // v9: jobs (graphs of SQL steps) and their runs
    r#"
    CREATE TABLE jobs (
        id                TEXT PRIMARY KEY,
        name              TEXT NOT NULL,
        spec_json         TEXT NOT NULL,
        last_scheduled_at INTEGER,
        created_at        INTEGER NOT NULL,
        updated_at        INTEGER NOT NULL
    );
    CREATE TABLE job_runs (
        id          INTEGER PRIMARY KEY AUTOINCREMENT,
        job_id      TEXT NOT NULL,
        trigger     TEXT NOT NULL,
        started_at  INTEGER NOT NULL,
        finished_at INTEGER,
        status      TEXT NOT NULL,
        nodes_json  TEXT NOT NULL,
        error       TEXT
    );
    CREATE INDEX job_runs_job ON job_runs (job_id, id DESC);
    "#,
    // v10: a log per job run (what ran, outputs, errors)
    r#"
    ALTER TABLE job_runs ADD COLUMN log_json TEXT;
    "#,
    // v11: manual-commit tabs
    r#"
    ALTER TABLE tabs ADD COLUMN auto_commit INTEGER;
    "#,
    // v12: tabs editing a file on disk
    r#"
    ALTER TABLE tabs ADD COLUMN file_path TEXT;
    "#,
    // v13: job leases (one runner per job across processes) and run heartbeats
    r#"
    ALTER TABLE jobs ADD COLUMN lease_owner TEXT;
    ALTER TABLE jobs ADD COLUMN lease_until INTEGER;
    ALTER TABLE job_runs ADD COLUMN runner TEXT;
    ALTER TABLE job_runs ADD COLUMN heartbeat_at INTEGER;
    "#,
    // One-time data fixes that ran (by name).
    r#"
    CREATE TABLE maintenance (name TEXT PRIMARY KEY, done_at INTEGER NOT NULL);
    "#,
    // Named sets of `:name` parameter values (tabs, notebooks, jobs).
    r#"
    CREATE TABLE param_sets (
        id          TEXT PRIMARY KEY,
        name        TEXT NOT NULL,
        values_json TEXT NOT NULL,
        updated_at  INTEGER NOT NULL
    );
    CREATE UNIQUE INDEX param_sets_name ON param_sets(lower(name));
    ALTER TABLE job_runs ADD COLUMN params_json TEXT;
    "#,
    // v16: knowledge learned from query history (joins, table usage)
    r#"
    CREATE TABLE kn_joins (
        id            INTEGER PRIMARY KEY,
        connection_id TEXT NOT NULL,
        left_table    TEXT NOT NULL,
        left_cols     TEXT NOT NULL,
        right_table   TEXT NOT NULL,
        right_cols    TEXT NOT NULL,
        status        TEXT NOT NULL DEFAULT 'learned',
        declared      INTEGER NOT NULL DEFAULT 0,
        first_seen    INTEGER NOT NULL,
        UNIQUE (connection_id, left_table, left_cols, right_table, right_cols)
    );
    CREATE TABLE kn_join_seen (
        join_id     INTEGER NOT NULL REFERENCES kn_joins(id) ON DELETE CASCADE,
        fingerprint TEXT NOT NULL,
        day         INTEGER NOT NULL,
        ai          INTEGER NOT NULL,
        kind        TEXT NOT NULL,
        history_id  INTEGER NOT NULL,
        at          INTEGER NOT NULL,
        PRIMARY KEY (join_id, fingerprint, day, ai)
    );
    CREATE TABLE kn_table_seen (
        connection_id TEXT NOT NULL,
        table_name    TEXT NOT NULL,
        fingerprint   TEXT NOT NULL,
        day           INTEGER NOT NULL,
        at            INTEGER NOT NULL,
        PRIMARY KEY (connection_id, table_name, fingerprint, day)
    );
    CREATE TABLE kn_learn_state (
        connection_id   TEXT PRIMARY KEY,
        last_history_id INTEGER NOT NULL,
        learned_at      INTEGER NOT NULL,
        error           TEXT
    );
    "#,
    // v17: joins documented by the user or the assistant (a data dictionary,
    // a spec, the user saying so), not only learned from history
    r#"
    ALTER TABLE kn_joins ADD COLUMN doc_source TEXT;
    ALTER TABLE kn_joins ADD COLUMN doc_by TEXT;
    ALTER TABLE kn_joins ADD COLUMN doc_state TEXT;
    ALTER TABLE kn_joins ADD COLUMN doc_at INTEGER;
    "#,
    // v18: tokens of AI calls outside conversations (inline completion,
    // test data), counted in the provider's usage with its conversations
    r#"
    CREATE TABLE ai_usage (
        id          INTEGER PRIMARY KEY AUTOINCREMENT,
        provider_id TEXT NOT NULL,
        feature     TEXT NOT NULL,
        tokens_in   INTEGER NOT NULL,
        tokens_out  INTEGER NOT NULL,
        created_at  INTEGER NOT NULL
    );
    CREATE INDEX ai_usage_provider ON ai_usage (provider_id, created_at);
    "#,
    // v19: foreign keys with a table's cached columns (NULL: not known)
    r#"
    ALTER TABLE meta_columns ADD COLUMN fks_json TEXT;
    "#,
    // v20: one metadata store; the AI uses only what the user's scope allows
    // (see docs/design/metadata-store.md). ai_tables is built after this
    // (Workspace::build_ai_tables), from the rules and the explorer cache.
    r#"
    CREATE TABLE ai_scope (
        connection_id TEXT NOT NULL,
        schema_rule   TEXT NOT NULL,
        table_name    TEXT NOT NULL DEFAULT '',
        allow         INTEGER NOT NULL,
        PRIMARY KEY (connection_id, schema_rule, table_name)
    );
    CREATE TABLE ai_tables (
        connection_id TEXT NOT NULL,
        schema_name   TEXT NOT NULL,
        name          TEXT NOT NULL,
        fts_rowid     INTEGER,
        body_hash     TEXT,
        PRIMARY KEY (connection_id, schema_name, name)
    );
    CREATE VIEW ai_objects AS
        SELECT o.connection_id, o.schema_name, o.name, o.kind, o.comment, o.row_estimate,
               c.columns_json, c.fks_json, c.seen_at AS described_at, c.stale
        FROM ai_tables a
        JOIN meta_objects o ON o.connection_id = a.connection_id AND o.schema_name = a.schema_name AND o.name = a.name
            AND o.kind IN ('table', 'view', 'materialized_view', 'foreign_table')
        LEFT JOIN meta_columns c ON c.connection_id = a.connection_id AND c.schema_name = a.schema_name AND c.table_name = a.name;
    -- The scope keeps exactly what the AI sees today: every schema an index
    -- run read in full (kn_state.schemas; `*` in index_schemas is carried by
    -- these, never as a rule that would also allow system schemas), and
    -- each table indexed on its own (index_tables) as a table rule.
    INSERT OR IGNORE INTO ai_scope (connection_id, schema_rule, table_name, allow)
        SELECT s.connection_id, trim(j.value), '', 1 FROM kn_state s, json_each(s.schemas_json) j
        WHERE trim(j.value) <> '';
    INSERT OR IGNORE INTO ai_scope (connection_id, schema_rule, table_name, allow)
        SELECT k.connection_id, k.schema_name, k.name, 1 FROM kn_objects k
        WHERE NOT EXISTS (SELECT 1 FROM ai_scope a WHERE a.connection_id = k.connection_id
            AND a.table_name = '' AND lower(a.schema_rule) = lower(k.schema_name));
    -- The facts: what the index knows that the explorer cache doesn't, or
    -- knows from longer ago.
    INSERT INTO meta_objects (connection_id, schema_name, name, kind, comment, row_estimate, seen_at)
        SELECT k.connection_id, k.schema_name, k.name, k.kind, k.comment, k.row_estimate, k.indexed_at FROM kn_objects k
        WHERE NOT EXISTS (SELECT 1 FROM meta_objects m WHERE m.connection_id = k.connection_id
            AND m.schema_name = k.schema_name AND m.name = k.name);
    INSERT INTO meta_columns (connection_id, schema_name, table_name, columns_json, seen_at, fks_json)
        SELECT connection_id, schema_name, name, columns_json, indexed_at, fks_json FROM kn_objects WHERE true
        ON CONFLICT (connection_id, schema_name, table_name) DO UPDATE SET columns_json = excluded.columns_json,
            seen_at = excluded.seen_at, fks_json = excluded.fks_json
        WHERE excluded.seen_at > meta_columns.seen_at;
    -- Object search rows are rebuilt from ai_objects.
    DELETE FROM kn_fts WHERE source = 'object';
    "#,
];

impl Workspace {
    /// Once: jobs saved before steps carried `JobNode::ai_sql` lost who
    /// wrote their SQL. Every step of a job the AI assistant (or an MCP
    /// client) created or changed, as the audit log records it by job name,
    /// is marked as the AI's, so its DuckDB SQL runs in the sandbox on the
    /// schedule too. Marking only restricts: a step wrongly marked (a job of
    /// yours with the same name) is cleared in its step panel. Returns the
    /// names of the jobs marked.
    pub(crate) fn mark_ai_written_jobs(&self) -> Result<Vec<String>> {
        const DONE: &str = "ai_jobs_marked_v1";
        let done: bool = self.conn.lock().query_row("SELECT count(*) > 0 FROM maintenance WHERE name = ?1", [DONE], |r| r.get(0))?;
        if done {
            return Ok(vec![]);
        }
        let names: std::collections::HashSet<String> = {
            let c = self.conn.lock();
            let mut st = c.prepare(
                "SELECT args_json FROM ai_audit WHERE tool IN ('create_job', 'update_job') AND decision IN ('approved', 'approved by MCP client', 'allowed')",
            )?;
            let rows = st.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            rows.iter()
                .filter_map(|a| serde_json::from_str::<serde_json::Value>(a).ok())
                .filter_map(|v| v.get("name").and_then(|n| n.as_str()).map(|n| n.trim().to_lowercase()))
                .filter(|n| !n.is_empty())
                .collect()
        };
        let mut marked = Vec::new();
        if !names.is_empty() {
            let ids: Vec<(String, String)> = {
                let c = self.conn.lock();
                let mut st = c.prepare("SELECT id, name FROM jobs")?;
                st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
            };
            for (id, _) in ids.into_iter().filter(|(_, n)| names.contains(&n.trim().to_lowercase())) {
                let job = self.get_job(&id)?;
                if job.nodes.iter().all(|n| n.ai_sql) {
                    continue;
                }
                self.set_ai_sql_all(&job)?;
                marked.push(job.name);
            }
        }
        self.conn.lock().execute("INSERT OR IGNORE INTO maintenance (name, done_at) VALUES (?1, ?2)", params![DONE, now_ms()])?;
        Ok(marked)
    }

    /// Once: remove passwords and keys from AI conversations kept before
    /// they were redacted on the way in (messages and titles; see
    /// [`Self::add_ai_message`]), and connection-string passwords from the
    /// audit log.
    pub(crate) fn redact_kept_transcripts(&self) -> Result<()> {
        const DONE: &str = "transcripts_redacted_v1";
        let done: bool = self.conn.lock().query_row("SELECT count(*) > 0 FROM maintenance WHERE name = ?1", [DONE], |r| r.get(0))?;
        if done {
            return Ok(());
        }
        use databrain_connector_core::sql::redact_text;
        let json_text = |s: &str| serde_json::from_str::<serde_json::Value>(s).map(|v| ai::redact_json(&v).to_string()).unwrap_or_else(|_| redact_text(s));
        {
            let c = self.conn.lock();
            let tx = c.unchecked_transaction()?;
            let rows: Vec<(i64, String)> =
                tx.prepare("SELECT id, content_json FROM ai_messages")?.query_map([], |x| Ok((x.get(0)?, x.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
            for (id, content) in rows {
                let c2 = json_text(&content);
                if c2 != content {
                    tx.execute("UPDATE ai_messages SET content_json = ?2 WHERE id = ?1", params![id, c2])?;
                }
            }
            let rows: Vec<(String, String)> =
                tx.prepare("SELECT id, title FROM ai_sessions")?.query_map([], |x| Ok((x.get(0)?, x.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
            for (id, title) in rows {
                let t2 = redact_text(&title);
                if t2 != title {
                    tx.execute("UPDATE ai_sessions SET title = ?2 WHERE id = ?1", params![id, t2])?;
                }
            }
            let rows: Vec<(i64, String, Option<String>)> = tx
                .prepare("SELECT id, args_json, summary FROM ai_audit")?
                .query_map([], |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?;
            for (id, args, summary) in rows {
                let (a2, s2) = (json_text(&args), summary.as_deref().map(redact_text));
                if a2 != args || s2 != summary {
                    tx.execute("UPDATE ai_audit SET args_json = ?2, summary = ?3 WHERE id = ?1", params![id, a2, s2])?;
                }
            }
            tx.commit()?;
        }
        self.conn.lock().execute("INSERT OR IGNORE INTO maintenance (name, done_at) VALUES (?1, ?2)", params![DONE, now_ms()])?;
        Ok(())
    }

    /// Once: remove passwords and keys from what was kept before they were
    /// redacted on the way in (history, job runs and their logs, AI audit).
    /// v2: with [`redact_text`](databrain_connector_core::sql::redact_text)
    /// (connection strings, `name=value` secrets); v1 used the SQL-only pass.
    pub(crate) fn redact_kept_sql(&self) -> Result<()> {
        const DONE: &str = "secrets_redacted_v2";
        let done: bool = self.conn.lock().query_row("SELECT count(*) > 0 FROM maintenance WHERE name = ?1", [DONE], |r| r.get(0))?;
        if done {
            return Ok(());
        }
        use databrain_connector_core::sql::redact_text as r;
        fn json(v: &serde_json::Value) -> serde_json::Value {
            match v {
                serde_json::Value::String(s) => serde_json::Value::String(r(s)),
                serde_json::Value::Array(a) => serde_json::Value::Array(a.iter().map(json).collect()),
                serde_json::Value::Object(o) => serde_json::Value::Object(o.iter().map(|(k, v)| (k.clone(), json(v))).collect()),
                other => other.clone(),
            }
        }
        let text = |s: Option<String>| s.map(|s| r(&s));
        let json_text = |s: Option<String>| s.map(|s| serde_json::from_str::<serde_json::Value>(&s).map(|v| json(&v).to_string()).unwrap_or_else(|_| r(&s)));
        {
            let c = self.conn.lock();
            let tx = c.unchecked_transaction()?;
            let rows: Vec<(i64, String, Option<String>)> = tx
                .prepare("SELECT id, sql, error FROM query_history")?
                .query_map([], |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?;
            for (id, sql, error) in rows {
                let (s2, e2) = (r(&sql), text(error.clone()));
                if s2 != sql || e2 != error {
                    tx.execute("UPDATE query_history SET sql = ?2, error = ?3 WHERE id = ?1", params![id, s2, e2])?;
                }
            }
            let rows: Vec<(i64, String, Option<String>)> = tx
                .prepare("SELECT id, args_json, summary FROM ai_audit")?
                .query_map([], |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?;
            for (id, args, summary) in rows {
                let (a2, s2) = (json_text(Some(args.clone())), text(summary.clone()));
                if a2.as_deref() != Some(args.as_str()) || s2 != summary {
                    tx.execute("UPDATE ai_audit SET args_json = ?2, summary = ?3 WHERE id = ?1", params![id, a2, s2])?;
                }
            }
            type Run = (i64, Option<String>, Option<String>, Option<String>);
            let rows: Vec<Run> = tx
                .prepare("SELECT id, nodes_json, error, log_json FROM job_runs")?
                .query_map([], |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?, x.get(3)?)))?
                .collect::<rusqlite::Result<_>>()?;
            for (id, nodes, error, log) in rows {
                let (n2, e2, l2) = (json_text(nodes.clone()), text(error.clone()), json_text(log.clone()));
                if n2 != nodes || e2 != error || l2 != log {
                    tx.execute("UPDATE job_runs SET nodes_json = ?2, error = ?3, log_json = ?4 WHERE id = ?1", params![id, n2, e2, l2])?;
                }
            }
            tx.commit()?;
        }
        self.conn.lock().execute("INSERT OR IGNORE INTO maintenance (name, done_at) VALUES (?1, ?2)", params![DONE, now_ms()])?;
        Ok(())
    }
}
