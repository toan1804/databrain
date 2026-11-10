use std::io::Write;

use crate::delimited::{is_formula_like, write_delimited};
use crate::json::json_value;
use crate::kind::{Kind, kind_of};
use crate::load::{Lit, lit_kind, literal};
use crate::markdown::md_escape;
use crate::{ExportError, ExportFormat, ExportOptions, Result};

use databrain_connector_core::arrow::array::{Array, RecordBatch};
use databrain_connector_core::arrow::datatypes::SchemaRef;
use databrain_connector_core::arrow::util::display::ArrayFormatter;
use databrain_connector_core::{ConnectorKind, quote_ident};
use databrain_result_store::display::format_options;

/// Incremental writer: call [`Exporter::write_batch`] for each batch, then
/// [`Exporter::finish`].
pub struct Exporter<W: Write> {
    out: W,
    opts: ExportOptions,
    schema: SchemaRef,
    cols: Vec<usize>,
    kinds: Vec<Kind>,
    /// How each column is written in SQL INSERTs (same as loads).
    lits: Vec<Lit>,
    rows_written: u64,
    started: bool,
}

impl<W: Write> Exporter<W> {
    pub fn new(out: W, schema: SchemaRef, opts: ExportOptions) -> Self {
        let cols: Vec<usize> = if opts.columns.is_empty() {
            (0..schema.fields().len()).collect()
        } else {
            opts.columns.iter().copied().filter(|c| *c < schema.fields().len()).collect()
        };
        let kinds = cols.iter().map(|c| kind_of(schema.field(*c))).collect();
        let lits = cols.iter().map(|c| lit_kind(schema.field(*c))).collect();
        Self { out, opts, schema, cols, kinds, lits, rows_written: 0, started: false }
    }

    fn names(&self) -> Vec<String> {
        self.cols.iter().map(|c| self.schema.field(*c).name().clone()).collect()
    }

    fn start(&mut self) -> Result<()> {
        if self.started {
            return Ok(());
        }
        self.started = true;
        let mut names = self.names();
        if self.opts.excel_safe && matches!(self.opts.format, ExportFormat::Csv | ExportFormat::Tsv) {
            names.iter_mut().for_each(excel_escape);
        }
        match self.opts.format {
            ExportFormat::Csv if self.opts.header => write_delimited(&mut self.out, names.iter().map(|s| Some(s.as_str())), b',')?,
            ExportFormat::Tsv if self.opts.header => write_delimited(&mut self.out, names.iter().map(|s| Some(s.as_str())), b'\t')?,
            ExportFormat::Json => self.out.write_all(b"[")?,
            ExportFormat::Markdown => {
                let head: Vec<String> = names.iter().map(|n| md_escape(n)).collect();
                writeln!(self.out, "| {} |", head.join(" | "))?;
                let sep: Vec<&str> = self.kinds.iter().map(|k| if *k == Kind::Number { "---:" } else { "---" }).collect();
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
        let fmts: Vec<ArrayFormatter<'_>> = arrays.iter().map(|a| ArrayFormatter::try_new(a.as_ref(), &fopts)).collect::<std::result::Result<_, _>>()?;
        let names = self.names();
        let table = self.opts.table_name.clone().unwrap_or_else(|| "export".into());
        let dialect = self.opts.dialect.unwrap_or(ConnectorKind::Postgres);
        let insert_head = format!(
            "INSERT INTO {} ({}) VALUES (",
            table.split('.').map(|p| quote_ident(dialect, p)).collect::<Vec<_>>().join("."),
            names.iter().map(|n| quote_ident(dialect, n)).collect::<Vec<_>>().join(", ")
        );

        let mut cells: Vec<Option<String>> = Vec::with_capacity(arrays.len());
        for r in 0..batch.num_rows() {
            cells.clear();
            for (a, f) in arrays.iter().zip(&fmts) {
                cells.push(if a.is_null(r) { None } else { Some(f.value(r).to_string()) });
            }
            match self.opts.format {
                ExportFormat::Csv | ExportFormat::Tsv => {
                    if self.opts.excel_safe {
                        // Also number columns: a plain number is no formula, but `-inf` is.
                        cells.iter_mut().flatten().for_each(excel_escape);
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
                    let row: Vec<String> = cells.iter().map(|c| c.as_deref().map(md_escape).unwrap_or_default()).collect();
                    writeln!(self.out, "| {} |", row.join(" | "))?;
                }
                ExportFormat::Parquet | ExportFormat::Xlsx => {
                    return Err(ExportError::Unsupported(format!("{:?} is a binary format; use export_file", self.opts.format)));
                }
                ExportFormat::SqlInsert => {
                    let vals: Vec<String> = cells.iter().zip(&self.lits).map(|(c, k)| literal(c.as_deref(), *k, dialect)).collect();
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
            self.out.write_all(if self.rows_written > 0 { b"\n]\n" } else { b"]\n" })?;
        }
        self.out.flush()?;
        Ok(self.rows_written)
    }
}

/// A leading `'` on text a spreadsheet would run as a formula (Excel safe).
fn excel_escape(s: &mut String) {
    if is_formula_like(s) {
        s.insert(0, '\'');
    }
}

/// Export batches to a writer in one call.
pub fn export<'a, W: Write>(out: W, schema: SchemaRef, batches: impl IntoIterator<Item = &'a RecordBatch>, opts: ExportOptions) -> Result<u64> {
    let mut e = Exporter::new(out, schema, opts);
    for b in batches {
        e.write_batch(b)?;
    }
    e.finish()
}

/// Export to a `String` (clipboard copy).
pub fn export_to_string<'a>(schema: SchemaRef, batches: impl IntoIterator<Item = &'a RecordBatch>, opts: ExportOptions) -> Result<String> {
    let mut buf = Vec::new();
    export(&mut buf, schema, batches, opts)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}
