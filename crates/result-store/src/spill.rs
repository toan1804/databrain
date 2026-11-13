//! Rows of a result kept on disk once it outgrows its share of memory:
//! Parquet files of [`CHUNK_ROWS`] rows each (one row group), so a page reads
//! one file, and DuckDB can read them all (`read_parquet`) to sort, filter or
//! join a result bigger than memory. Rows not yet a whole file wait in memory.
//! The folder goes when the result does.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use databrain_connector_core::arrow::array::RecordBatch;
use databrain_connector_core::arrow::compute::concat_batches;
use databrain_connector_core::arrow::datatypes::{DataType, Field, Schema, SchemaRef};

use crate::{Error, Result};

/// Rows per file.
pub const CHUNK_ROWS: usize = 32_768;
/// Files kept decoded in memory (the pages around the grid's position).
const CACHED: usize = 4;

struct Chunk {
    path: PathBuf,
    /// The row group within the file (files we write have one).
    row_group: usize,
    /// First row of the file within the result.
    start: usize,
    rows: usize,
    bytes: u64,
}

pub struct Spill {
    dir: PathBuf,
    schema: SchemaRef,
    chunks: Vec<Chunk>,
    pending: Vec<RecordBatch>,
    pending_rows: usize,
    cache: VecDeque<(usize, RecordBatch)>,
    /// Per column: the decimal type that fits every value so far, for exact
    /// decimals kept as text (None for other columns).
    decimals: Vec<Option<DecimalFit>>,
}

/// The DECIMAL that holds every value of a text column of exact decimals
/// (as `outputs::sql_ready_all` picks it for results in memory), or DOUBLE
/// for exponents, NaN/infinity or more than 38 digits.
#[derive(Debug, Clone, Copy, Default)]
pub struct DecimalFit {
    int_digits: usize,
    scale: usize,
    float: bool,
}

impl DecimalFit {
    pub fn add(&mut self, values: &dyn databrain_connector_core::arrow::array::Array) {
        use databrain_connector_core::arrow::array::AsArray;
        let Some(s) = values.as_string_opt::<i32>() else {
            self.float = true;
            return;
        };
        for v in s.iter().flatten() {
            let v = v.trim().trim_start_matches(['-', '+']);
            if v.contains(['e', 'E']) || v.eq_ignore_ascii_case("nan") || v.contains("inf") {
                self.float = true;
                return;
            }
            let (i, f) = v.split_once('.').unwrap_or((v, ""));
            self.int_digits = self.int_digits.max(i.trim_start_matches('0').len().max(1));
            self.scale = self.scale.max(f.len());
        }
    }

    pub fn data_type(&self) -> DataType {
        let p = self.int_digits.max(1) + self.scale;
        if self.float || p > 38 { DataType::Float64 } else { DataType::Decimal128(p as u8, self.scale as i8) }
    }
}

fn io(e: impl std::fmt::Display) -> Error {
    Error::Spill(e.to_string())
}

impl Spill {
    /// A new spill folder `dir` (created; must not hold another result's files).
    pub fn create(dir: PathBuf, schema: SchemaRef) -> Result<Self> {
        std::fs::create_dir_all(&dir).map_err(io)?;
        let decimals = schema.fields().iter().map(|f| crate::display::is_decimal_text(f).then(DecimalFit::default)).collect();
        Ok(Self { dir, schema, chunks: Vec::new(), pending: Vec::new(), pending_rows: 0, cache: VecDeque::new(), decimals })
    }

    /// The rows of an existing Parquet file (written by DuckDB: many row
    /// groups) in `dir`, which goes with it. Columns are cast back to
    /// `schema`'s types when they come back different.
    pub fn from_file(dir: PathBuf, file: PathBuf, schema: SchemaRef) -> Result<Self> {
        use parquet::file::reader::FileReader;
        let reader = parquet::file::reader::SerializedFileReader::new(std::fs::File::open(&file).map_err(io)?).map_err(io)?;
        let meta = reader.metadata();
        let bytes = std::fs::metadata(&file).map_err(io)?.len();
        let mut chunks = Vec::new();
        let mut start = 0usize;
        for rg in 0..meta.num_row_groups() {
            let rows = meta.row_group(rg).num_rows() as usize;
            let share = if meta.num_row_groups() == 0 { 0 } else { bytes / meta.num_row_groups() as u64 };
            chunks.push(Chunk { path: file.clone(), row_group: rg, start, rows, bytes: share });
            start += rows;
        }
        Ok(Self { dir, schema, chunks, pending: Vec::new(), pending_rows: 0, cache: VecDeque::new(), decimals: Vec::new() })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Each file and the result row it starts at (files we wrote).
    pub fn file_starts(&self) -> Vec<(PathBuf, usize)> {
        self.chunks.iter().filter(|c| c.row_group == 0).map(|c| (c.path.clone(), c.start)).collect()
    }

    pub fn rows(&self) -> usize {
        self.chunks.last().map_or(0, |c| c.start + c.rows) + self.pending_rows
    }

    /// Bytes on disk.
    pub fn disk_bytes(&self) -> u64 {
        self.chunks.iter().map(|c| c.bytes).sum()
    }

    /// Bytes held in memory (rows waiting for a file, decoded files).
    pub fn memory_bytes(&self) -> usize {
        databrain_connector_core::memory_size(self.pending.iter().chain(self.cache.iter().map(|(_, b)| b)))
    }

    /// The schema for SQL: exact decimals kept as text become the DECIMAL
    /// that fits every value (all columns nullable).
    pub fn sql_schema(&self) -> SchemaRef {
        let fields: Vec<Field> = self
            .schema
            .fields()
            .iter()
            .zip(&self.decimals)
            .map(|(f, d)| Field::new(f.name(), d.map_or_else(|| f.data_type().clone(), |d| d.data_type()), true))
            .collect();
        std::sync::Arc::new(Schema::new(fields))
    }

    /// Files written so far, in row order.
    pub fn files(&self) -> Vec<PathBuf> {
        self.chunks.iter().map(|c| c.path.clone()).collect()
    }

    /// Add rows; whole files are written as they fill.
    pub fn push(&mut self, batch: RecordBatch) -> Result<()> {
        if batch.num_rows() == 0 {
            return Ok(());
        }
        for (fit, col) in self.decimals.iter_mut().zip(batch.columns()) {
            if let Some(fit) = fit {
                fit.add(col.as_ref());
            }
        }
        self.pending_rows += batch.num_rows();
        self.pending.push(batch);
        while self.pending_rows >= CHUNK_ROWS {
            self.write_chunk(CHUNK_ROWS)?;
        }
        Ok(())
    }

    /// Write the rows still waiting (the result is complete).
    pub fn finish(&mut self) -> Result<()> {
        if self.pending_rows > 0 {
            self.write_chunk(self.pending_rows)?;
        }
        Ok(())
    }

    /// Write the first `rows` waiting rows as one file.
    fn write_chunk(&mut self, rows: usize) -> Result<()> {
        let all = concat_batches(&self.schema, &self.pending)?;
        let (head, rest) = (all.slice(0, rows), all.slice(rows, all.num_rows() - rows));
        let start = self.chunks.last().map_or(0, |c| c.start + c.rows);
        let path = self.dir.join(format!("{:06}.parquet", self.chunks.len()));
        let bytes = write_parquet(&path, &head)?;
        self.chunks.push(Chunk { path, row_group: 0, start, rows, bytes });
        self.pending_rows = rest.num_rows();
        self.pending = if rest.num_rows() > 0 { vec![rest] } else { Vec::new() };
        Ok(())
    }

    fn chunk(&mut self, i: usize) -> Result<RecordBatch> {
        if let Some(pos) = self.cache.iter().position(|(c, _)| *c == i) {
            let hit = self.cache.remove(pos).expect("position is in range");
            let b = hit.1.clone();
            self.cache.push_front(hit);
            return Ok(b);
        }
        let b = read_parquet(&self.chunks[i].path, self.chunks[i].row_group, &self.schema)?;
        self.cache.push_front((i, b.clone()));
        self.cache.truncate(CACHED);
        Ok(b)
    }

    /// Rows `[offset, offset + len)` (clamped), read from the files and the
    /// rows still waiting.
    pub fn slice(&mut self, offset: usize, len: usize) -> Result<RecordBatch> {
        let end = (offset + len).min(self.rows());
        let mut parts = Vec::new();
        let mut at = offset.min(end);
        while at < end {
            let i = self.chunks.partition_point(|c| c.start + c.rows <= at);
            if i < self.chunks.len() {
                let (start, rows) = (self.chunks[i].start, self.chunks[i].rows);
                let (from, to) = (at - start, (end - start).min(rows));
                let b = self.chunk(i)?;
                parts.push(b.slice(from, to - from));
                at = start + to;
            } else {
                // Not yet in a file.
                let base = self.chunks.last().map_or(0, |c| c.start + c.rows);
                let waiting = concat_batches(&self.schema, &self.pending)?;
                parts.push(waiting.slice(at - base, end - at));
                at = end;
            }
        }
        Ok(concat_batches(&self.schema, &parts)?)
    }

    /// Every row, a batch at a time (at most `chunk` rows), without keeping
    /// them: `f` gets each batch in order.
    pub fn for_each(&mut self, chunk: usize, f: &mut dyn FnMut(RecordBatch) -> Result<()>) -> Result<()> {
        let chunk = chunk.max(1);
        let total = self.rows();
        let mut at = 0;
        while at < total {
            let len = chunk.min(total - at);
            f(self.slice(at, len)?)?;
            at += len;
        }
        Ok(())
    }
}

impl Drop for Spill {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn write_parquet(path: &Path, b: &RecordBatch) -> Result<u64> {
    use parquet::arrow::ArrowWriter;
    use parquet::basic::{Compression, ZstdLevel};
    use parquet::file::properties::WriterProperties;
    let tmp = path.with_extension("tmp");
    let file = std::fs::File::create(&tmp).map_err(io)?;
    // Fast compression: rows are written as fast as they arrive.
    let props = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::try_new(1).map_err(io)?))
        .set_max_row_group_row_count(Some(CHUNK_ROWS))
        .build();
    let mut w = ArrowWriter::try_new(file, b.schema(), Some(props)).map_err(io)?;
    w.write(b).map_err(io)?;
    w.close().map_err(io)?;
    std::fs::rename(&tmp, path).map_err(io)?;
    Ok(std::fs::metadata(path).map_err(io)?.len())
}

fn read_parquet(path: &Path, row_group: usize, schema: &SchemaRef) -> Result<RecordBatch> {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    let file = std::fs::File::open(path).map_err(io)?;
    let reader =
        ParquetRecordBatchReaderBuilder::try_new(file).map_err(io)?.with_row_groups(vec![row_group]).with_batch_size(CHUNK_ROWS).build().map_err(io)?;
    let batches = reader.collect::<std::result::Result<Vec<_>, _>>()?;
    let read = batches.first().map(|b| b.schema()).unwrap_or_else(|| schema.clone());
    let b = concat_batches(&read, &batches)?;
    // The result's own schema (field metadata such as database types; a
    // type DuckDB wrote differently, e.g. microsecond timestamps, cast back).
    let cols = b
        .columns()
        .iter()
        .zip(schema.fields())
        .map(|(c, f)| if c.data_type() == f.data_type() { Ok(c.clone()) } else { databrain_connector_core::arrow::compute::cast(c, f.data_type()) })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(RecordBatch::try_new(schema.clone(), cols)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use databrain_connector_core::arrow::array::{Array, AsArray, Int64Array, StringArray};
    use databrain_connector_core::arrow::datatypes::Int64Type;
    use std::sync::Arc;

    fn batch(schema: &SchemaRef, from: i64, n: i64) -> RecordBatch {
        RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from_iter_values(from..from + n)),
                Arc::new(StringArray::from_iter((from..from + n).map(|i| (i % 7 != 0).then(|| format!("v{i}"))))),
            ],
        )
        .unwrap()
    }

    #[test]
    fn rows_come_back_in_order_across_files() {
        let schema: SchemaRef = Arc::new(Schema::new(vec![Field::new("n", DataType::Int64, false), Field::new("s", DataType::Utf8, true)]));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r1");
        let mut s = Spill::create(path.clone(), schema.clone()).unwrap();
        let total = CHUNK_ROWS as i64 * 3 + 1234;
        let mut at = 0;
        while at < total {
            let n = 5000.min(total - at);
            s.push(batch(&schema, at, n)).unwrap();
            at += n;
        }
        assert_eq!(s.files().len(), 3, "whole files only; the rest waits");
        assert_eq!(s.rows(), total as usize);
        // A page across a file boundary and into the waiting rows.
        for off in [0usize, CHUNK_ROWS - 3, CHUNK_ROWS * 3 - 2] {
            let p = s.slice(off, 10).unwrap();
            let ns: Vec<i64> = p.column(0).as_primitive::<Int64Type>().values().to_vec();
            assert_eq!(ns, (off as i64..off as i64 + 10).collect::<Vec<_>>());
            assert_eq!(p.column(1).is_null(0), off % 7 == 0);
        }
        s.finish().unwrap();
        assert_eq!(s.files().len(), 4);
        let mut seen = 0i64;
        s.for_each(10_000, &mut |b| {
            assert_eq!(b.column(0).as_primitive::<Int64Type>().value(0), seen);
            seen += b.num_rows() as i64;
            Ok(())
        })
        .unwrap();
        assert_eq!(seen, total);
        assert!(s.disk_bytes() > 0 && s.memory_bytes() < 64 << 20);
        drop(s);
        assert!(!path.exists(), "the folder goes with the result");
    }
}
