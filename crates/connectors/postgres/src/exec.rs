use databrain_connector_core::value::Column;
use databrain_connector_core::{BatchBuilder, ColType, ConnectorError, ConnectorKind, ExecOptions, ExecSummary, Result, StreamEvent, StreamSender, Value};
use futures::StreamExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio_postgres::SimpleQueryMessage;

use crate::connect::Link;
use crate::error::map_err;
use crate::types::{col_type, parse_text, type_name};
use crate::wkb;

pub(crate) async fn run(link: Arc<Link>, sql: String, opts: ExecOptions, tx: StreamSender, finished: Arc<AtomicBool>) -> Result<()> {
    // The statement's text says what it does to the transaction before it
    // runs; one that failed may not have done it (`BEGIN ISOLATION LEVEL
    // bogus`, `COMMIT AND CHAIN` outside a transaction or after a failed
    // deferred constraint): then the server is asked.
    link.state.observe(&sql, ConnectorKind::Postgres);
    let res = run_statement(&link, sql.clone(), opts, tx, finished).await;
    if res.is_err() && names_transaction(&sql) {
        let client = link.client();
        if let Some(open) = server_in_transaction(&client).await {
            link.state.set_depth(u32::from(open));
        }
    }
    res
}

/// The statement may begin or end a transaction: it has one of the words
/// (in a comment too: then only a needless question to the server).
pub(crate) fn names_transaction(sql: &str) -> bool {
    sql.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|w| ["BEGIN", "START", "COMMIT", "ROLLBACK", "END", "ABORT", "PREPARE", "CHAIN"].iter().any(|k| w.eq_ignore_ascii_case(k)))
}

/// Whether a transaction is open, asked of the server: a savepoint can only
/// be set inside one (released at once), and inside one that failed the
/// server refuses it with its own code. None when the answer is neither
/// (Redshift, a lost connection): the statement's text stays the guess.
/// Open the cursor [`fetch_cursor`] reads, for `sql` when it is one read
/// (`SELECT`, `WITH`, `VALUES`, `TABLE`) a cursor can hold. Under a
/// savepoint: a statement a cursor can't hold (a CTE that writes) fails
/// without aborting the transaction, and is then run as it is (false).
async fn declare_cursor(client: &tokio_postgres::Client, sql: &str) -> bool {
    use databrain_connector_core::sql::{StatementKind, classify};
    if classify(sql, ConnectorKind::Postgres).kind != StatementKind::Read {
        return false;
    }
    let body = sql.trim().trim_end_matches(';').trim_end();
    if client.simple_query("SAVEPOINT databrain_cursor").await.is_err() {
        return false;
    }
    // A line break before the end: a trailing `-- comment` stays a comment.
    let declared = client.simple_query(&format!("DECLARE {CURSOR} NO SCROLL CURSOR FOR {body}\n")).await.is_ok();
    if !declared {
        let _ = client.simple_query("ROLLBACK TO SAVEPOINT databrain_cursor").await;
    }
    let _ = client.simple_query("RELEASE SAVEPOINT databrain_cursor").await;
    declared
}

/// The cursor a read cut by the row limit inside a transaction is fetched through.
const CURSOR: &str = "databrain_rows";

/// Fetch the rows a read keeps (up to `max`) from [`CURSOR`], then close it.
#[allow(clippy::too_many_arguments)]
async fn fetch_cursor(
    client: &tokio_postgres::Client,
    max: usize,
    types: &[ColType],
    geo: &[bool],
    builder: &mut BatchBuilder,
    opts: &ExecOptions,
    tx: &StreamSender,
    finished: &AtomicBool,
) -> Result<()> {
    let mut rows = 0usize;
    let mut res = Ok(());
    'fetch: while rows < max && !opts.cancel.is_cancelled() {
        let n = (max - rows).min(opts.batch_size.max(1));
        let msgs = match client.simple_query(&format!("FETCH FORWARD {n} FROM {CURSOR}")).await {
            Ok(m) => m,
            Err(e) => {
                res = Err(map_err(&e));
                break;
            }
        };
        let mut got = 0usize;
        for m in msgs {
            if let SimpleQueryMessage::Row(row) = m {
                builder.push_row(types.iter().enumerate().map(|(i, t)| match row.get(i) {
                    None => Value::Null,
                    Some(s) if geo[i] => wkb::hex_ewkb_to_ewkt(s).map(Value::Text).unwrap_or_else(|| Value::Text(s.to_string())),
                    Some(s) => parse_text(*t, s),
                }));
                got += 1;
                if builder.is_full() {
                    match builder.finish() {
                        Ok(b) => {
                            if !tx.send(StreamEvent::Batch(b)).await {
                                break 'fetch;
                            }
                        }
                        Err(e) => {
                            res = Err(e);
                            break 'fetch;
                        }
                    }
                }
            }
        }
        rows += got;
        if got < n {
            break;
        }
    }
    finished.store(true, Ordering::SeqCst);
    // An error aborted the transaction: no cursor is left to close.
    if res.is_ok() {
        let _ = client.simple_query(&format!("CLOSE {CURSOR}")).await;
    }
    res?;
    if opts.cancel.is_cancelled() {
        return Err(ConnectorError::cancelled());
    }
    if !builder.is_empty() && !tx.send(StreamEvent::Batch(builder.finish()?)).await {
        return Ok(());
    }
    for n in builder.notices(Some("infinity dates")) {
        tx.send(StreamEvent::Notice(n)).await;
    }
    tx.send(StreamEvent::Done(ExecSummary::default())).await;
    Ok(())
}

pub(crate) async fn server_in_transaction(client: &tokio_postgres::Client) -> Option<bool> {
    use tokio_postgres::error::SqlState;
    match client.simple_query("SAVEPOINT databrain_tx_probe").await {
        Ok(_) => {
            let _ = client.simple_query("RELEASE SAVEPOINT databrain_tx_probe").await;
            Some(true)
        }
        Err(e) => match e.code() {
            Some(c) if *c == SqlState::NO_ACTIVE_SQL_TRANSACTION => Some(false),
            Some(c) if *c == SqlState::IN_FAILED_SQL_TRANSACTION => Some(true),
            _ => None,
        },
    }
}

async fn run_statement(link: &Arc<Link>, sql: String, opts: ExecOptions, tx: StreamSender, finished: Arc<AtomicBool>) -> Result<()> {
    let client = link.client();
    let stmt = client.prepare(&sql).await.map_err(|e| map_err(&e))?;

    if stmt.columns().is_empty() {
        let msgs = client.simple_query(&sql).await.map_err(|e| map_err(&e))?;
        finished.store(true, Ordering::SeqCst);
        let affected = msgs.iter().rev().find_map(|m| match m {
            SimpleQueryMessage::CommandComplete(n) => Some(*n),
            _ => None,
        });
        tx.send(StreamEvent::Done(ExecSummary { rows_affected: affected })).await;
        return Ok(());
    }

    let columns: Vec<Column> = stmt.columns().iter().map(|c| Column::new(c.name(), col_type(c.type_()), type_name(c.type_(), c.type_modifier()))).collect();
    let types: Vec<ColType> = columns.iter().map(|c| c.col_type).collect();
    // PostGIS: hex EWKB becomes EWKT.
    let geo: Vec<bool> = stmt.columns().iter().map(|c| matches!(c.type_().name(), "geometry" | "geography")).collect();
    drop(stmt);

    let batch_size = opts.batch_size.max(1);
    let mut builder = BatchBuilder::new(&columns, batch_size);
    if !tx.send(StreamEvent::Schema(builder.schema())).await {
        return Ok(());
    }

    // A read cut by the row limit inside a transaction: through a cursor,
    // fetching only the rows it keeps (otherwise the rest of the result is
    // read to its end: a cancel would abort the transaction, and a new
    // connection would lose it).
    if let Some(max) = opts.max_rows
        && link.state.in_transaction()
        && declare_cursor(&client, &sql).await
    {
        return fetch_cursor(&client, max, &types, &geo, &mut builder, &opts, &tx, &finished).await;
    }

    let stream = client.simple_query_raw(&sql).await.map_err(|e| map_err(&e))?;
    futures::pin_mut!(stream);
    let mut seen_first_set = false;
    let max = opts.max_rows.unwrap_or(usize::MAX);
    let mut rows = 0usize;
    // Stopped before the end: the row limit, or the consumer went away.
    let mut stopped = false;
    while let Some(msg) = stream.next().await {
        match msg.map_err(|e| map_err(&e))? {
            SimpleQueryMessage::RowDescription(_) => {
                if seen_first_set {
                    // Only the first result set is shown.
                    break;
                }
                seen_first_set = true;
            }
            SimpleQueryMessage::Row(row) => {
                let vals = types.iter().enumerate().map(|(i, t)| match row.get(i) {
                    None => Value::Null,
                    Some(s) if geo[i] => wkb::hex_ewkb_to_ewkt(s).map(Value::Text).unwrap_or_else(|| Value::Text(s.to_string())),
                    Some(s) => parse_text(*t, s),
                });
                builder.push_row(vals);
                rows += 1;
                if rows >= max {
                    stopped = true;
                    break;
                }
                if builder.is_full() && !tx.send(StreamEvent::Batch(builder.finish()?)).await {
                    stopped = true;
                    break;
                }
            }
            SimpleQueryMessage::CommandComplete(_) => {}
            _ => {}
        }
        // Stop: `execute` asks the server to cancel, and the cancellation
        // error ends this loop. Returning here instead (a row was ready when
        // Stop came) left the query running on the server and the
        // connection reading the rest before the next statement.
        if opts.cancel.is_cancelled() && !builder.is_empty() {
            let _ = builder.finish();
        }
    }
    if opts.cancel.is_cancelled() {
        return Err(ConnectorError::cancelled());
    }
    if stopped {
        // The rest of the result is still coming, and the connection reads
        // it all before the next statement. A clean connection is replaced
        // instead; one with a transaction or settings is kept.
        finished.store(true, Ordering::SeqCst);
        let replaced = link.state.replaceable() && link.replace(&client).await;
        if !replaced {
            if !link.state.in_transaction() {
                // Outside a transaction a cancel loses nothing; inside one
                // it would abort the transaction, so the rest is read.
                let _ = client.cancel_token().cancel_query(link.tls.clone()).await;
            }
            while let Some(msg) = stream.next().await {
                if let Err(e) = msg {
                    if map_err(&e).is_cancelled() {
                        break;
                    }
                    return Err(map_err(&e));
                }
            }
        }
    }
    finished.store(true, Ordering::SeqCst);
    if !builder.is_empty() && !tx.send(StreamEvent::Batch(builder.finish()?)).await {
        return Ok(());
    }
    for n in builder.notices(Some("infinity dates")) {
        tx.send(StreamEvent::Notice(n)).await;
    }
    tx.send(StreamEvent::Done(ExecSummary::default())).await;
    Ok(())
}
