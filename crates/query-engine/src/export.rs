//! Re-running a query straight into a file (full export).

use super::*;

/// Exports whose files "Show file" can still find.
pub(crate) const KEPT_EXPORTS: usize = 100;

impl QueryEngine {
    /// Run one read statement again without a row limit and stream every
    /// row straight into `path` (nothing is kept in memory, except for XLSX).
    /// Runs on its own session, except when `session_key` (the session the
    /// statement first ran on) has an open transaction: then on that one,
    /// so the export sees the same uncommitted rows as the grid did (the tab
    /// is busy meanwhile). Written to a temp file, renamed when done.
    /// Cancel with [`QueryEngine::cancel_export`]. Returns rows written.
    ///
    /// `progress` is called when the phase changes and, while rows are
    /// written, at most every [`EXPORT_PROGRESS_EVERY`].
    #[allow(clippy::too_many_arguments)]
    pub async fn export_query(
        &self,
        export_id: &str,
        connection_id: &str,
        session_key: Option<&str>,
        sql: &str,
        path: &std::path::Path,
        opts: databrain_export::ExportOptions,
        progress: &(dyn Fn(ExportProgress) + Send + Sync),
    ) -> Result<u64> {
        let profile = self.workspace.get_connection(connection_id)?;
        let kind = profile.config.kind;
        if split_statements(sql, kind).len() != 1 {
            return Err(EngineError::new("invalid", "Only a single statement can be exported again"));
        }
        let c = classify(sql, kind);
        if !c.kind.is_read() {
            return Err(EngineError::new("invalid", format!("Only queries that read data are run again for an export ({} would run again)", c.keyword)));
        }
        let cancel = CancellationToken::new();
        self.exports.lock().insert(export_id.to_string(), cancel.clone());
        // The original session, when its transaction holds rows others can't see.
        let shared = session_key.filter(|k| self.sessions.lock().get(&(connection_id.to_string(), k.to_string())).is_some_and(|s| s.session.in_transaction()));
        let tab = match shared {
            Some(k) => {
                if self.jobs.lock().values().any(|j| j.session_key == k) {
                    self.exports.lock().remove(export_id);
                    return Err(EngineError::new("busy", "A query is running in this tab: wait for it, or commit, to export everything"));
                }
                // Busy like a running query until the export ends.
                self.jobs.lock().insert(
                    format!("export:{export_id}"),
                    JobHandle { tab_id: k.to_string(), session_key: k.to_string(), cancel: cancel.clone(), tab_closed: false },
                );
                k.to_string()
            }
            None => format!("__export__:{export_id}"),
        };
        let out = self.export_stream(connection_id, &tab, sql, path, opts, &cancel, progress).await;
        self.exports.lock().remove(export_id);
        if shared.is_some() {
            self.jobs.lock().remove(&format!("export:{export_id}"));
        } else {
            self.drop_session(connection_id, &tab);
        }
        let rows = out?;
        {
            let mut e = self.exported.lock();
            e.retain(|(id, _)| id != export_id);
            e.push_back((export_id.to_string(), path.to_path_buf()));
            while e.len() > KEPT_EXPORTS {
                e.pop_front();
            }
        }
        Ok(rows)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn export_stream(
        &self,
        connection_id: &str,
        tab: &str,
        sql: &str,
        path: &std::path::Path,
        opts: databrain_export::ExportOptions,
        cancel: &CancellationToken,
        progress: &(dyn Fn(ExportProgress) + Send + Sync),
    ) -> Result<u64> {
        // Size of the temp file being written (the writer renames it at the end).
        let report = |phase: ExportPhase, rows: u64, w: Option<&databrain_export::FileWriter>| {
            let bytes = w.and_then(|w| std::fs::metadata(w.temp_path()).ok()).map(|m| m.len()).unwrap_or(0);
            progress(ExportProgress { phase, rows, bytes });
        };
        report(ExportPhase::Connecting, 0, None);
        let session = tokio::select! {
            s = self.session(connection_id, tab) => s?,
            _ = cancel.cancelled() => return Err(ConnectorError::cancelled().into()),
        };
        report(ExportPhase::Running, 0, None);
        let mut stream = session.execute(sql, ExecOptions { batch_size: 10_000, cancel: cancel.clone(), max_rows: None, output_masks: None }).await?;
        let mut writer: Option<databrain_export::FileWriter> = None;
        let fail = |e: databrain_export::ExportError| EngineError::new("invalid", e.to_string());
        let mut rows = 0u64;
        let mut phase = ExportPhase::Running;
        let mut last = Instant::now();
        loop {
            let ev = tokio::select! {
                ev = stream.next() => ev,
                _ = cancel.cancelled() => {
                    // Wait for the connector to finish cancelling before the
                    // session is used again (it may be the tab's own, in a
                    // transaction): a late server cancel could hit the
                    // tab's next statement. As `run_statement` does.
                    let _ = tokio::time::timeout(std::time::Duration::from_secs(15), async { while stream.next().await.is_some() {} }).await;
                    return Err(ConnectorError::cancelled().into());
                }
            };
            match ev {
                None => return Err(EngineError::new("internal", "result stream ended unexpectedly")),
                Some(Ok(StreamEvent::Schema(schema))) => writer = Some(databrain_export::FileWriter::create(path, schema, opts.clone()).map_err(fail)?),
                Some(Ok(StreamEvent::Batch(b))) => {
                    if let Some(w) = writer.as_mut() {
                        w.write(&b).map_err(fail)?;
                        rows += b.num_rows() as u64;
                        if phase != ExportPhase::Writing || last.elapsed() >= EXPORT_PROGRESS_EVERY {
                            phase = ExportPhase::Writing;
                            last = Instant::now();
                            report(phase, rows, Some(w));
                        }
                    }
                }
                Some(Ok(StreamEvent::Downloading(_))) => {
                    phase = ExportPhase::Downloading;
                    report(phase, rows, writer.as_ref());
                }
                Some(Ok(StreamEvent::Notice(_))) => {}
                Some(Ok(StreamEvent::Done(_))) => {
                    return match writer {
                        Some(w) => {
                            report(ExportPhase::Finishing, rows, Some(&w));
                            w.finish().map_err(fail)
                        }
                        None => Err(EngineError::new("invalid", "The query returned no rows to export")),
                    };
                }
                Some(Err(e)) => return Err(e.into()),
            }
        }
    }

    pub fn cancel_export(&self, export_id: &str) -> bool {
        self.exports.lock().get(export_id).map(|t| t.cancel()).is_some()
    }

    /// File written by a finished full export (to show it in Finder/Explorer).
    pub fn exported_file(&self, export_id: &str) -> Option<std::path::PathBuf> {
        self.exported.lock().iter().find(|(id, _)| id == export_id).map(|(_, p)| p.clone())
    }
}
