//! Streaming exporters from Arrow batches to text formats.
//!
//! Values use the same display formatting as the grid, so an export matches
//! what the user sees. JSON keeps numbers and booleans typed.

pub mod load;

use std::io::Write;

use databrain_connector_core::arrow::array::{Array, RecordBatch};
use databrain_connector_core::arrow::datatypes::{DataType, SchemaRef};
use databrain_connector_core::arrow::util::display::ArrayFormatter;
use databrain_connector_core::{ConnectorKind, quote_ident};
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
    /// CSV/TSV for spreadsheets: text starting with `=`, `+`, `-`, `@`
    /// (a formula when opened in Excel) gets a leading `'`. Numbers, also
    /// negative ones, are left alone.
    #[serde(default)]
    pub excel_safe: bool,
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
            excel_safe: false,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Number,
    Bool,
    Binary,
    /// Postgres array (`{1,2}` text): a JSON array, elements typed as `elem`.
    Array { elem: ArrayElem },
    Other,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ArrayElem {
    Number,
    Bool,
    Text,
}

fn kind_of(f: &databrain_connector_core::arrow::datatypes::Field) -> Kind {
    match f.data_type() {
        DataType::Boolean => Kind::Bool,
        t if t.is_numeric() => Kind::Number,
        DataType::Binary | DataType::LargeBinary | DataType::FixedSizeBinary(_) => Kind::Binary,
        _ if is_decimal_text(f) => Kind::Number,
        DataType::Utf8 | DataType::LargeUtf8 => match f.metadata().get(databrain_connector_core::value::META_DB_TYPE).and_then(|t| t.strip_suffix("[]")) {
            Some(base) => Kind::Array {
                elem: match base.split('(').next().unwrap_or("").trim() {
                    "smallint" | "integer" | "bigint" | "real" | "double precision" | "oid" => ArrayElem::Number,
                    b if databrain_result_store::display::is_decimal_type(b) => ArrayElem::Number,
                    "boolean" => ArrayElem::Bool,
                    _ => ArrayElem::Text,
                },
            },
            None => Kind::Other,
        },
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
            .map(|c| kind_of(schema.field(*c)))
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
                ExportFormat::Csv | ExportFormat::Tsv => {
                    if self.opts.excel_safe {
                        for (c, k) in cells.iter_mut().zip(&self.kinds) {
                            if let Some(s) = c.as_mut().filter(|s| *k != Kind::Number && is_formula_like(s)) {
                                s.insert(0, '\'');
                            }
                        }
                    }
                    let delim = if self.opts.format == ExportFormat::Csv { b',' } else { b'\t' };
                    write_delimited(&mut self.out, cells.iter().map(|c| c.as_deref()), delim)?
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

/// Text a spreadsheet would run as a formula. A number (`-12.5`, `+3`,
/// `-1e5`) is not: it opens as the number either way.
fn is_formula_like(s: &str) -> bool {
    s.starts_with(['=', '+', '-', '@', '\t', '\r']) && s.trim().parse::<f64>().is_err()
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
        Kind::Array { elem } => pg_array_json(s, elem).unwrap_or_else(|| serde_json::Value::String(s.to_string())),
        _ => serde_json::Value::String(s.to_string()),
    }
}

/// A Postgres array literal (`{1,2}`, `{{a,b},{"c d",NULL}}`) as JSON;
/// `None` when the text isn't one.
fn pg_array_json(s: &str, elem: ArrayElem) -> Option<serde_json::Value> {
    fn parse(b: &[u8], i: &mut usize, elem: ArrayElem) -> Option<serde_json::Value> {
        if b.get(*i) != Some(&b'{') {
            return None;
        }
        *i += 1;
        let mut out = Vec::new();
        if b.get(*i) == Some(&b'}') {
            *i += 1;
            return Some(serde_json::Value::Array(out));
        }
        loop {
            let v = match b.get(*i)? {
                b'{' => parse(b, i, elem)?,
                b'"' => {
                    *i += 1;
                    let mut t = Vec::new();
                    loop {
                        match *b.get(*i)? {
                            b'\\' => {
                                t.push(*b.get(*i + 1)?);
                                *i += 2;
                            }
                            b'"' => {
                                *i += 1;
                                break;
                            }
                            c => {
                                t.push(c);
                                *i += 1;
                            }
                        }
                    }
                    // Quoted: always the text (`"NULL"` is the word).
                    typed(&String::from_utf8(t).ok()?, elem)
                }
                _ => {
                    let start = *i;
                    while !matches!(b.get(*i), Some(b',') | Some(b'}') | None) {
                        *i += 1;
                    }
                    let t = std::str::from_utf8(&b[start..*i]).ok()?.trim();
                    if t.eq_ignore_ascii_case("NULL") { serde_json::Value::Null } else { typed(t, elem) }
                }
            };
            out.push(v);
            match *b.get(*i)? {
                b',' => *i += 1,
                b'}' => {
                    *i += 1;
                    return Some(serde_json::Value::Array(out));
                }
                _ => return None,
            }
        }
    }
    fn typed(t: &str, elem: ArrayElem) -> serde_json::Value {
        match elem {
            ArrayElem::Number => json_value(Some(t), Kind::Number),
            ArrayElem::Bool if t == "t" || t == "f" => serde_json::Value::Bool(t == "t"),
            _ => serde_json::Value::String(t.to_string()),
        }
    }
    // Arrays with custom bounds start with `[1:2]=`.
    let body = match s.find("={") {
        Some(p) if s.starts_with('[') => &s[p + 1..],
        _ => s,
    };
    let mut i = 0;
    let v = parse(body.as_bytes(), &mut i, elem)?;
    (i == body.len()).then_some(v)
}

/// A value for `INSERT … VALUES`, written the way the dialect reads it
/// (same rules as job loads: `crate::load`).
fn sql_value(cell: Option<&str>, kind: Kind, dialect: ConnectorKind) -> String {
    let lit = match kind {
        Kind::Number => load::Lit::Number,
        Kind::Bool => load::Lit::Bool,
        Kind::Binary => load::Lit::Binary,
        Kind::Array { .. } | Kind::Other => load::Lit::Text,
    };
    load::literal(cell, lit, dialect)
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
/// [`finish`]: FileWriter::finish
pub struct FileWriter {
    inner: Writer,
    rows: u64,
}

enum Writer {
    Parquet { w: Box<parquet::arrow::ArrowWriter<std::fs::File>>, cols: Vec<usize>, projected: SchemaRef },
    Xlsx { path: std::path::PathBuf, schema: SchemaRef, cols: Vec<usize>, opts: ExportOptions, batches: Vec<RecordBatch> },
    Text(Exporter<std::io::BufWriter<std::fs::File>>),
}

impl FileWriter {
    pub fn create(path: &std::path::Path, schema: SchemaRef, opts: ExportOptions) -> Result<Self> {
        let cols: Vec<usize> = if opts.columns.is_empty() {
            (0..schema.fields().len()).collect()
        } else {
            opts.columns.iter().copied().filter(|c| *c < schema.fields().len()).collect()
        };
        let inner = match opts.format {
            ExportFormat::Parquet => {
                let projected = std::sync::Arc::new(schema.project(&cols)?);
                let file = std::fs::File::create(path)?;
                let props = parquet::file::properties::WriterProperties::builder()
                    .set_compression(parquet::basic::Compression::ZSTD(Default::default()))
                    .build();
                Writer::Parquet { w: Box::new(parquet::arrow::ArrowWriter::try_new(file, projected.clone(), Some(props))?), cols, projected }
            }
            ExportFormat::Xlsx => Writer::Xlsx { path: path.to_path_buf(), schema, cols, opts, batches: Vec::new() },
            _ => Writer::Text(Exporter::new(std::io::BufWriter::new(std::fs::File::create(path)?), schema, opts)),
        };
        Ok(Self { inner, rows: 0 })
    }

    /// Rows written so far.
    pub fn rows(&self) -> u64 {
        self.rows
    }

    pub fn write(&mut self, b: &RecordBatch) -> Result<()> {
        match &mut self.inner {
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

    pub fn finish(self) -> Result<u64> {
        match self.inner {
            Writer::Parquet { w, .. } => {
                w.close()?;
                Ok(self.rows)
            }
            Writer::Xlsx { path, schema, cols, opts, batches } => write_xlsx(&path, &schema, &batches, &cols, &opts),
            Writer::Text(e) => e.finish(),
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
        .map(|c| kind_of(schema.field(*c)))
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

/// Save a result losslessly (Arrow schema incl. field metadata is embedded)
/// for pinned outputs. Written to a temp file and renamed, so a crash never
/// leaves a truncated snapshot.
pub fn write_snapshot(path: &std::path::Path, schema: SchemaRef, batches: &[RecordBatch]) -> Result<u64> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("parquet.tmp");
    let file = std::fs::File::create(&tmp)?;
    let props = parquet::file::properties::WriterProperties::builder()
        .set_compression(parquet::basic::Compression::ZSTD(Default::default()))
        .build();
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
    fn postgres_arrays_become_json_arrays() {
        use databrain_connector_core::value::META_DB_TYPE;
        let f = |n: &str, t: &str| Field::new(n, DataType::Utf8, true).with_metadata(std::collections::HashMap::from([(META_DB_TYPE.to_string(), t.to_string())]));
        let schema: SchemaRef = Arc::new(Schema::new(vec![f("ints", "integer[]"), f("tags", "text[]"), f("grid", "numeric(4,1)[]"), f("flags", "boolean[]"), f("odd", "text[]")]));
        let b = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(StringArray::from(vec!["{1,2,NULL}"])),
                Arc::new(StringArray::from(vec![r#"{a,"b c","NULL","q\"x"}"#])),
                Arc::new(StringArray::from(vec!["{{1.5,2},{3,4}}"])),
                Arc::new(StringArray::from(vec!["{t,f}"])),
                Arc::new(StringArray::from(vec!["not {an array"])),
            ],
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&export_to_string(schema, [&b], ExportOptions::new(ExportFormat::Json)).unwrap()).unwrap();
        assert_eq!(v[0]["ints"], serde_json::json!([1, 2, null]));
        assert_eq!(v[0]["tags"], serde_json::json!(["a", "b c", "NULL", "q\"x"]));
        assert_eq!(v[0]["grid"], serde_json::json!([[1.5, 2], [3, 4]]));
        assert_eq!(v[0]["flags"], serde_json::json!([true, false]));
        assert_eq!(v[0]["odd"], "not {an array", "kept as text when it isn't an array");
    }

    #[test]
    fn excel_safe_csv_escapes_formulas_not_numbers() {
        let schema: SchemaRef = Arc::new(Schema::new(vec![Field::new("t", DataType::Utf8, true), Field::new("n", DataType::Int64, true)]));
        let b = RecordBatch::try_new(
            schema.clone(),
            vec![Arc::new(StringArray::from(vec!["=1+2", "-12.5", "@sum(A1)", "+x", "plain"])), Arc::new(Int64Array::from(vec![-1, -2, 3, 4, 5]))],
        )
        .unwrap();
        let mut o = ExportOptions::new(ExportFormat::Csv);
        o.excel_safe = true;
        assert_eq!(export_to_string(schema.clone(), [&b], o).unwrap(), "t,n\n'=1+2,-1\n-12.5,-2\n'@sum(A1),3\n'+x,4\nplain,5\n");
        let plain = export_to_string(schema, [&b], ExportOptions::new(ExportFormat::Csv)).unwrap();
        assert!(plain.contains("\n=1+2,-1\n"), "off by default: data stays as it is");
    }

    #[test]
    fn decimal_types_of_every_database_stay_numbers() {
        for t in ["numeric(12,2)", "decimal", "NUMBER(10,2)", "NUMBER", "BIGNUMERIC", "money", "DECIMAL(38,10)"] {
            assert!(databrain_result_store::display::is_decimal_type(t), "{t}");
        }
        for t in ["varchar(10)", "numbers", "text[]"] {
            assert!(!databrain_result_store::display::is_decimal_type(t), "{t}");
        }
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
        assert_eq!(write_snapshot(&p, schema.clone(), &[b.clone()]).unwrap(), 2);
        let (s2, bs) = read_snapshot(&p).unwrap();
        assert_eq!(s2.field(0).metadata(), schema.field(0).metadata());
        assert_eq!(bs[0], b);
        assert!(!dir.path().join("s.parquet.tmp").exists());
    }
}
