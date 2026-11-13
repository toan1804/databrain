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
    /// Output columns to mask when `results.<name>` tables are loaded
    /// (DuckDB; see [`crate::external::OutputMasks`]).
    pub output_masks: Option<std::sync::Arc<crate::external::OutputMasks>>,
}

impl Default for ExecOptions {
    fn default() -> Self {
        Self { batch_size: 1000, cancel: CancellationToken::new(), max_rows: None, output_masks: None }
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
    /// The server has finished running the statement; what follows is the
    /// result being downloaded (cloud warehouses that hand results out in
    /// chunks, e.g. Databricks external links). Sent once, before the
    /// schema. With a message for the user when the result is large.
    Downloading(Option<String>),
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
                StreamEvent::Notice(n) | StreamEvent::Downloading(Some(n)) => out.notices.push(n),
                StreamEvent::Downloading(None) => {}
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

/// Bytes `batches` keep in memory, counting each allocation once however
/// many arrays (or batches) share it.
///
/// `RecordBatch::get_array_memory_size` counts a buffer once per array that
/// points into it. A batch decoded from Arrow IPC (DuckDB, Databricks) has
/// every column inside one message buffer, so it was counted once per
/// column: 16 times the real size for 8 columns, 80 times for 40.
pub fn memory_size<'a>(batches: impl IntoIterator<Item = &'a RecordBatch>) -> usize {
    fn walk(d: &arrow::array::ArrayData, seen: &mut std::collections::HashSet<usize>, total: &mut usize) {
        let mut add = |b: &arrow::buffer::Buffer| {
            if seen.insert(b.data_ptr().as_ptr() as usize) {
                *total += b.capacity();
            }
        };
        d.buffers().iter().for_each(&mut add);
        if let Some(n) = d.nulls() {
            add(n.buffer());
        }
        for c in d.child_data() {
            walk(c, seen, total);
        }
    }
    let mut seen = std::collections::HashSet::new();
    let mut total = 0;
    for b in batches {
        for col in b.columns() {
            walk(&col.to_data(), &mut seen, &mut total);
        }
    }
    total
}

#[cfg(test)]
mod memory_tests {
    use super::*;
    use arrow::array::{ArrayRef, Int64Array, StringArray};
    use std::sync::Arc;

    fn batch(cols: usize, rows: usize) -> RecordBatch {
        let arrays: Vec<(String, ArrayRef)> =
            (0..cols).map(|i| (format!("c{i}"), Arc::new(StringArray::from_iter_values((0..rows).map(|r| format!("value {r}")))) as ArrayRef)).collect();
        RecordBatch::try_from_iter(arrays).unwrap()
    }

    fn ipc_round_trip(b: &RecordBatch) -> RecordBatch {
        let mut buf = Vec::new();
        let mut w = arrow::ipc::writer::StreamWriter::try_new(&mut buf, &b.schema()).unwrap();
        w.write(b).unwrap();
        w.finish().unwrap();
        drop(w);
        arrow::ipc::reader::StreamReader::try_new(std::io::Cursor::new(buf), None).unwrap().next().unwrap().unwrap()
    }

    #[test]
    fn a_shared_buffer_is_counted_once() {
        let b = ipc_round_trip(&batch(40, 2000));
        let real = memory_size([&b]);
        // Every column points into the one IPC buffer.
        assert!(b.get_array_memory_size() > 30 * real, "{} vs {real}", b.get_array_memory_size());
        let raw: usize = b.columns().iter().map(|c| c.to_data().buffers().iter().map(|x| x.len()).sum::<usize>()).sum();
        assert!(real >= raw && real < raw * 2, "{real} vs {raw}");
    }

    #[test]
    fn separate_buffers_and_slices() {
        let b = batch(3, 1000);
        // Arrays of their own: same as Arrow's count, less the struct overhead.
        assert!(memory_size([&b]) <= b.get_array_memory_size());
        assert!(memory_size([&b]) * 2 > b.get_array_memory_size());
        // Slices keep their parent's buffers: counted once across batches.
        let (x, y) = (b.slice(0, 500), b.slice(500, 500));
        assert_eq!(memory_size([&x, &y]), memory_size([&b]));
        let n = RecordBatch::try_from_iter([("n", Arc::new(Int64Array::from(vec![Some(1), None])) as ArrayRef)]).unwrap();
        assert_eq!(memory_size([&n]), n.column(0).to_data().buffers()[0].capacity() + n.column(0).nulls().unwrap().buffer().capacity());
    }
}
