use databrain_connector_core::arrow::array::RecordBatch;
use databrain_connector_core::arrow::datatypes::SchemaRef;

use crate::exporter::Exporter;
use crate::xlsx::write_xlsx;
use crate::{ExportError, ExportFormat, ExportOptions, Result};

/// Excel's row limit (including the header row).
pub const XLSX_MAX_ROWS: usize = 1_048_576;

/// Write batches to a file in any format. Parquet keeps Arrow types (exact
/// decimals stay text); XLSX writes numbers/booleans natively, streaming rows
/// with constant memory. Returns rows written.
pub fn export_file(path: &std::path::Path, schema: SchemaRef, batches: &[RecordBatch], opts: ExportOptions) -> Result<u64> {
    let mut w = FileWriter::create(path, schema, opts)?;
    for b in batches {
        w.write(b)?;
    }
    w.finish()
}

/// Incremental file export: batches are written as they arrive (a query
/// streamed to disk), except XLSX, whose rows are kept until [`finish`]
/// (Excel is limited to about a million rows anyway).
///
/// Rows go to a new temporary file next to the destination (created
/// exclusively, so an existing file or link of that name is never truncated
/// or followed), which [`finish`] renames to the destination. A writer
/// dropped without `finish`, or a failed one, removes its temporary file and
/// leaves the destination as it was.
///
/// [`finish`]: FileWriter::finish
pub struct FileWriter {
    inner: Option<Writer>,
    rows: u64,
    tmp: std::path::PathBuf,
    dest: std::path::PathBuf,
}

enum Writer {
    Parquet { w: Box<parquet::arrow::ArrowWriter<std::fs::File>>, cols: Vec<usize>, projected: SchemaRef },
    Xlsx { file: std::fs::File, schema: SchemaRef, cols: Vec<usize>, opts: ExportOptions, batches: Vec<RecordBatch> },
    Text(Exporter<std::io::BufWriter<std::fs::File>>),
}

/// A new, exclusively created temporary file next to `dest`.
fn create_temp_beside(dest: &std::path::Path) -> Result<(std::path::PathBuf, std::fs::File)> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = dest.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(std::path::Path::new("."));
    let name = dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "export".into());
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    loop {
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = dir.join(format!(".{name}.{}-{nanos:x}-{seq}.partial", std::process::id()));
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(f) => return Ok((tmp, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
}

impl FileWriter {
    pub fn create(path: &std::path::Path, schema: SchemaRef, opts: ExportOptions) -> Result<Self> {
        let cols: Vec<usize> = if opts.columns.is_empty() {
            (0..schema.fields().len()).collect()
        } else {
            opts.columns.iter().copied().filter(|c| *c < schema.fields().len()).collect()
        };
        let (tmp, file) = create_temp_beside(path)?;
        let made = (|| -> Result<Writer> {
            Ok(match opts.format {
                ExportFormat::Parquet => {
                    let projected = std::sync::Arc::new(schema.project(&cols)?);
                    let props =
                        parquet::file::properties::WriterProperties::builder().set_compression(parquet::basic::Compression::ZSTD(Default::default())).build();
                    Writer::Parquet { w: Box::new(parquet::arrow::ArrowWriter::try_new(file, projected.clone(), Some(props))?), cols, projected }
                }
                ExportFormat::Xlsx => Writer::Xlsx { file, schema, cols, opts, batches: Vec::new() },
                _ => Writer::Text(Exporter::new(std::io::BufWriter::new(file), schema, opts)),
            })
        })();
        match made {
            Ok(w) => Ok(Self { inner: Some(w), rows: 0, tmp, dest: path.to_path_buf() }),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(e)
            }
        }
    }

    /// Rows written so far.
    pub fn rows(&self) -> u64 {
        self.rows
    }

    /// The temporary file being written (its size shows progress).
    pub fn temp_path(&self) -> &std::path::Path {
        &self.tmp
    }

    pub fn write(&mut self, b: &RecordBatch) -> Result<()> {
        match self.inner.as_mut().expect("written after finish") {
            Writer::Parquet { w, cols, projected } => {
                let pb = b.project(cols)?;
                // Re-attach the projected schema (keeps field metadata).
                let pb = RecordBatch::try_new(projected.clone(), pb.columns().to_vec())?;
                w.write(&pb)?;
            }
            Writer::Xlsx { batches, .. } => {
                if self.rows as usize + b.num_rows() + 1 > XLSX_MAX_ROWS {
                    return Err(ExportError::Unsupported(format!(
                        "Excel supports at most {} rows; this export has more. Use CSV or Parquet.",
                        XLSX_MAX_ROWS - 1
                    )));
                }
                batches.push(b.clone());
            }
            Writer::Text(e) => e.write_batch(b)?,
        }
        self.rows += b.num_rows() as u64;
        Ok(())
    }

    /// Finish the file and move it to the destination (replacing a file
    /// there, never writing through a link). Returns rows written.
    pub fn finish(mut self) -> Result<u64> {
        let rows = match self.inner.take().expect("finished twice") {
            Writer::Parquet { w, .. } => {
                w.close()?;
                self.rows
            }
            Writer::Xlsx { file, schema, cols, opts, batches } => write_xlsx(file, &schema, &batches, &cols, &opts)?,
            Writer::Text(e) => e.finish()?,
        };
        std::fs::rename(&self.tmp, &self.dest)?;
        Ok(rows)
    }
}

impl Drop for FileWriter {
    fn drop(&mut self) {
        // Not finished, or finishing failed: the destination stays as it was.
        if std::fs::symlink_metadata(&self.tmp).is_ok() {
            let _ = std::fs::remove_file(&self.tmp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::*;
    use crate::*;

    #[test]
    fn parquet_roundtrip() {
        use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
        let b = batch();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.parquet");
        let mut o = ExportOptions::new(ExportFormat::Parquet);
        o.columns = vec![0, 1];
        assert_eq!(export_file(&p, b.schema(), &[b.clone()], o).unwrap(), 2);
        let r = ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(&p).unwrap()).unwrap().build().unwrap();
        let got: Vec<RecordBatch> = r.collect::<std::result::Result<_, _>>().unwrap();
        assert_eq!(got[0].num_columns(), 2);
        assert_eq!(got[0].schema().field(1).name(), "name");
        assert_eq!(got[0].num_rows(), 2);
    }

    #[test]
    fn xlsx_writes_zip() {
        let b = batch();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.xlsx");
        assert_eq!(export_file(&p, b.schema(), &[b.clone()], ExportOptions::new(ExportFormat::Xlsx)).unwrap(), 2);
        let bytes = std::fs::read(&p).unwrap();
        assert_eq!(&bytes[..2], b"PK");
        assert!(export_to_string(b.schema(), [&b], ExportOptions::new(ExportFormat::Xlsx)).is_err());
    }

    /// The destination is replaced only by a finished export: a dropped or
    /// failed writer leaves it whole, a link there is replaced (not written
    /// through), and no temporary file is left behind.
    #[test]
    fn file_writer_replaces_the_file_only_when_finished() {
        let dir = tempfile::tempdir().unwrap();
        let b = RecordBatch::try_from_iter([(
            "a",
            std::sync::Arc::new(databrain_connector_core::arrow::array::Int32Array::from(vec![1, 2])) as databrain_connector_core::arrow::array::ArrayRef,
        )])
        .unwrap();
        let dest = dir.path().join("out.csv");
        std::fs::write(&dest, "old").unwrap();
        for fmt in [ExportFormat::Csv, ExportFormat::Parquet, ExportFormat::Xlsx] {
            let mut w = FileWriter::create(&dest, b.schema(), ExportOptions::new(fmt)).unwrap();
            w.write(&b).unwrap();
            assert!(w.temp_path().exists());
            drop(w);
            assert_eq!(std::fs::read_to_string(&dest).unwrap(), "old", "{fmt:?}: dropped before finish");
        }
        let mut w = FileWriter::create(&dest, b.schema(), ExportOptions::new(ExportFormat::Csv)).unwrap();
        w.write(&b).unwrap();
        assert_eq!(w.finish().unwrap(), 2);
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "a\n1\n2\n");
        #[cfg(unix)]
        {
            let target = dir.path().join("target.txt");
            std::fs::write(&target, "keep").unwrap();
            let link = dir.path().join("link.csv");
            std::os::unix::fs::symlink(&target, &link).unwrap();
            assert_eq!(export_file(&link, b.schema(), &[b.clone()], ExportOptions::new(ExportFormat::Csv)).unwrap(), 2);
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep", "not written through the link");
            assert!(!std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        }
        let left: Vec<_> =
            std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).filter(|n| n.to_string_lossy().ends_with(".partial")).collect();
        assert!(left.is_empty(), "{left:?}");
    }
}
