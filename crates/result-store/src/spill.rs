//! Rows of a result kept on disk once it outgrows its share of memory:
//! Parquet files of at most [`CHUNK_ROWS`] rows and about [`CHUNK_BYTES`]
//! each (one row group), so a page reads one file, and DuckDB can read them
//! all (`read_parquet`) to sort or join a result bigger than memory. Rows
//! not yet a whole file wait in memory. The folder goes when the result and
//! every reader of it ([`Files`]) do.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use databrain_connector_core::arrow::array::RecordBatch;
use databrain_connector_core::arrow::compute::{CastOptions, cast_with_options, concat_batches};
use databrain_connector_core::arrow::datatypes::{DataType, Field, Schema, SchemaRef};

use crate::{Error, Result};

/// Most rows per file.
pub const CHUNK_ROWS: usize = 32_768;
/// About the most bytes (in memory) per file: wide rows make smaller files.
pub const CHUNK_BYTES: usize = 64 << 20;
/// Decoded files kept in memory (the pages around the grid's position), by
/// bytes: at least the last one read.
const CACHE_BYTES: usize = 192 << 20;

#[derive(Clone)]
struct Chunk {
    path: PathBuf,
    /// The row group within the file (files we write have one).
    row_group: usize,
    /// First row of the file within the result.
    start: usize,
    rows: usize,
    bytes: u64,
}

/// Deletes a folder when the last holder lets go: the result's [`Spill`]
/// and any [`Files`] still reading it.
struct DirGuard(PathBuf);

impl Drop for DirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub struct Spill {
    dir: Arc<DirGuard>,
    schema: SchemaRef,
    chunks: Vec<Chunk>,
    pending: Vec<RecordBatch>,
    pending_rows: usize,
    pending_bytes: usize,
    cache: VecDeque<(usize, RecordBatch, usize)>,
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
            if !is_plain_decimal(v) {
                self.float = true;
                return;
            }
            let v = v.trim().trim_start_matches(['-', '+']);
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

/// Digits with an optional sign and point: not an exponent, NaN or
/// infinity (`Infinity`, `-inf`, any case), which need a DOUBLE.
pub fn is_plain_decimal(v: &str) -> bool {
    let t = v.trim().trim_start_matches(['-', '+']);
    !(t.contains(['e', 'E']) || t.eq_ignore_ascii_case("nan") || t.to_ascii_lowercase().starts_with("inf"))
}

fn io(e: impl std::fmt::Display) -> Error {
    Error::Spill(e.to_string())
}

/// Whether rows like `sample` can be written to the result's files: a
/// one-row trial in memory (some Arrow types have no Parquet form, and the
/// writer panics on a few, e.g. unions).
pub fn can_write(sample: &RecordBatch) -> bool {
    let one = sample.slice(0, sample.num_rows().min(1));
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
        let mut w = parquet::arrow::ArrowWriter::try_new(Vec::new(), one.schema(), None).map_err(io)?;
        w.write(&one).map_err(io)?;
        w.close().map_err(io)?;
        Ok(())
    }))
    .is_ok_and(|r| r.is_ok())
}

/// A snapshot of a [`Spill`]'s rows to read without it (and without the
/// result's lock): the files keep existing until it is dropped.
pub struct Files {
    _dir: Arc<DirGuard>,
    schema: SchemaRef,
    chunks: Vec<Chunk>,
    /// Rows not yet in a file.
    tail: Vec<RecordBatch>,
}

impl Files {
    pub fn rows(&self) -> usize {
        self.chunks.last().map_or(0, |c| c.start + c.rows) + self.tail.iter().map(|b| b.num_rows()).sum::<usize>()
    }

    /// Every row in order, a file (or part) at a time, at most `chunk` rows.
    pub fn for_each(&self, chunk: usize, f: &mut dyn FnMut(RecordBatch) -> Result<()>) -> Result<()> {
        let chunk = chunk.max(1);
        let mut emit = |b: RecordBatch| -> Result<()> {
            let mut at = 0;
            while at < b.num_rows() {
                let len = chunk.min(b.num_rows() - at);
                f(b.slice(at, len))?;
                at += len;
            }
            Ok(())
        };
        for c in &self.chunks {
            emit(read_parquet(&c.path, c.row_group, &self.schema)?)?;
        }
        for b in &self.tail {
            emit(b.clone())?;
        }
        Ok(())
    }
}

impl Spill {
    /// A new spill folder `dir` (created; must not hold another result's files).
    pub fn create(dir: PathBuf, schema: SchemaRef) -> Result<Self> {
        std::fs::create_dir_all(&dir).map_err(io)?;
        let decimals = schema.fields().iter().map(|f| crate::display::is_decimal_text(f).then(DecimalFit::default)).collect();
        Ok(Self {
            dir: Arc::new(DirGuard(dir)),
            schema,
            chunks: Vec::new(),
            pending: Vec::new(),
            pending_rows: 0,
            pending_bytes: 0,
            cache: VecDeque::new(),
            decimals,
        })
    }

    /// The rows of an existing Parquet file (written by DuckDB: many row
    /// groups) in `dir`, which goes with it. Columns are cast back to
    /// `schema`'s types (an error when a value doesn't fit, never NULL).
    pub fn from_file(dir: PathBuf, file: PathBuf, schema: SchemaRef) -> Result<Self> {
        use parquet::file::reader::FileReader;
        let guard = Arc::new(DirGuard(dir));
        let reader = parquet::file::reader::SerializedFileReader::new(std::fs::File::open(&file).map_err(io)?).map_err(io)?;
        let meta = reader.metadata();
        let mut chunks = Vec::new();
        let mut start = 0usize;
        for rg in 0..meta.num_row_groups() {
            let g = meta.row_group(rg);
            let rows = g.num_rows() as usize;
            chunks.push(Chunk { path: file.clone(), row_group: rg, start, rows, bytes: g.compressed_size().max(0) as u64 });
            start += rows;
        }
        Ok(Self { dir: guard, schema, chunks, pending: Vec::new(), pending_rows: 0, pending_bytes: 0, cache: VecDeque::new(), decimals: Vec::new() })
    }

    pub fn dir(&self) -> &Path {
        &self.dir.0
    }

    /// Each file and the result row it starts at (files we wrote).
    pub fn file_starts(&self) -> Vec<(PathBuf, usize)> {
        self.chunks.iter().filter(|c| c.row_group == 0).map(|c| (c.path.clone(), c.start)).collect()
    }

    pub fn rows(&self) -> usize {
        self.chunks.last().map_or(0, |c| c.start + c.rows) + self.pending_rows
    }

    /// Rows in files (not waiting in memory).
    pub fn written_rows(&self) -> usize {
        self.chunks.last().map_or(0, |c| c.start + c.rows)
    }

    /// Bytes on disk.
    pub fn disk_bytes(&self) -> u64 {
        self.chunks.iter().map(|c| c.bytes).sum()
    }

    /// Bytes held in memory (rows waiting for a file, decoded files).
    pub fn memory_bytes(&self) -> usize {
        self.pending_bytes + self.cache.iter().map(|(_, _, b)| b).sum::<usize>()
    }

    /// The schema for SQL: exact decimals kept as text become the DECIMAL
    /// that fits every value (all columns nullable).
    pub fn sql_schema(&self) -> SchemaRef {
        let fields: Vec<Field> = self
            .schema
            .fields()
            .iter()
            .zip(self.decimals.iter().chain(std::iter::repeat(&None)))
            .map(|(f, d)| Field::new(f.name(), d.map_or_else(|| f.data_type().clone(), |d| d.data_type()), true))
            .collect();
        Arc::new(Schema::new(fields))
    }

    /// Files written so far, in row order.
    pub fn files(&self) -> Vec<PathBuf> {
        self.chunks.iter().map(|c| c.path.clone()).collect()
    }

    /// The rows as they are now, to read without this [`Spill`].
    pub fn snapshot(&self) -> Files {
        Files { _dir: self.dir.clone(), schema: self.schema.clone(), chunks: self.chunks.clone(), tail: self.pending.clone() }
    }

    /// Average bytes per row in memory so far (for row groups others write).
    pub fn rows_per_group(&self) -> usize {
        let (rows, bytes) = self.chunks.iter().fold((0usize, 0u64), |(r, b), c| (r + c.rows, b + c.bytes));
        // Files are compressed: estimate memory as 4× their size.
        if rows == 0 || bytes == 0 { CHUNK_ROWS } else { ((CHUNK_BYTES as u64 * rows as u64) / (bytes * 4)).clamp(256, CHUNK_ROWS as u64) as usize }
    }

    /// Add rows; files are written as they fill (by rows or bytes). On a
    /// write error the rows waiting for a file are dropped (see
    /// [`Self::discard_pending`]) and the error returned.
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
        self.pending_bytes += databrain_connector_core::memory_size([&batch]);
        self.pending.push(batch);
        while self.pending_rows >= CHUNK_ROWS || (self.pending_bytes >= CHUNK_BYTES && self.pending_rows > 0) {
            // A file of whole rows within the byte size (at least one row).
            let per_row = (self.pending_bytes / self.pending_rows).max(1);
            let rows = (CHUNK_BYTES / per_row).clamp(1, CHUNK_ROWS).min(self.pending_rows);
            if let Err(e) = self.write_chunk(rows) {
                self.discard_pending();
                return Err(e);
            }
        }
        Ok(())
    }

    /// Write the rows still waiting (the result is complete).
    pub fn finish(&mut self) -> Result<()> {
        if self.pending_rows > 0
            && let Err(e) = self.write_chunk(self.pending_rows)
        {
            self.discard_pending();
            return Err(e);
        }
        Ok(())
    }

    /// Drop the rows waiting for a file: after a failed write, so what
    /// SQL, sorting and the grid see is the same (the rows in files).
    pub fn discard_pending(&mut self) {
        self.pending.clear();
        self.pending_rows = 0;
        self.pending_bytes = 0;
    }

    /// Write the first `rows` waiting rows as one file.
    fn write_chunk(&mut self, rows: usize) -> Result<()> {
        let all = concat_batches(&self.schema, &self.pending)?;
        self.pending.clear();
        let (head, rest) = (all.slice(0, rows), all.slice(rows, all.num_rows() - rows));
        let start = self.written_rows();
        let path = self.dir.0.join(format!("{:06}.parquet", self.chunks.len()));
        let written = write_parquet(&path, &head);
        // Keep the rows either way (a failed write drops them in the caller).
        self.pending_rows = all.num_rows();
        self.pending = vec![all.clone()];
        let bytes = written?;
        self.chunks.push(Chunk { path, row_group: 0, start, rows, bytes });
        self.pending_rows = rest.num_rows();
        self.pending_bytes = databrain_connector_core::memory_size([&rest]);
        self.pending = if rest.num_rows() > 0 { vec![rest] } else { Vec::new() };
        Ok(())
    }

    fn chunk(&mut self, i: usize) -> Result<RecordBatch> {
        if let Some(pos) = self.cache.iter().position(|(c, _, _)| *c == i) {
            let hit = self.cache.remove(pos).expect("position is in range");
            let b = hit.1.clone();
            self.cache.push_front(hit);
            return Ok(b);
        }
        let b = read_parquet(&self.chunks[i].path, self.chunks[i].row_group, &self.schema)?;
        let size = databrain_connector_core::memory_size([&b]);
        self.cache.push_front((i, b.clone(), size));
        while self.cache.len() > 1 && self.cache.iter().map(|(_, _, s)| s).sum::<usize>() > CACHE_BYTES {
            self.cache.pop_back();
        }
        Ok(b)
    }

    /// Rows `[offset, offset + len)` (clamped), read from the files and the
    /// rows still waiting.
    pub fn slice(&mut self, offset: usize, len: usize) -> Result<RecordBatch> {
        let end = offset.saturating_add(len).min(self.rows());
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
                let base = self.written_rows();
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
        self.snapshot().for_each(chunk, f)
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
    let r = (|| -> Result<()> {
        let mut w = ArrowWriter::try_new(file, b.schema(), Some(props)).map_err(io)?;
        w.write(b).map_err(io)?;
        w.close().map_err(io)?;
        std::fs::rename(&tmp, path).map_err(io)
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r?;
    Ok(std::fs::metadata(path).map_err(io)?.len())
}

pub(crate) fn read_parquet(path: &Path, row_group: usize, schema: &SchemaRef) -> Result<RecordBatch> {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    let file = std::fs::File::open(path).map_err(io)?;
    let reader =
        ParquetRecordBatchReaderBuilder::try_new(file).map_err(io)?.with_row_groups(vec![row_group]).with_batch_size(CHUNK_ROWS).build().map_err(io)?;
    let batches = reader.collect::<std::result::Result<Vec<_>, _>>()?;
    let read = batches.first().map(|b| b.schema()).unwrap_or_else(|| schema.clone());
    let b = concat_batches(&read, &batches)?;
    // The result's own schema (field metadata such as database types; a
    // type DuckDB wrote differently, e.g. microsecond timestamps, cast back:
    // a value that doesn't fit is an error, never a silent NULL).
    let strict = CastOptions { safe: false, ..Default::default() };
    let cols = b
        .columns()
        .iter()
        .zip(schema.fields())
        .map(|(c, f)| if c.data_type() == f.data_type() { Ok(c.clone()) } else { cast_with_options(c, f.data_type(), &strict) })
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

    /// Wide rows make smaller files: a file stays near CHUNK_BYTES in memory.
    #[test]
    fn wide_rows_are_split_by_bytes() {
        let schema: SchemaRef = Arc::new(Schema::new(vec![Field::new("blob", DataType::Utf8, false)]));
        let dir = tempfile::tempdir().unwrap();
        let mut s = Spill::create(dir.path().join("w"), schema.clone()).unwrap();
        let big = "x".repeat(100_000);
        for _ in 0..4 {
            let b = RecordBatch::try_new(schema.clone(), vec![Arc::new(StringArray::from_iter_values((0..500).map(|_| big.as_str())))]).unwrap();
            s.push(b).unwrap();
        }
        // 2,000 rows of ~100 KB: several files though far below CHUNK_ROWS.
        assert!(s.files().len() >= 2, "{} files", s.files().len());
        assert!(s.memory_bytes() < 2 * CHUNK_BYTES);
        assert_eq!(s.rows(), 2000);
        assert_eq!(s.slice(1999, 5).unwrap().num_rows(), 1);
    }

    /// A failed write drops the rows waiting for a file, so the count is
    /// what every reader sees.
    #[test]
    fn a_failed_write_keeps_the_count_true() {
        let schema: SchemaRef = Arc::new(Schema::new(vec![Field::new("n", DataType::Int64, false), Field::new("s", DataType::Utf8, true)]));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        let mut s = Spill::create(path.clone(), schema.clone()).unwrap();
        s.push(batch(&schema, 0, CHUNK_ROWS as i64 + 10)).unwrap();
        assert_eq!((s.files().len(), s.rows()), (1, CHUNK_ROWS + 10));
        std::fs::remove_dir_all(&path).unwrap(); // the disk is gone
        assert!(s.push(batch(&schema, 0, CHUNK_ROWS as i64)).is_err());
        assert_eq!(s.rows(), CHUNK_ROWS, "only the rows in files");
        assert_eq!(s.rows(), s.written_rows());
    }

    #[test]
    fn exotic_decimals_are_doubles() {
        for v in ["Infinity", "-Infinity", "inf", "NaN", "1e5"] {
            assert!(!is_plain_decimal(v), "{v}");
        }
        assert!(is_plain_decimal("-12.50"));
    }
}
