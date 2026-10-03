use arrow::datatypes::SchemaRef;
use arrow::record_batch::RecordBatch;
use serde::Serialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::error::{ConnectorError, Result};

/// Options for a single statement execution.
#[derive(Debug, Clone)]
pub struct ExecOptions {
    /// Target rows per Arrow batch.
    pub batch_size: usize,
    /// Cancelled by the query engine when the user presses Stop or the row
    /// cap is reached. Connectors must stop producing and cancel server-side.
    pub cancel: CancellationToken,
    /// Most rows the caller will keep (row cap + 1 to detect truncation).
    /// A hint: connectors whose server can limit the result (Databricks
    /// `row_limit`) pass it on so less data is produced and transferred.
    pub max_rows: Option<usize>,
}

impl Default for ExecOptions {
    fn default() -> Self {
        Self {
            batch_size: 1000,
            cancel: CancellationToken::new(),
            max_rows: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ExecSummary {
    /// Rows affected for DML/DDL statements, when reported by the driver.
    pub rows_affected: Option<u64>,
}

#[derive(Debug)]
pub enum StreamEvent {
    /// Result schema. Sent once, before any batch. Absent for statements that
    /// return no rows.
    Schema(SchemaRef),
    Batch(RecordBatch),
    /// Server notice or conversion warning.
    Notice(String),
    Done(ExecSummary),
}

/// Receiving half of a statement's result stream.
pub struct QueryStream {
    rx: mpsc::Receiver<Result<StreamEvent>>,
}

/// Sending half, used by connector producer tasks.
#[derive(Clone)]
pub struct StreamSender {
    tx: mpsc::Sender<Result<StreamEvent>>,
}

impl QueryStream {
    /// Create a bounded channel. A small bound provides backpressure so a
    /// slow consumer doesn't make the driver buffer the whole result.
    pub fn channel(capacity: usize) -> (StreamSender, QueryStream) {
        let (tx, rx) = mpsc::channel(capacity.max(1));
        (StreamSender { tx }, QueryStream { rx })
    }

    pub async fn next(&mut self) -> Option<Result<StreamEvent>> {
        self.rx.recv().await
    }

    /// Drain the stream into memory (tests and small internal queries).
    pub async fn collect(mut self) -> Result<Collected> {
        let mut out = Collected::default();
        while let Some(ev) = self.next().await {
            match ev? {
                StreamEvent::Schema(s) => out.schema = Some(s),
                StreamEvent::Batch(b) => out.batches.push(b),
                StreamEvent::Notice(n) => out.notices.push(n),
                StreamEvent::Done(s) => {
                    out.summary = s;
                    return Ok(out);
                }
            }
        }
        Err(ConnectorError::internal("stream ended without completion"))
    }
}

#[derive(Debug, Default)]
pub struct Collected {
    pub schema: Option<SchemaRef>,
    pub batches: Vec<RecordBatch>,
    pub notices: Vec<String>,
    pub summary: ExecSummary,
}

impl Collected {
    pub fn num_rows(&self) -> usize {
        self.batches.iter().map(|b| b.num_rows()).sum()
    }
}

impl StreamSender {
    /// Returns `false` if the receiver was dropped (consumer stopped).
    pub async fn send(&self, ev: StreamEvent) -> bool {
        self.tx.send(Ok(ev)).await.is_ok()
    }

    pub async fn send_err(&self, e: ConnectorError) -> bool {
        self.tx.send(Err(e)).await.is_ok()
    }

    /// Blocking variant for connectors running on a blocking thread.
    pub fn blocking_send(&self, ev: Result<StreamEvent>) -> bool {
        self.tx.blocking_send(ev).is_ok()
    }

    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }
}
