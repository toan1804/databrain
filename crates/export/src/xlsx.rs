use databrain_connector_core::arrow::array::{Array, RecordBatch};
use databrain_connector_core::arrow::datatypes::SchemaRef;
use databrain_connector_core::arrow::util::display::ArrayFormatter;
use databrain_result_store::display::format_options;

use crate::file::XLSX_MAX_ROWS;
use crate::kind::{Kind, kind_of};
use crate::{ExportError, ExportOptions, Result};

pub(crate) fn write_xlsx(file: std::fs::File, schema: &SchemaRef, batches: &[RecordBatch], cols: &[usize], opts: &ExportOptions) -> Result<u64> {
    use rust_xlsxwriter::{Format, Workbook};
    let total: usize = batches.iter().map(|b| b.num_rows()).sum();
    if total + 1 > XLSX_MAX_ROWS {
        return Err(ExportError::Unsupported(format!("Excel supports at most {} rows; this export has {total}. Use CSV or Parquet.", XLSX_MAX_ROWS - 1)));
    }
    let mut wb = Workbook::new();
    let ws = wb.add_worksheet_with_constant_memory()?;
    ws.set_name(opts.table_name.as_deref().filter(|n| !n.is_empty()).unwrap_or("Result").chars().take(31).collect::<String>())?;
    let bold = Format::new().set_bold();
    let kinds: Vec<Kind> = cols.iter().map(|c| kind_of(schema.field(*c))).collect();
    let mut widths: Vec<usize> = cols.iter().map(|c| schema.field(*c).name().chars().count()).collect();
    for (j, c) in cols.iter().enumerate() {
        ws.write_string_with_format(0, j as u16, schema.field(*c).name(), &bold)?;
    }
    let fopts = format_options();
    let mut row = 1u32;
    for b in batches {
        let arrays: Vec<_> = cols.iter().map(|c| b.column(*c).clone()).collect();
        let fmts: Vec<ArrayFormatter<'_>> = arrays.iter().map(|a| ArrayFormatter::try_new(a.as_ref(), &fopts)).collect::<std::result::Result<_, _>>()?;
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
    wb.save_to_writer(file)?;
    Ok((row - 1) as u64)
}
