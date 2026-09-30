//! Streaming exporters from Arrow batches to text formats.
//!
//! Values use the same display formatting as the grid, so an export matches
//! what the user sees. JSON keeps numbers and booleans typed.

use std::io::Write;

use databrain_connector_core::arrow::array::{Array, RecordBatch};
use databrain_connector_core::arrow::datatypes::{DataType, SchemaRef};
use databrain_connector_core::arrow::util::display::ArrayFormatter;
use databrain_connector_core::{ConnectorKind, quote_ident, quote_literal};
use databrain_result_store::display::{format_options, is_decimal_text};
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("arrow: {0}")]
    Arrow(#[from] databrain_connector_core::arrow::error::ArrowError),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("parquet: {0}")]
    Parquet(#[from] parquet::errors::ParquetError),
    #[error("xlsx: {0}")]
    Xlsx(#[from] rust_xlsxwriter::XlsxError),
    #[error("{0}")]
    Unsupported(String),
}

pub type Result<T> = std::result::Result<T, ExportError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    Csv,
    Tsv,
    Json,
    Ndjson,
    Markdown,
    SqlInsert,
    /// Binary formats: written to files only (see [`export_file`]).
    Parquet,
    Xlsx,
}

impl ExportFormat {
    pub fn is_binary(self) -> bool {
        matches!(self, ExportFormat::Parquet | ExportFormat::Xlsx)
    }

    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Parquet => "parquet",
            ExportFormat::Xlsx => "xlsx",
            ExportFormat::Csv => "csv",
            ExportFormat::Tsv => "tsv",
            ExportFormat::Json => "json",
            ExportFormat::Ndjson => "ndjson",
            ExportFormat::Markdown => "md",
            ExportFormat::SqlInsert => "sql",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportOptions {
    pub format: ExportFormat,
    /// Write a header row (CSV/TSV).
    #[serde(default = "yes")]
    pub header: bool,
    /// Table name for SQL INSERT statements.
    #[serde(default)]
    pub table_name: Option<String>,
    /// Identifier quoting dialect for SQL INSERT.
    #[serde(default)]
    pub dialect: Option<ConnectorKind>,
    /// Restrict to these column indices (in order). Empty = all.
    #[serde(default)]
    pub columns: Vec<usize>,
}

fn yes() -> bool {
    true
}

impl ExportOptions {
    pub fn new(format: ExportFormat) -> Self {
        Self {
            format,
            header: true,
            table_name: None,
            dialect: None,
            columns: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Number,
    Bool,
    Binary,
    Other,
}

fn kind_of(dt: &DataType, decimal_text: bool) -> Kind {
    match dt {
        DataType::Boolean => Kind::Bool,
        t if t.is_numeric() => Kind::Number,
        DataType::Binary | DataType::LargeBinary | DataType::FixedSizeBinary(_) => Kind::Binary,
        _ if decimal_text => Kind::Number,
        _ => Kind::Other,
    }
}

/// Incremental writer: call [`Exporter::write_batch`] for each batch, then
/// [`Exporter::finish`].
pub struct Exporter<W: Write> {
    out: W,
    opts: ExportOptions,
    schema: SchemaRef,
    cols: Vec<usize>,
    kinds: Vec<Kind>,
    rows_written: u64,
    started: bool,
}

impl<W: Write> Exporter<W> {
    pub fn new(out: W, schema: SchemaRef, opts: ExportOptions) -> Self {
        let cols: Vec<usize> = if opts.columns.is_empty() {
            (0..schema.fields().len()).collect()
        } else {
            opts.columns
                .iter()
                .copied()
                .filter(|c| *c < schema.fields().len())
                .collect()
        };
        let kinds = cols
            .iter()
            .map(|c| {
                let f = schema.field(*c);
                kind_of(f.data_type(), is_decimal_text(f))
            })
            .collect();
        Self {
            out,
            opts,
            schema,
            cols,
            kinds,
            rows_written: 0,
            started: false,
        }
    }

    fn names(&self) -> Vec<String> {
        self.cols
            .iter()
            .map(|c| self.schema.field(*c).name().clone())
            .collect()
    }

    fn start(&mut self) -> Result<()> {
        if self.started {
            return Ok(());
        }
        self.started = true;
        let names = self.names();
        match self.opts.format {
            ExportFormat::Csv if self.opts.header => {
                write_delimited(&mut self.out, names.iter().map(|s| Some(s.as_str())), b',')?
            }
            ExportFormat::Tsv if self.opts.header => {
                write_delimited(&mut self.out, names.iter().map(|s| Some(s.as_str())), b'\t')?
            }
            ExportFormat::Json => self.out.write_all(b"[")?,
            ExportFormat::Markdown => {
                let head: Vec<String> = names.iter().map(|n| md_escape(n)).collect();
                writeln!(self.out, "| {} |", head.join(" | "))?;
                let sep: Vec<&str> = self
                    .kinds
                    .iter()
                    .map(|k| if *k == Kind::Number { "---:" } else { "---" })
                    .collect();
                writeln!(self.out, "| {} |", sep.join(" | "))?;
            }
            _ => {}
        }
        Ok(())
    }

    pub fn write_batch(&mut self, batch: &RecordBatch) -> Result<()> {
        self.start()?;
        let fopts = format_options();
        let arrays: Vec<_> = self.cols.iter().map(|c| batch.column(*c).clone()).collect();
        let fmts: Vec<ArrayFormatter<'_>> = arrays
            .iter()
            .map(|a| ArrayFormatter::try_new(a.as_ref(), &fopts))
            .collect::<std::result::Result<_, _>>()?;
        let names = self.names();
        let table = self.opts.table_name.clone().unwrap_or_else(|| "export".into());
        let dialect = self.opts.dialect.unwrap_or(ConnectorKind::Postgres);
        let insert_head = format!(
            "INSERT INTO {} ({}) VALUES (",
            table
                .split('.')
                .map(|p| quote_ident(dialect, p))
                .collect::<Vec<_>>()
                .join("."),
            names
                .iter()
                .map(|n| quote_ident(dialect, n))
                .collect::<Vec<_>>()
                .join(", ")
        );

        let mut cells: Vec<Option<String>> = Vec::with_capacity(arrays.len());
        for r in 0..batch.num_rows() {
            cells.clear();
            for (a, f) in arrays.iter().zip(&fmts) {
                cells.push(if a.is_null(r) {
                    None
                } else {
                    Some(f.value(r).to_string())
                });
            }
            match self.opts.format {
                ExportFormat::Csv => {
                    write_delimited(&mut self.out, cells.iter().map(|c| c.as_deref()), b',')?
                }
                ExportFormat::Tsv => {
                    write_delimited(&mut self.out, cells.iter().map(|c| c.as_deref()), b'\t')?
                }
                ExportFormat::Json | ExportFormat::Ndjson => {
                    let mut obj = serde_json::Map::with_capacity(cells.len());
                    for ((name, cell), kind) in names.iter().zip(&cells).zip(&self.kinds) {
                        obj.insert(name.clone(), json_value(cell.as_deref(), *kind));
                    }
                    if self.opts.format == ExportFormat::Json {
                        if self.rows_written > 0 {
                            self.out.write_all(b",")?;
                        }
                        self.out.write_all(b"\n  ")?;
                        serde_json::to_writer(&mut self.out, &obj)?;
                    } else {
                        serde_json::to_writer(&mut self.out, &obj)?;
                        self.out.write_all(b"\n")?;
                    }
                }
                ExportFormat::Markdown => {
                    let row: Vec<String> = cells
                        .iter()
                        .map(|c| c.as_deref().map(md_escape).unwrap_or_default())
                        .collect();
                    writeln!(self.out, "| {} |", row.join(" | "))?;
                }
                ExportFormat::Parquet | ExportFormat::Xlsx => {
                    return Err(ExportError::Unsupported(format!(
                        "{:?} is a binary format; use export_file",
                        self.opts.format
                    )));
                }
                ExportFormat::SqlInsert => {
                    let vals: Vec<String> = cells
                        .iter()
                        .zip(&self.kinds)
                        .map(|(c, k)| sql_value(c.as_deref(), *k, dialect))
                        .collect();
                    writeln!(self.out, "{insert_head}{});", vals.join(", "))?;
                }
            }
            self.rows_written += 1;
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<u64> {
        self.start()?;
        if self.opts.format == ExportFormat::Json {
            self.out
                .write_all(if self.rows_written > 0 { b"\n]\n" } else { b"]\n" })?;
        }
        self.out.flush()?;
        Ok(self.rows_written)
    }
}

/// Export batches to a writer in one call.
pub fn export<'a, W: Write>(
    out: W,
    schema: SchemaRef,
    batches: impl IntoIterator<Item = &'a RecordBatch>,
    opts: ExportOptions,
) -> Result<u64> {
    let mut e = Exporter::new(out, schema, opts);
    for b in batches {
        e.write_batch(b)?;
    }
    e.finish()
}

/// Export to a `String` (clipboard copy).
pub fn export_to_string<'a>(
    schema: SchemaRef,
    batches: impl IntoIterator<Item = &'a RecordBatch>,
    opts: ExportOptions,
) -> Result<String> {
    let mut buf = Vec::new();
    export(&mut buf, schema, batches, opts)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn write_delimited<'a, W: Write>(
    out: &mut W,
    cells: impl Iterator<Item = Option<&'a str>>,
    delim: u8,
) -> Result<()> {
    let mut first = true;
    for c in cells {
        if !first {
            out.write_all(&[delim])?;
        }
        first = false;
        let Some(s) = c else { continue };
        if delim == b'\t' {
            // TSV: no quoting; replace control characters that break rows.
            let clean: String = s
                .chars()
                .map(|ch| if matches!(ch, '\t' | '\n' | '\r') { ' ' } else { ch })
                .collect();
            out.write_all(clean.as_bytes())?;
        } else if s.contains([',', '"', '\n', '\r']) || s.starts_with(' ') || s.ends_with(' ') {
            out.write_all(b"\"")?;
            out.write_all(s.replace('"', "\"\"").as_bytes())?;
            out.write_all(b"\"")?;
        } else {
            out.write_all(s.as_bytes())?;
        }
    }
    out.write_all(b"\n")?;
    Ok(())
}

fn md_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('|', "\\|")
        .replace(['\n', '\r'], " ")
}

fn json_value(cell: Option<&str>, kind: Kind) -> serde_json::Value {
    let Some(s) = cell else {
        return serde_json::Value::Null;
    };
    match kind {
        Kind::Bool => serde_json::Value::Bool(s == "true"),
        Kind::Number => {
            if let Ok(i) = s.parse::<i64>() {
                i.into()
            } else if let Ok(u) = s.parse::<u64>() {
                u.into()
            } else {
                // Emit a JSON number only when f64 represents it without
                // precision loss (<= 15 significant digits); otherwise keep
                // the exact text (large decimals, NaN, Infinity).
                let digits = s.chars().filter(|c| c.is_ascii_digit()).count();
                match s.parse::<f64>().ok().and_then(serde_json::Number::from_f64) {
                    Some(n) if digits <= 15 => serde_json::Value::Number(n),
                    _ => serde_json::Value::String(s.to_string()),
                }
            }
        }
        _ => serde_json::Value::String(s.to_string()),
    }
}

fn sql_value(cell: Option<&str>, kind: Kind, dialect: ConnectorKind) -> String {
    let Some(s) = cell else {
        return "NULL".into();
    };
    match kind {
        Kind::Number if s.parse::<f64>().is_ok_and(|f| f.is_finite()) => s.to_string(),
        Kind::Bool => match dialect {
            ConnectorKind::Postgres => s.to_uppercase(),
            _ => if s == "true" { "1".into() } else { "0".into() },
        },
        Kind::Binary => match dialect {
            ConnectorKind::Postgres => format!("'\\x{s}'::bytea"),
            _ => format!("X'{s}'"),
        },
        _ => quote_literal(s),
    }
}

/// Excel's row limit (including the header row).
pub const XLSX_MAX_ROWS: usize = 1_048_576;

/// Write batches to a file in any format. Parquet keeps Arrow types (exact
/// decimals stay text); XLSX writes numbers/booleans natively, streaming rows
/// with constant memory. Returns rows written.
pub fn export_file(
    path: &std::path::Path,
    schema: SchemaRef,
    batches: &[RecordBatch],
    opts: ExportOptions,
) -> Result<u64> {
    let cols: Vec<usize> = if opts.columns.is_empty() {
        (0..schema.fields().len()).collect()
    } else {
        opts.columns.iter().copied().filter(|c| *c < schema.fields().len()).collect()
    };
    match opts.format {
        ExportFormat::Parquet => {
            let projected = std::sync::Arc::new(schema.project(&cols)?);
            let file = std::fs::File::create(path)?;
            let props = parquet::file::properties::WriterProperties::builder()
                .set_compression(parquet::basic::Compression::ZSTD(Default::default()))
                .build();
            let mut w = parquet::arrow::ArrowWriter::try_new(file, projected.clone(), Some(props))?;
            let mut n = 0u64;
            for b in batches {
                let pb = b.project(&cols)?;
                // Re-attach the projected schema (keeps field metadata).
                let pb = RecordBatch::try_new(projected.clone(), pb.columns().to_vec())?;
                n += pb.num_rows() as u64;
                w.write(&pb)?;
            }
            w.close()?;
            Ok(n)
        }
        ExportFormat::Xlsx => write_xlsx(path, &schema, batches, &cols, &opts),
        _ => {
            let file = std::fs::File::create(path)?;
            let mut e = Exporter::new(std::io::BufWriter::new(file), schema, opts);
            for b in batches {
                e.write_batch(b)?;
            }
            e.finish()
        }
    }
}

fn write_xlsx(
    path: &std::path::Path,
    schema: &SchemaRef,
    batches: &[RecordBatch],
    cols: &[usize],
    opts: &ExportOptions,
) -> Result<u64> {
    use rust_xlsxwriter::{Format, Workbook};
    let total: usize = batches.iter().map(|b| b.num_rows()).sum();
    if total + 1 > XLSX_MAX_ROWS {
        return Err(ExportError::Unsupported(format!(
            "Excel supports at most {} rows; this export has {total}. Use CSV or Parquet.",
            XLSX_MAX_ROWS - 1
        )));
    }
    let mut wb = Workbook::new();
    let ws = wb.add_worksheet_with_constant_memory();
    ws.set_name(opts.table_name.as_deref().filter(|n| !n.is_empty()).unwrap_or("Result").chars().take(31).collect::<String>())?;
    let bold = Format::new().set_bold();
    let kinds: Vec<Kind> = cols
        .iter()
        .map(|c| {
            let f = schema.field(*c);
            kind_of(f.data_type(), is_decimal_text(f))
        })
        .collect();
    let mut widths: Vec<usize> = cols.iter().map(|c| schema.field(*c).name().chars().count()).collect();
    for (j, c) in cols.iter().enumerate() {
        ws.write_string_with_format(0, j as u16, schema.field(*c).name(), &bold)?;
    }
    let fopts = format_options();
    let mut row = 1u32;
    for b in batches {
        let arrays: Vec<_> = cols.iter().map(|c| b.column(*c).clone()).collect();
        let fmts: Vec<ArrayFormatter<'_>> = arrays
            .iter()
            .map(|a| ArrayFormatter::try_new(a.as_ref(), &fopts))
            .collect::<std::result::Result<_, _>>()?;
        for r in 0..b.num_rows() {
            for (j, (a, f)) in arrays.iter().zip(&fmts).enumerate() {
                if a.is_null(r) {
                    continue;
                }
                let text = f.value(r).to_string();
                let col = j as u16;
                widths[j] = widths[j].max(text.chars().count().min(60));
                match kinds[j] {
                    Kind::Number => match text.parse::<f64>() {
                        // Excel keeps 15 significant digits; longer values stay text.
                        Ok(v) if v.is_finite() && text.chars().filter(|c| c.is_ascii_digit()).count() <= 15 => {
                            ws.write_number(row, col, v)?;
                        }
                        _ => {
                            ws.write_string(row, col, &text)?;
                        }
                    },
                    Kind::Bool => {
                        ws.write_boolean(row, col, text == "true")?;
                    }
                    _ => {
                        // Excel cells hold at most 32,767 characters.
                        let t: String = text.chars().take(32_767).collect();
                        ws.write_string(row, col, &t)?;
                    }
                }
            }
            row += 1;
        }
    }
    for (j, w) in widths.iter().enumerate() {
        ws.set_column_width(j as u16, (*w as f64 + 2.0).clamp(6.0, 62.0))?;
    }
    ws.set_freeze_panes(1, 0)?;
    if !cols.is_empty() {
        ws.autofilter(0, 0, row.saturating_sub(1).max(1), cols.len() as u16 - 1)?;
    }
    wb.save(path)?;
    Ok((row - 1) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use databrain_connector_core::arrow::array::{BinaryArray, BooleanArray, Int64Array, StringArray};
    use databrain_connector_core::arrow::datatypes::{Field, Schema};
    use std::sync::Arc;

    fn batch() -> RecordBatch {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, true),
            Field::new("name", DataType::Utf8, true),
            Field::new("ok", DataType::Boolean, true),
            Field::new("raw", DataType::Binary, true),
        ]));
        RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int64Array::from(vec![Some(1), None])),
                Arc::new(StringArray::from(vec![Some("a,\"b\""), Some("it's|x")])),
                Arc::new(BooleanArray::from(vec![Some(true), None])),
                Arc::new(BinaryArray::from(vec![Some(&b"\x00\xff"[..]), None])),
            ],
        )
        .unwrap()
    }

    fn run(format: ExportFormat) -> String {
        let b = batch();
        let mut o = ExportOptions::new(format);
        o.table_name = Some("public.t".into());
        export_to_string(b.schema(), [&b], o).unwrap()
    }

    #[test]
    fn csv() {
        assert_eq!(
            run(ExportFormat::Csv),
            "id,name,ok,raw\n1,\"a,\"\"b\"\"\",true,00ff\n,it's|x,,\n"
        );
    }

    #[test]
    fn json_typed() {
        let v: serde_json::Value = serde_json::from_str(&run(ExportFormat::Json)).unwrap();
        assert_eq!(v[0]["id"], 1);
        assert_eq!(v[0]["ok"], true);
        assert_eq!(v[1]["id"], serde_json::Value::Null);
        assert_eq!(v[0]["name"], "a,\"b\"");
        assert_eq!(run(ExportFormat::Ndjson).lines().count(), 2);
    }

    #[test]
    fn empty_json_is_valid() {
        let b = batch().slice(0, 0);
        let s = export_to_string(b.schema(), [&b], ExportOptions::new(ExportFormat::Json)).unwrap();
        assert_eq!(serde_json::from_str::<serde_json::Value>(&s).unwrap(), serde_json::json!([]));
    }

    #[test]
    fn sql_and_markdown() {
        let s = run(ExportFormat::SqlInsert);
        assert!(s.starts_with(
            "INSERT INTO \"public\".\"t\" (\"id\", \"name\", \"ok\", \"raw\") VALUES (1, 'a,\"b\"', TRUE, '\\x00ff'::bytea);"
        ), "{s}");
        assert!(s.contains("(NULL, 'it''s|x', NULL, NULL);"));
        let m = run(ExportFormat::Markdown);
        assert!(m.contains("| id | name | ok | raw |\n| ---: | --- | --- | --- |"));
        assert!(m.contains("it's\\|x"));
    }

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
}
