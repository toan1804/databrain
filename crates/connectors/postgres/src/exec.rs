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
    link.state.observe(&sql, ConnectorKind::Postgres);
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
        if opts.cancel.is_cancelled() {
            return Err(ConnectorError::cancelled());
        }
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
