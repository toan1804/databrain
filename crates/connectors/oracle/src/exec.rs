//! Statement execution on both drivers: streaming, cancellation and blocking calls.

use crate::agent;
use crate::error::map_err;
use crate::types::col_type;
use crate::types::convert;
use crate::types::db_type;
use databrain_connector_core::BatchBuilder;
use databrain_connector_core::ColType;
use databrain_connector_core::ConnectorError;
use databrain_connector_core::ExecOptions;
use databrain_connector_core::ExecSummary;
use databrain_connector_core::QueryStream;
use databrain_connector_core::Result;
use databrain_connector_core::StreamEvent;
use databrain_connector_core::StreamSender;
use databrain_connector_core::value::Column;
use oracle::Connection;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

#[derive(Default)]
pub(crate) struct CallState {
    /// The caller stopped waiting.
    pub(crate) abandoned: AtomicBool,
    /// The statement holds the connection now.
    pub(crate) running: AtomicBool,
}

pub(crate) struct BreakOnDrop {
    pub(crate) conn: Arc<Connection>,
    pub(crate) state: Arc<CallState>,
    pub(crate) armed: bool,
}

impl Drop for BreakOnDrop {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.state.abandoned.store(true, Ordering::SeqCst);
        if self.state.running.load(Ordering::SeqCst) {
            let _ = self.conn.break_execution();
        }
    }
}

pub(crate) async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f).await.map_err(|e| ConnectorError::internal(e.to_string()))?
}

/// SQL as Oracle accepts it (no trailing `;`, except for PL/SQL blocks) and
/// whether it returns rows.
pub(crate) fn prepare_sql(sql: &str) -> (&str, bool) {
    let trimmed = sql.trim_end();
    let kw = databrain_connector_core::sql::leading_keyword(trimmed);
    let plsql_like = kw == "BEGIN" || kw == "DECLARE" || (kw == "CREATE" && trimmed.to_ascii_uppercase().contains(" END"));
    let sql = if !plsql_like { trimmed.trim_end_matches(';') } else { trimmed };
    (sql, kw == "SELECT" || kw == "WITH")
}

pub(crate) fn run(conn: &Connection, sql: &str, opts: &ExecOptions, tx: &StreamSender) -> Result<()> {
    // Oracle rejects a trailing semicolon in SQL (but PL/SQL blocks need theirs).
    let (sql, _) = prepare_sql(sql);
    let batch = opts.batch_size.clamp(1, 10_000);
    let mut stmt = conn.statement(sql).fetch_array_size(batch as u32).prefetch_rows(batch as u32 + 1).build().map_err(map_err)?;
    if !stmt.is_query() {
        stmt.execute(&[]).map_err(map_err)?;
        let n = stmt.row_count().ok();
        tx.blocking_send(Ok(StreamEvent::Done(ExecSummary { rows_affected: n })));
        return Ok(());
    }
    let rows = stmt.query(&[]).map_err(map_err)?;
    let cols: Vec<Column> = rows.column_info().iter().map(|c| Column::new(c.name(), col_type(c.oracle_type()), db_type(c.oracle_type()))).collect();
    let types: Vec<ColType> = cols.iter().map(|c| c.col_type).collect();
    let mut b = BatchBuilder::new(&cols, batch);
    if !tx.blocking_send(Ok(StreamEvent::Schema(b.schema()))) {
        return Ok(());
    }
    for (n, row) in rows.enumerate() {
        if opts.max_rows.is_some_and(|m| n >= m) {
            break; // the caller keeps no more
        }
        let row = row.map_err(map_err)?;
        b.push_row(row.sql_values().iter().zip(&types).map(|(v, t)| convert(*t, v)));
        if b.is_full() && !tx.blocking_send(Ok(StreamEvent::Batch(b.finish()?))) {
            return Ok(());
        }
        if opts.cancel.is_cancelled() {
            return Err(ConnectorError::cancelled());
        }
    }
    if !b.is_empty() && !tx.blocking_send(Ok(StreamEvent::Batch(b.finish()?))) {
        return Ok(());
    }
    tx.blocking_send(Ok(StreamEvent::Done(ExecSummary::default())));
    Ok(())
}

/// Run a statement through the thin driver, streaming batches as they arrive.
pub(crate) fn thin_execute(a: Arc<agent::Agent>, sql: &str, opts: ExecOptions) -> QueryStream {
    let (tx, stream) = QueryStream::channel(4);
    let (sql, is_query) = prepare_sql(sql);
    let sql = sql.to_string();
    tokio::spawn(async move {
        if let Err(e) = thin_run(&a, &sql, is_query, &opts, &tx).await {
            let e = if opts.cancel.is_cancelled() { ConnectorError::cancelled() } else { e };
            tx.send_err(e).await;
        }
    });
    stream
}

pub(crate) async fn thin_run(a: &agent::Agent, sql: &str, is_query: bool, opts: &ExecOptions, tx: &StreamSender) -> Result<()> {
    use agent::Frame;
    let batch = opts.batch_size.clamp(1, 10_000);
    let req = serde_json::json!({
        "op": if is_query { "query" } else { "exec" },
        "sql": sql,
        "batch_size": batch,
        "max_rows": opts.max_rows.unwrap_or(0),
    });
    let (id, mut r) = a.start(req).await?;
    let mut builder: Option<(BatchBuilder, usize)> = None;
    let mut cancelled = false;
    // Stop asked (Stop button / row cap / consumer gone): tell the helper,
    // then keep reading until it ends the request.
    loop {
        let frame = tokio::select! {
            f = agent::Agent::next(&mut r, id) => f?,
            _ = opts.cancel.cancelled(), if !cancelled => {
                cancelled = true;
                a.cancel(id).await;
                continue;
            }
        };
        match frame {
            Frame::Json(j) => match j["type"].as_str() {
                Some("schema") => {
                    let cols: Vec<Column> = j["columns"]
                        .as_array()
                        .map(|c| {
                            c.iter()
                                .map(|c| {
                                    let t = match c["type"].as_str() {
                                        Some("int64") => ColType::Int64,
                                        Some("float64") => ColType::Float64,
                                        Some("timestamp") => ColType::Timestamp,
                                        Some("timestamptz") => ColType::TimestampTz,
                                        Some("binary") => ColType::Binary,
                                        Some("bool") => ColType::Bool,
                                        _ => ColType::Utf8,
                                    };
                                    Column::new(c["name"].as_str().unwrap_or(""), t, c["db_type"].as_str().unwrap_or("").to_string())
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    let b = BatchBuilder::new(&cols, batch);
                    if !cancelled && !tx.send(StreamEvent::Schema(b.schema())).await {
                        cancelled = true;
                        a.cancel(id).await;
                    }
                    builder = Some((b, cols.len()));
                }
                Some("done") => {
                    if cancelled {
                        return Err(ConnectorError::cancelled());
                    }
                    if let Some((b, _)) = builder.as_mut()
                        && !b.is_empty()
                    {
                        tx.send(StreamEvent::Batch(b.finish()?)).await;
                    }
                    let rows_affected = j["rows_affected"].as_u64();
                    tx.send(StreamEvent::Done(ExecSummary { rows_affected })).await;
                    return Ok(());
                }
                Some("error") => return Err(agent::reply_error(&j)),
                _ => {}
            },
            Frame::Batch { ncols, nrows, data, .. } => {
                if cancelled {
                    continue;
                }
                let Some((b, n)) = builder.as_mut() else { continue };
                if *n != ncols {
                    return Err(ConnectorError::internal("Oracle driver sent a batch with the wrong column count"));
                }
                agent::decode_rows(&data, ncols, nrows, |row| b.push_row(row))?;
                if b.is_full() && !tx.send(StreamEvent::Batch(b.finish()?)).await {
                    cancelled = true;
                    a.cancel(id).await;
                }
            }
        }
    }
}

pub(crate) fn tokio_util_token() -> databrain_connector_core::CancellationToken {
    databrain_connector_core::CancellationToken::new()
}
