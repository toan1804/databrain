//! Statement execution: streaming results and row limits.

use crate::config::Conn;
use crate::error::map_err;
use crate::types::col_type;
use crate::types::convert;
use databrain_connector_core::BatchBuilder;
use databrain_connector_core::ConnectorKind;
use databrain_connector_core::ExecOptions;
use databrain_connector_core::ExecSummary;
use databrain_connector_core::Result;
use databrain_connector_core::StreamEvent;
use databrain_connector_core::StreamSender;
use databrain_connector_core::sql::leading_keyword;
use databrain_connector_core::value::Column;
use futures::StreamExt;
use tiberius::QueryItem;

pub(crate) fn is_dml(sql: &str) -> bool {
    let kw = leading_keyword(sql);
    let writes = matches!(
        kw.as_str(),
        // DDL (CREATE/ALTER/DROP) runs as a plain batch: CREATE PROCEDURE,
        // VIEW, FUNCTION and TRIGGER must start a batch, which an RPC
        // (sp_executesql) call is not.
        "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "TRUNCATE" | "GRANT" | "REVOKE"
    );
    writes && !sql.split(|c: char| !c.is_ascii_alphanumeric()).any(|w| w.eq_ignore_ascii_case("OUTPUT"))
}

/// How `run` ended.
pub(crate) enum Ended {
    /// The result was read to the end (or the statement returned none).
    Complete,
    /// The row limit was reached (`last` still to send) or the consumer went
    /// away. With `unread`, the rest of the result was not read: the
    /// connection must not be used again (tiberius would read it first).
    Stopped { last: Option<databrain_connector_core::arrow::record_batch::RecordBatch>, unread: bool },
}

/// Run `sql`. A plain read stops on the server at the row limit (`SET
/// ROWCOUNT`, reset right after; not when the user set ROWCOUNT). Other
/// statements stop reading at the limit: `replaceable` (the connection
/// carries no session state) at once, otherwise after reading the rest so
/// the connection, and its transaction or settings, stay usable.
pub(crate) async fn run(conn: &mut Conn, sql: &str, opts: &ExecOptions, tx: &StreamSender, replaceable: bool, user_rowcount: bool) -> Result<Ended> {
    if is_dml(sql) {
        let r = conn.execute(sql, &[]).await.map_err(map_err)?;
        tx.send(StreamEvent::Done(ExecSummary { rows_affected: Some(r.rows_affected().iter().sum()) })).await;
        return Ok(Ended::Complete);
    }
    let server_cap = opts.max_rows.filter(|_| !user_rowcount && databrain_connector_core::sql::classify(sql, ConnectorKind::Mssql).kind.is_read());
    let Some(n) = server_cap else {
        return read(conn, sql, opts, tx, replaceable).await;
    };
    // Same line: error line numbers stay those of the user's SQL.
    let r = read(conn, &format!("set rowcount {n}; {sql}"), opts, tx, false).await;
    let reset = match conn.simple_query("set rowcount 0").await {
        Ok(s) => s.into_results().await.map(|_| ()).map_err(map_err),
        Err(e) => Err(map_err(e)),
    };
    // The statement's own error comes first.
    let ended = r?;
    reset?;
    Ok(ended)
}

pub(crate) async fn read(conn: &mut Conn, sql: &str, opts: &ExecOptions, tx: &StreamSender, replaceable: bool) -> Result<Ended> {
    let mut stream = conn.simple_query(sql).await.map_err(map_err)?;
    let mut builder: Option<BatchBuilder> = None;
    let mut extra_sets = 0usize;
    let mut rows = 0usize;
    let max = opts.max_rows.unwrap_or(usize::MAX);
    let gone = Ended::Stopped { last: None, unread: true };
    while let Some(item) = stream.next().await {
        match item.map_err(map_err)? {
            QueryItem::Metadata(meta) => {
                if builder.is_some() {
                    extra_sets += 1;
                    continue;
                }
                let cols: Vec<Column> = meta
                    .columns()
                    .iter()
                    .map(|c| {
                        let (t, name) = col_type(c.column_type());
                        Column::new(c.name(), t, name)
                    })
                    .collect();
                let b = BatchBuilder::new(&cols, opts.batch_size.max(1));
                if !tx.send(StreamEvent::Schema(b.schema())).await {
                    return Ok(gone);
                }
                builder = Some(b);
            }
            QueryItem::Row(row) => {
                if row.result_index() > 0 {
                    continue;
                }
                let Some(b) = builder.as_mut() else { continue };
                b.push_row(row.into_iter().map(convert));
                rows += 1;
                if rows >= max {
                    // The caller keeps no more rows.
                    let last = if b.is_empty() { None } else { Some(b.finish()?) };
                    if replaceable {
                        return Ok(Ended::Stopped { last, unread: true });
                    }
                    while let Some(item) = stream.next().await {
                        item.map_err(map_err)?;
                    }
                    return Ok(Ended::Stopped { last, unread: false });
                }
                if b.is_full() && !tx.send(StreamEvent::Batch(b.finish()?)).await {
                    return Ok(gone);
                }
            }
        }
    }
    if let Some(b) = builder.as_mut()
        && !b.is_empty()
        && !tx.send(StreamEvent::Batch(b.finish()?)).await
    {
        return Ok(Ended::Complete);
    }
    if extra_sets > 0 {
        tx.send(StreamEvent::Notice(format!("{extra_sets} additional result set(s) not shown"))).await;
    }
    tx.send(StreamEvent::Done(ExecSummary::default())).await;
    Ok(Ended::Complete)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dml_detection() {
        assert!(is_dml("update t set a = 1"));
        assert!(!is_dml("insert into t output inserted.id values (1)"));
        assert!(!is_dml("select 1"));
        assert!(!is_dml("exec sp_who"));
        assert!(!is_dml("create procedure p as select 1"), "DDL runs as a batch");
    }
}
