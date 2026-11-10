use databrain_connector_core::{ConnectionConfig, ConnectorError, ConnectorKind, ErrorKind, Result, quote_ident, quote_literal};

use crate::engine::load_extension;
use crate::error::map_err;
use crate::excel::{excel_scan, excel_sources_with};

/// How a file is read, from its extension (or directory layout).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileFormat {
    Csv,
    Parquet,
    Json,
    Excel,
    Delta,
    Iceberg,
}

pub fn detect_format(path: &str) -> Option<FileFormat> {
    let p = std::path::Path::new(path.trim_end_matches(['/', '\\']));
    if p.is_dir() {
        if p.join("_delta_log").is_dir() {
            return Some(FileFormat::Delta);
        }
        if p.join("metadata").is_dir() {
            return Some(FileFormat::Iceberg);
        }
        // A directory of parquet files.
        return Some(FileFormat::Parquet);
    }
    let lower = path.to_ascii_lowercase();
    let lower = lower.trim_end_matches(".gz").trim_end_matches(".zst");
    let ext = lower.rsplit('.').next()?;
    Some(match ext {
        "csv" | "tsv" | "txt" | "psv" => FileFormat::Csv,
        "parquet" | "pq" => FileFormat::Parquet,
        "json" | "ndjson" | "jsonl" => FileFormat::Json,
        "xlsx" => FileFormat::Excel,
        _ => return None,
    })
}

/// The delimiter a file's extension promises (`.tsv`: tab, `.psv`: `|`,
/// also gzipped), as DuckDB's `delim` option spells it; `None` to sniff.
fn delimiter_of(path: &str) -> Option<&'static str> {
    let lower = path.to_ascii_lowercase();
    let lower = lower.trim_end_matches(".gz").trim_end_matches(".zst");
    match lower.rsplit('.').next()? {
        "tsv" => Some("\\t"),
        "psv" => Some("|"),
        _ => None,
    }
}

/// SQL expression that reads the file (used in views and suggestions).
pub fn scan_expr(path: &str, format: FileFormat) -> String {
    let lit = quote_literal(path);
    let dir_glob = || {
        let base = path.trim_end_matches(['/', '\\']);
        quote_literal(&format!("{base}/**/*.parquet"))
    };
    match format {
        // DuckDB's sniffer guesses the delimiter, and picks `,` for a TSV
        // whose values contain commas: the extension says which it is.
        FileFormat::Csv => match delimiter_of(path) {
            Some(d) => format!("read_csv({lit}, delim = '{d}')"),
            None => format!("read_csv({lit})"),
        },
        FileFormat::Parquet if std::path::Path::new(path).is_dir() => format!("read_parquet({}, hive_partitioning = true)", dir_glob()),
        FileFormat::Parquet => format!("read_parquet({lit})"),
        FileFormat::Json => format!("read_json_auto({lit})"),
        FileFormat::Excel => format!("read_xlsx({lit})"),
        FileFormat::Delta => format!("delta_scan({lit})"),
        FileFormat::Iceberg => format!("iceberg_scan({lit}, allow_moved_paths = true)"),
    }
}

pub(crate) fn required_extension(f: FileFormat) -> Option<&'static str> {
    match f {
        FileFormat::Excel => Some("excel"),
        FileFormat::Delta => Some("delta"),
        FileFormat::Iceberg => Some("iceberg"),
        _ => None,
    }
}

/// View name for an attached file: file stem, sanitized, de-duplicated.
pub fn view_name(path: &str, taken: &[String]) -> String {
    let stem = std::path::Path::new(path.trim_end_matches(['/', '\\'])).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "file".into());
    let stem = stem.split('.').next().unwrap_or(&stem).to_string();
    let mut base: String = stem.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' }).collect();
    if base.is_empty() || base.starts_with(|c: char| c.is_ascii_digit()) {
        base = format!("f_{base}");
    }
    let mut name = base.clone();
    let mut i = 2;
    while taken.contains(&name) {
        name = format!("{base}_{i}");
        i += 1;
    }
    name
}

pub(crate) fn files_of(cfg: &ConnectionConfig) -> Vec<String> {
    cfg.opt("files").map(|f| f.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect()).unwrap_or_default()
}

/// Create `files.<name>` views over attached files. Unreadable files are
/// skipped with a comment on the view list rather than failing the connection.
pub(crate) fn attach_files(conn: &duckdb::Connection, files: &[String], all_sheets: bool) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }
    conn.execute_batch("CREATE SCHEMA IF NOT EXISTS files").map_err(map_err)?;
    let mut taken = Vec::new();
    for f in files {
        let Some(fmt) = detect_format(f) else { continue };
        if let Some(ext) = required_extension(fmt) {
            load_extension(conn, ext);
        }
        // (view name, source, comment): one per sheet for Excel with all sheets.
        let mut views: Vec<(String, String, String)> = Vec::new();
        if fmt == FileFormat::Excel {
            let srcs = excel_sources_with(conn, f, all_sheets).unwrap_or_else(|_| vec![(String::new(), excel_scan(conn, f, None))]);
            let many = srcs.len() > 1;
            for (i, (sheet, src)) in srcs.into_iter().enumerate() {
                // First sheet keeps the file's name; others are <file>_<sheet>.
                let safe: String = sheet.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
                let base = if many && i > 0 { format!("{}_{safe}", f.trim_end_matches(".xlsx").trim_end_matches(".XLSX")) } else { f.clone() };
                let name = view_name(&base, &taken);
                taken.push(name.clone());
                let note = if sheet.is_empty() { format!("File: {f}") } else { format!("File: {f} · sheet {sheet}") };
                views.push((name, src, note));
            }
        } else {
            let name = view_name(f, &taken);
            taken.push(name.clone());
            views.push((name, scan_expr(f, fmt), format!("File: {f}")));
        }
        for (name, src, note) in views {
            let sql = format!("CREATE OR REPLACE VIEW files.{} AS SELECT * FROM {src}", quote_ident(ConnectorKind::Duckdb, &name));
            if let Err(e) = conn.execute_batch(&sql) {
                return Err(ConnectorError::new(ErrorKind::Config, format!("cannot read {f}: {e}")));
            }
            let _ = conn.execute_batch(&format!("COMMENT ON VIEW files.{} IS {}", quote_ident(ConnectorKind::Duckdb, &name), quote_literal(&note)));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DuckdbConnector;
    use crate::test_util::*;
    use databrain_auth::{AuthMethod, InlineCredentialSource};
    use databrain_connector_core::arrow::array::AsArray;
    use databrain_connector_core::arrow::datatypes::Int64Type;

    use databrain_connector_core::{ConnectionConfig, Connector, ConnectorKind};
    use std::sync::Arc;

    #[test]
    fn detects_formats_and_names() {
        assert_eq!(detect_format("/x/sales.CSV"), Some(FileFormat::Csv));
        assert_eq!(detect_format("/x/a.jsonl.gz"), Some(FileFormat::Json));
        assert_eq!(detect_format("/x/a.xlsx"), Some(FileFormat::Excel));
        assert_eq!(detect_format("/x/a.bin"), None);
        assert_eq!(view_name("/x/2024 Sales.csv", &[]), "f_2024_sales");
        assert_eq!(view_name("/x/sales.csv", &["sales".into()]), "sales_2");
        assert_eq!(scan_expr("/x/it's.csv", FileFormat::Csv), "read_csv('/x/it''s.csv')");
        assert_eq!(scan_expr("/x/a.TSV", FileFormat::Csv), "read_csv('/x/a.TSV', delim = '\\t')");
        assert_eq!(scan_expr("/x/a.tsv.gz", FileFormat::Csv), "read_csv('/x/a.tsv.gz', delim = '\\t')");
        assert_eq!(scan_expr("/x/a.psv", FileFormat::Csv), "read_csv('/x/a.psv', delim = '|')");
        assert_eq!(scan_expr("/x/a.txt", FileFormat::Csv), "read_csv('/x/a.txt')");
    }

    #[tokio::test]
    async fn queries_csv_parquet_json_files() {
        let dir = tempfile::tempdir().unwrap();
        let csv = dir.path().join("sales.csv");
        std::fs::write(&csv, "region,amount,day\nEU,10,2024-01-02\nUS,5,2024-01-03\nEU,2.5,2024-01-04\n").unwrap();
        let json = dir.path().join("events.ndjson");
        std::fs::write(&json, "{\"id\":1,\"kind\":\"a\"}\n{\"id\":2,\"kind\":\"b\"}\n").unwrap();
        let parquet = dir.path().join("out.parquet");

        let mut cfg = ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None);
        cfg.options.insert("files".into(), format!("{}\n{}", csv.display(), json.display()));
        let s = session(cfg).await;

        // Attached files appear in the explorer as views.
        let schemas = s.list_schemas().await.unwrap();
        let files = schemas.iter().find(|x| x.name.ends_with(".files")).unwrap();
        let files_schema = files.name.clone();
        // Three-level names: database (catalog) → schema.
        assert_eq!(files.name, format!("{}.files", files.catalog.as_deref().unwrap()));
        let hits = s.search_objects("ale", 10).await.unwrap();
        assert_eq!(hits.iter().map(|o| (o.schema.as_str(), o.name.as_str())).collect::<Vec<_>>(), vec![(files_schema.as_str(), "sales")]);
        assert_eq!(s.search_objects("files.ev", 10).await.unwrap()[0].name, "events");
        assert!(s.search_objects("x'; drop table t; --", 10).await.unwrap().is_empty());
        let objs = s.list_objects(&files_schema).await.unwrap();
        assert_eq!(objs.iter().map(|o| o.name.as_str()).collect::<Vec<_>>(), vec!["events", "sales"]);
        let d = s.describe(&files_schema, "sales").await.unwrap();
        assert_eq!(d.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["region", "amount", "day"]);
        assert!(d.columns[2].data_type.contains("DATE"));
        assert!(s.schema_columns(&files_schema).await.unwrap().len() == 2);

        let r = q(s.as_ref(), "select region, sum(amount) s from files.sales group by 1 order by 1").await;
        assert_eq!(r.num_rows(), 2);
        // Direct file query + write Parquet + read it back.
        q(s.as_ref(), &format!("copy (select * from '{}') to '{}' (format parquet)", csv.display(), parquet.display())).await;
        let r = q(s.as_ref(), &format!("select count(*) n from read_parquet('{}')", parquet.display())).await;
        assert_eq!(r.batches[0].column(0).as_primitive::<Int64Type>().value(0), 3);
        let r = q(s.as_ref(), "select * from files.events").await;
        assert_eq!(r.num_rows(), 2);
        // DDL/DML report affected rows.
        let r = q(s.as_ref(), "create table t as select * from range(5)").await;
        assert!(r.schema.is_none());
        let r = q(s.as_ref(), "delete from t where range < 2").await;
        assert_eq!(r.summary.rows_affected, Some(2));
        // Empty result still has a schema.
        let r = q(s.as_ref(), "select * from t where false").await;
        assert_eq!(r.schema.unwrap().fields().len(), 1);
    }

    #[tokio::test]
    async fn reads_tsv_files_by_tab_even_with_commas_in_values() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None);
        let s = DuckdbConnector::new().connect(&cfg, Arc::new(InlineCredentialSource::new(AuthMethod::None, None))).await.unwrap();
        for (name, body, cols, rows) in [
            ("names.tsv", "name, full\tnote, text\nSmith, John\tlikes a, b\nDoe, Jane\tx, y\n", vec!["name, full", "note, text"], 2),
            ("cities.tsv", "id\tcity\n1\tHanoi, VN\n2\tParis, FR\n", vec!["id", "city"], 2),
            ("one.tsv", "only\nx\ny\n", vec!["only"], 2),
            ("pipes.psv", "a|b\n1, 2|3\n", vec!["a", "b"], 1),
            ("plain.csv", "a,b\n1,2\n", vec!["a", "b"], 1),
        ] {
            let p = dir.path().join(name);
            std::fs::write(&p, body).unwrap();
            let r = q(s.as_ref(), &format!("select * from {}", scan_expr(&p.to_string_lossy(), FileFormat::Csv))).await;
            let names: Vec<String> = r.schema.clone().unwrap().fields().iter().map(|f| f.name().clone()).collect();
            assert_eq!(names, cols, "{name}");
            assert_eq!(r.num_rows(), rows, "{name}");
        }
    }

    #[tokio::test]
    async fn bad_attached_file_is_config_error() {
        let mut cfg = ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None);
        cfg.options.insert("files".into(), "/definitely/missing.csv".into());
        let e = DuckdbConnector::new().connect(&cfg, Arc::new(InlineCredentialSource::new(AuthMethod::None, None))).await.err().unwrap();
        assert_eq!(e.kind, ErrorKind::Config);
        assert!(e.message.contains("missing.csv"));
    }
}
