use databrain_connector_core::arrow::array::RecordBatch;
use databrain_connector_core::arrow::datatypes::SchemaRef;

use crate::Result;

/// Save a result losslessly (Arrow schema incl. field metadata is embedded)
/// for pinned outputs. Written to a temp file and renamed, so a crash never
/// leaves a truncated snapshot.
pub fn write_snapshot(path: &std::path::Path, schema: SchemaRef, batches: &[RecordBatch]) -> Result<u64> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("parquet.tmp");
    let file = std::fs::File::create(&tmp)?;
    let props = parquet::file::properties::WriterProperties::builder().set_compression(parquet::basic::Compression::ZSTD(Default::default())).build();
    let mut w = parquet::arrow::ArrowWriter::try_new(file, schema, Some(props))?;
    let mut rows = 0u64;
    for b in batches {
        rows += b.num_rows() as u64;
        w.write(b)?;
    }
    w.close()?;
    std::fs::rename(&tmp, path)?;
    Ok(rows)
}

/// Read a snapshot written by [`write_snapshot`].
pub fn read_snapshot(path: &std::path::Path) -> Result<(SchemaRef, Vec<RecordBatch>)> {
    let file = std::fs::File::open(path)?;
    let builder = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)?;
    let schema = builder.schema().clone();
    let reader = builder.with_batch_size(8192).build()?;
    let batches = reader.collect::<std::result::Result<Vec<_>, _>>()?;
    Ok((schema, batches))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::*;

    #[test]
    fn snapshots_keep_database_types() {
        use databrain_connector_core::value::META_DB_TYPE;
        let md = std::collections::HashMap::from([(META_DB_TYPE.to_string(), "numeric(12,2)".to_string())]);
        let schema: SchemaRef = Arc::new(Schema::new(vec![Field::new("amount", DataType::Utf8, true).with_metadata(md)]));
        let b = RecordBatch::try_new(schema.clone(), vec![Arc::new(StringArray::from(vec!["1.50"]))]).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.parquet");
        write_snapshot(&p, schema, &[b]).unwrap();
        let (back, _) = read_snapshot(&p).unwrap();
        assert_eq!(back.field(0).metadata().get(META_DB_TYPE).map(String::as_str), Some("numeric(12,2)"));
    }

    #[test]
    fn snapshot_roundtrip_keeps_metadata() {
        use databrain_connector_core::arrow::array::StringArray;
        use databrain_connector_core::arrow::datatypes::{DataType, Field, Schema};
        let mut md = std::collections::HashMap::new();
        md.insert("databrain.db_type".to_string(), "numeric".to_string());
        let schema = Arc::new(Schema::new(vec![Field::new("amount", DataType::Utf8, true).with_metadata(md)]));
        let b = RecordBatch::try_new(schema.clone(), vec![Arc::new(StringArray::from(vec![Some("1.50"), None]))]).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.parquet");
        assert_eq!(write_snapshot(&p, schema.clone(), std::slice::from_ref(&b)).unwrap(), 2);
        let (s2, bs) = read_snapshot(&p).unwrap();
        assert_eq!(s2.field(0).metadata(), schema.field(0).metadata());
        assert_eq!(bs[0], b);
        assert!(!dir.path().join("s.parquet.tmp").exists());
    }
}
