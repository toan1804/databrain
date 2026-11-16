//! Statement execution: streaming results and row limits.

use crate::config::Conn;
use crate::error::map_err;
use crate::types::MONEY_NOTICE;
use crate::types::col_type;
use crate::types::convert_typed;
use crate::types::money_exact;
use databrain_connector_core::BatchBuilder;
use databrain_connector_core::ConnState;
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
/// The open transactions as the server counts them (`@@TRANCOUNT`), when
/// the batch may have changed them ([`ConnState::may_change_depth`]): text
/// can't follow a batch's branches (`IF … COMMIT`), a one-batch
/// `begin tran … commit`, or an error that ended the transaction. Done
/// before the batch's end is reported, so its state is right by then.
pub(crate) async fn sync_depth(conn: &mut Conn, state: &ConnState, sql: &str, failed: bool) {
    if !state.may_change_depth(sql, failed) {
        return;
    }
    let Ok(stream) = conn.simple_query("SELECT @@TRANCOUNT").await else { return };
    if let Ok(Some(row)) = stream.into_row().await
        && let Ok(Some(n)) = row.try_get::<i32, _>(0)
    {
        state.set_depth(n.max(0) as u32);
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run(
    conn: &mut Conn,
    sql: &str,
    opts: &ExecOptions,
    tx: &StreamSender,
    replaceable: bool,
    user_rowcount: bool,
    state: &ConnState,
) -> Result<Ended> {
    if is_dml(sql) {
        let r = conn.execute(sql, &[]).await.map_err(map_err)?;
        sync_depth(conn, state, sql, false).await;
        tx.send(StreamEvent::Done(ExecSummary { rows_affected: Some(r.rows_affected().iter().sum()) })).await;
        return Ok(Ended::Complete);
    }
    let limit = Limit::of(sql, replaceable);
    let server_cap = opts.max_rows.filter(|_| !user_rowcount && limit.is_read);
    let Some(n) = server_cap else {
        return read(conn, sql, opts, tx, limit, state).await;
    };
    // Same line: error line numbers stay those of the user's SQL. The
    // server stops at the limit, so reading the rest costs nothing.
    let r = read(conn, &format!("set rowcount {n}; {sql}"), opts, tx, Limit { abandon: false, ..limit }, state).await;
    let reset = match conn.simple_query("set rowcount 0").await {
        Ok(s) => s.into_results().await.map(|_| ()).map_err(map_err),
        Err(e) => Err(map_err(e)),
    };
    // The statement's own error comes first.
    let ended = r?;
    reset?;
    Ok(ended)
}

/// What [`read`] does at the row limit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Limit {
    /// Stop without reading the rest (the connection is closed); otherwise
    /// the rest is read and dropped.
    pub abandon: bool,
    /// The statement is a classified read (else a notice says it ran to its end).
    pub is_read: bool,
}

impl Limit {
    /// Only a classified read on a connection that carries no session
    /// state may be abandoned at the row limit. Anything else (`EXEC p`, a
    /// batch that writes after its SELECT) is read to its end: closing the
    /// connection would stop the procedure half-way while the run looked
    /// finished.
    pub(crate) fn of(sql: &str, replaceable: bool) -> Limit {
        let is_read = databrain_connector_core::sql::classify(sql, ConnectorKind::Mssql).kind.is_read();
        Limit { abandon: replaceable && is_read, is_read }
    }
}

/// Read `sql`'s results.
pub(crate) async fn read(conn: &mut Conn, sql: &str, opts: &ExecOptions, tx: &StreamSender, limit: Limit, state: &ConnState) -> Result<Ended> {
    let mut stream = conn.simple_query(sql).await.map_err(map_err)?;
    let mut builder: Option<BatchBuilder> = None;
    // Per column: money (the driver's float is turned back into exact text).
    let mut money: Vec<bool> = Vec::new();
    // A money value too large to come through the driver exactly: said once.
    let mut money_inexact = false;
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
                money = meta.columns().iter().map(|c| matches!(c.column_type(), tiberius::ColumnType::Money | tiberius::ColumnType::Money4)).collect();
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
                b.push_row(row.into_iter().enumerate().map(|(i, v)| {
                    let m = money.get(i).copied().unwrap_or(false);
                    if m && matches!(&v, tiberius::ColumnData::F64(Some(f)) if !money_exact(*f)) {
                        money_inexact = true;
                    }
                    convert_typed(v, m)
                }));
                rows += 1;
                if rows >= max {
                    // The caller keeps no more rows.
                    let last = if b.is_empty() { None } else { Some(b.finish()?) };
                    for n in b.notices(None) {
                        tx.send(StreamEvent::Notice(n)).await;
                    }
                    if money_inexact {
                        tx.send(StreamEvent::Notice(MONEY_NOTICE.into())).await;
                    }
                    if limit.abandon {
                        return Ok(Ended::Stopped { last, unread: true });
                    }
                    // Read to its end (a procedure finishes its work, and an
                    // error after the limit is still reported); the rows
                    // past the limit are dropped.
                    while let Some(item) = stream.next().await {
                        item.map_err(map_err)?;
                    }
                    if !limit.is_read {
                        tx.send(StreamEvent::Notice(format!("The statement ran to its end; only its first {max} rows are kept."))).await;
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
    for n in builder.as_ref().map(|b| b.notices(None)).unwrap_or_default() {
        tx.send(StreamEvent::Notice(n)).await;
    }
    if money_inexact {
        tx.send(StreamEvent::Notice(MONEY_NOTICE.into())).await;
    }
    if extra_sets > 0 {
        tx.send(StreamEvent::Notice(format!("{extra_sets} additional result set(s) not shown"))).await;
    }
    drop(stream);
    sync_depth(conn, state, sql, false).await;
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

    /// Only a plain read may be cut off at the row limit; a procedure or a
    /// batch that writes runs to its end.
    #[test]
    fn only_reads_are_abandoned_at_the_row_limit() {
        for sql in ["select * from t", "with x as (select 1 as a) select * from x", "SELECT TOP 10 * FROM sys.objects"] {
            assert_eq!(Limit::of(sql, true), Limit { abandon: true, is_read: true }, "{sql}");
            assert_eq!(Limit::of(sql, false), Limit { abandon: false, is_read: true }, "{sql}: the connection's state is kept");
        }
        for sql in [
            "exec p",
            "EXECUTE dbo.load_orders @day = '2026-01-01'",
            "p",
            "select * from t; insert into t values (1)",
            "select * from t\nupdate t set a = 1",
            "insert into t output inserted.id select id from s",
            "declare @x int; select @x = 1; select * from t",
        ] {
            assert_eq!(Limit::of(sql, true), Limit { abandon: false, is_read: false }, "{sql}");
        }
    }
}
