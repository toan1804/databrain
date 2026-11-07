//! Statement execution: streaming results, cancel and connection replacement.

use crate::error::map_err;
use crate::session::MysqlSession;
use crate::types::col_type;
use crate::types::convert;
use crate::types::type_name;
use databrain_connector_core::BatchBuilder;
use databrain_connector_core::ColType;
use databrain_connector_core::ConnState;
use databrain_connector_core::ConnectorError;
use databrain_connector_core::ConnectorKind;
use databrain_connector_core::ExecOptions;
use databrain_connector_core::ExecSummary;
use databrain_connector_core::Result;
use databrain_connector_core::StreamEvent;
use databrain_connector_core::StreamSender;
use databrain_connector_core::value::Column;
use mysql_async::Conn;
use mysql_async::Opts;
use mysql_async::consts::StatusFlags;
use mysql_async::prelude::*;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::Mutex;

/// What `run` needs besides the statement.
pub(crate) struct Ctx {
    pub(crate) conn: Arc<Mutex<Conn>>,
    /// Thread id of the current connection (for KILL).
    pub(crate) conn_id: Arc<AtomicU32>,
    pub(crate) opts: Opts,
    pub(crate) state: Arc<ConnState>,
    /// Set once the statement no longer needs the server: a late cancel must
    /// not KILL whatever runs next on the connection.
    pub(crate) finished: Arc<AtomicBool>,
    pub(crate) read_only: bool,
}

pub(crate) async fn run(ctx: &Ctx, sql: String, opts: ExecOptions, tx: StreamSender) -> Result<()> {
    let mut guard = ctx.conn.lock().await;
    ctx.state.observe(&sql, ConnectorKind::Mysql);
    // An open transaction counts too, also one a procedure started.
    let in_tx = guard.last_ok_packet().is_some_and(|p| p.status_flags().contains(StatusFlags::SERVER_STATUS_IN_TRANS));
    let replaceable = ctx.state.replaceable() && !in_tx;
    if ctx.read_only && !in_tx {
        // Applies to the next transaction, which the statement is (it can't
        // be changed while that transaction runs).
        guard.query_drop("SET SESSION TRANSACTION READ ONLY").await.map_err(map_err)?;
    }
    let mut result = guard.query_iter(sql).await.map_err(map_err)?;

    let columns = result.columns().unwrap_or_else(|| Arc::from(Vec::new()));
    if columns.is_empty() {
        let affected = result.affected_rows();
        result.drop_result().await.map_err(map_err)?;
        ctx.finished.store(true, Ordering::SeqCst);
        tx.send(StreamEvent::Done(ExecSummary { rows_affected: Some(affected) })).await;
        return Ok(());
    }

    let cols: Vec<Column> = columns.iter().map(|c| Column::new(c.name_str().into_owned(), col_type(c), type_name(c))).collect();
    let types: Vec<ColType> = cols.iter().map(|c| c.col_type).collect();
    let batch_size = opts.batch_size.max(1);
    let mut builder = BatchBuilder::new(&cols, batch_size);
    if !tx.send(StreamEvent::Schema(builder.schema())).await {
        return Ok(());
    }

    let max = opts.max_rows.unwrap_or(usize::MAX);
    let mut rows = 0usize;
    // Stopped before the end: the row limit, or the consumer went away.
    let mut stopped = false;
    while let Some(row) = result.next().await.map_err(map_err)? {
        let vals = row.unwrap();
        builder.push_row(vals.into_iter().zip(types.iter()).map(|(v, t)| convert(*t, v)));
        rows += 1;
        if rows >= max {
            stopped = true;
            break;
        }
        if builder.is_full() && !tx.send(StreamEvent::Batch(builder.finish()?)).await {
            stopped = true;
            break;
        }
        if opts.cancel.is_cancelled() {
            return Err(ConnectorError::cancelled());
        }
    }
    if stopped {
        // The rest of the result is still coming. Reading it takes as long
        // as the whole result; a clean connection is replaced instead.
        drop(result);
        ctx.finished.store(true, Ordering::SeqCst);
        let replaced = replaceable && replace_conn(ctx, &mut guard).await;
        if !replaced {
            // Keep the connection (transaction, settings): stop the
            // statement on the server, then read what is left.
            MysqlSession::kill_query(ctx.opts.clone(), ctx.conn_id.load(Ordering::SeqCst)).await;
            settle_kill(&mut guard).await;
        }
    } else {
        // Drain any additional result sets (e.g. from CALL) so the connection
        // is ready for the next statement.
        result.drop_result().await.map_err(map_err)?;
        ctx.finished.store(true, Ordering::SeqCst);
    }
    drop(guard);
    if !builder.is_empty() && !tx.send(StreamEvent::Batch(builder.finish()?)).await {
        return Ok(());
    }
    if builder.coercion_failures() > 0 {
        tx.send(StreamEvent::Notice(format!("{} value(s) could not be converted (e.g. zero dates) and are shown as NULL", builder.coercion_failures()))).await;
    }
    tx.send(StreamEvent::Done(ExecSummary::default())).await;
    Ok(())
}

/// Open a new connection in place of `conn`, which is still receiving a
/// result, and end the old one on the server (`KILL` from the new
/// connection: no extra login). False when no new connection could be
/// opened; `conn` is then unchanged.
pub(crate) async fn replace_conn(ctx: &Ctx, conn: &mut Conn) -> bool {
    let Ok(Ok(mut fresh)) = tokio::time::timeout(Duration::from_secs(5), Conn::new(ctx.opts.clone())).await else {
        return false;
    };
    let old_id = conn.id();
    let _ = fresh.query_drop(format!("KILL {old_id}")).await;
    ctx.conn_id.store(fresh.id(), Ordering::SeqCst);
    // Dropping the old connection reads its remains in the background
    // until the server's close.
    drop(std::mem::replace(conn, fresh));
    ctx.state.reset();
    true
}

/// After `KILL QUERY`: read the rest of the interrupted result. If the
/// statement had already finished, the kill is still pending on the
/// connection and would stop the next statement: let a no-op take it.
pub(crate) async fn settle_kill(conn: &mut Conn) {
    let interrupted = |e: &mysql_async::Error| matches!(e, mysql_async::Error::Server(se) if se.code == 1317);
    // The dropped result is read before the next command.
    match conn.query_drop("DO 0").await {
        Err(e) if interrupted(&e) => {
            // Either the statement or this no-op took the kill; one more
            // no-op tells (a second 1317 can't happen).
            let _ = conn.query_drop("DO 0").await;
        }
        _ => {}
    }
}
