use databrain_connector_core::{ConnectorError, ConnectorKind, ErrorKind, Result, quote_ident, quote_literal};

use crate::engine::{engine, extension_dir, load_extension};
use crate::error::map_err;
use crate::files::{FileFormat, scan_expr};
use crate::xlsx;

/// Like [`scan_expr`], but Excel files are read with [`excel_scan`] (the
/// first sheet with data). Opens a private in-memory DuckDB for the checks.
pub fn file_scan_expr(path: &str, format: FileFormat) -> String {
    if format != FileFormat::Excel {
        return scan_expr(path, format);
    }
    match excel_sources(path, false) {
        Ok(mut v) if !v.is_empty() => v.remove(0).sql,
        _ => scan_expr(path, format),
    }
}

/// Private DuckDB with the Excel extension loaded.
pub(crate) fn excel_conn() -> Result<duckdb::Connection> {
    engine()?;
    let conn = duckdb::Connection::open_in_memory().map_err(map_err)?;
    if let Some(dir) = extension_dir() {
        let _ = conn.execute_batch(&format!("SET extension_directory = {}", quote_literal(&dir)));
    }
    load_extension(&conn, "excel");
    Ok(conn)
}

/// One sheet to read.
#[derive(Debug, Clone)]
pub struct ExcelSource {
    pub sheet: String,
    /// DuckDB SQL that reads it, for `FROM`.
    pub sql: String,
    /// Cells with values left out of the read (title rows, stray cells far
    /// from the table), as "reads A3:F30 of A1:KO5001".
    pub left_out: Option<String>,
}

/// The first sheet with data, or every sheet with data when `all_sheets`.
pub fn excel_sources(path: &str, all_sheets: bool) -> Result<Vec<ExcelSource>> {
    let conn = excel_conn()?;
    excel_sources_with(&conn, path, all_sheets)
}

/// The sheet named `sheet`, or the first sheet with data when `sheet` is
/// None. Only that sheet is read.
pub fn excel_sheet_source(path: &str, sheet: Option<&str>) -> Result<ExcelSource> {
    let sheets = xlsx::sheets_with(path, sheet.is_none()).map_err(|e| ConnectorError::new(ErrorKind::Config, e))?;
    let pick = match sheet {
        Some(want) => {
            sheets.iter().find(|s| s.name == want).ok_or_else(|| ConnectorError::new(ErrorKind::Config, format!("the workbook has no sheet {want}")))?
        }
        None => sheets.iter().find(|s| s.range.is_some()).ok_or_else(|| ConnectorError::new(ErrorKind::Config, "every sheet in this workbook is empty"))?,
    };
    if pick.range.is_none() {
        return Err(ConnectorError::new(ErrorKind::Config, format!("sheet {} is empty", pick.name)));
    }
    let conn = excel_conn()?;
    Ok(ExcelSource { sheet: pick.name.clone(), sql: excel_scan(&conn, path, Some(pick)), left_out: pick.left_out() })
}

/// Sheets with data: the first one, or all of them.
pub(crate) fn excel_sources_with(conn: &duckdb::Connection, path: &str, all_sheets: bool) -> Result<Vec<ExcelSource>> {
    let sheets = xlsx::sheets_with(path, !all_sheets).map_err(|e| ConnectorError::new(ErrorKind::Config, e))?;
    let used = sheets.iter().filter(|s| s.range.is_some());
    let picked: Vec<&xlsx::SheetInfo> = if all_sheets { used.collect() } else { used.take(1).collect() };
    let srcs = scan_sheets(conn, path, &picked);
    Ok(picked.into_iter().zip(srcs).map(|(s, sql)| ExcelSource { sheet: s.name.clone(), sql, left_out: s.left_out() }).collect())
}

/// [`excel_scan`] for each sheet, several at a time: each sheet takes a few
/// reads of the file, so a workbook with many sheets is slow one by one.
fn scan_sheets(conn: &duckdb::Connection, path: &str, sheets: &[&xlsx::SheetInfo]) -> Vec<String> {
    let workers = std::thread::available_parallelism().map_or(1, |n| n.get()).min(8).min(sheets.len());
    // Extra connections to the same database (the Excel extension is loaded on it).
    let extra: Vec<duckdb::Connection> = (1..workers).map_while(|_| conn.try_clone().ok()).collect();
    if extra.is_empty() {
        return sheets.iter().map(|s| excel_scan(conn, path, Some(s))).collect();
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let out: std::sync::Mutex<Vec<Option<String>>> = std::sync::Mutex::new(vec![None; sheets.len()]);
    let work = &|c: &duckdb::Connection| {
        loop {
            let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let Some(s) = sheets.get(i) else { break };
            let src = excel_scan(c, path, Some(s));
            if let Ok(mut o) = out.lock() {
                o[i] = Some(src);
            }
        }
    };
    std::thread::scope(|scope| {
        for c in extra {
            scope.spawn(move || work(&c));
        }
        work(conn);
    });
    let out = out.into_inner().unwrap_or_else(|e| e.into_inner());
    // A worker that panicked leaves its sheet unread: read it here.
    out.into_iter().zip(sheets).map(|(src, s)| src.unwrap_or_else(|| excel_scan(conn, path, Some(s)))).collect()
}

/// Read one sheet of an .xlsx file completely, with column types that fit
/// every row.
///
/// DuckDB's `read_xlsx` reads the first sheet, guesses the cell range from
/// the first row, stops at the first empty row, and treats a header row with
/// a blank cell as data. So this:
/// - reads the sheet's used range (every cell with a value) with `header = true`
///   when the first row is all text; blank header cells are named after their
///   column letter (`column_D`), so columns past a blank header are kept;
/// - drops rows that are completely empty;
/// - checks each typed column over the whole sheet (DuckDB types from the first
///   data row only) and reads columns that do not fit as text (`A-12`, `0042`).
///
/// `sheet` = None reads the first sheet with DuckDB's own range detection
/// (used when the workbook cannot be inspected).
pub fn excel_scan(conn: &duckdb::Connection, path: &str, sheet: Option<&xlsx::SheetInfo>) -> String {
    let lit = quote_literal(path);
    let qi = |n: &str| quote_ident(ConnectorKind::Duckdb, n);
    let mut base = lit.clone();
    if let Some(s) = sheet {
        base.push_str(&format!(", sheet = {}", quote_literal(&s.name)));
    }
    let range = sheet.and_then(|s| s.range.as_deref()).and_then(|r| xlsx::parse_range(r).map(|p| (r, p)));
    let describe = |src: &str| -> Option<Vec<(String, String)>> {
        let mut st = conn.prepare(&format!("DESCRIBE SELECT * FROM {src}")).ok()?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).ok()?;
        rows.collect::<std::result::Result<Vec<_>, _>>().ok()
    };
    let layout = sheet.and_then(|s| s.layout.as_ref()).filter(|_| range.is_some());
    // Per column, whether its header cell has a value (None: no header row).
    let mut header_cells: Option<Vec<bool>> = None;
    if let Some((r, ((c0, r0), (c1, r1)))) = range {
        header_cells = match layout {
            Some(l) => l.header.clone(),
            None => {
                // A header row has only text (or blank) cells, and something below it.
                let first = format!("{}{r0}:{}{r0}", xlsx::col_letters(c0), xlsx::col_letters(c1));
                let src = format!("SELECT * FROM read_xlsx({base}, header = false, range = {})", quote_literal(&first));
                first_row_text(conn, &src)
                    .filter(|vals| r1 > r0 && vals.iter().any(|v| matches!(v, Some(Some(_)))) && vals.iter().all(|v| v.is_some()))
                    .map(|vals| vals.into_iter().map(|v| matches!(v, Some(Some(_)))).collect())
            }
        };
        base.push_str(&format!(", header = {}, range = {}, stop_at_empty = false", header_cells.is_some(), quote_literal(r)));
    }
    let plain = format!("read_xlsx({base})");
    let text = format!("read_xlsx({base}, all_varchar = true)");
    let Some(typed) = describe(&plain) else { return plain };
    // With a range, `header` is given, so the text read has the same names.
    if range.is_none() {
        let Some(texts) = describe(&text) else { return plain };
        if typed.iter().map(|c| &c.0).ne(texts.iter().map(|c| &c.0)) {
            return text;
        }
    }
    // Output names: blank header cells (or no header) → column_<letter>.
    let c0 = range.map(|(_, ((c, _), _))| c).unwrap_or(1);
    let out_names: Vec<String> = typed
        .iter()
        .enumerate()
        .map(|(i, (n, _))| match &header_cells {
            Some(h) if h.get(i) == Some(&false) => format!("column_{}", xlsx::col_letters(c0 + i as u32)),
            _ => n.clone(),
        })
        .collect();
    let renamed = out_names.iter().zip(&typed).any(|(o, (n, _))| o != n);
    // Cells (as text) that do not fit the inferred type. Number, date and
    // time cells come through as numbers (dates as Excel serial numbers).
    let misfit = |name: &str, ty: &str| -> Option<String> {
        let v = qi(name);
        let present = format!("{v} IS NOT NULL AND trim({v}) <> ''");
        match ty {
            "VARCHAR" => None,
            "BOOLEAN" => Some(format!("{present} AND lower(trim({v})) NOT IN ('true', 'false', '1', '0')")),
            // Leading zeros (`0042`) mean an identifier stored as text.
            _ => Some(format!("{present} AND (TRY_CAST({v} AS DOUBLE) IS NULL OR regexp_matches(trim({v}), '^[+-]?0[0-9]'))")),
        }
    };
    // What the scan of the sheet already tells, per column: true = read as
    // text, false = the type fits, None = check every cell. A column whose
    // cells are all of the kind its type came from fits; a typed column with
    // no number in it (its first data cell was blank) is text.
    let known: Vec<Option<bool>> = typed
        .iter()
        .enumerate()
        .map(|(i, (_, t))| {
            let k = *layout.filter(|l| l.kinds.len() == typed.len())?.kinds.get(i)?;
            match t.as_str() {
                "VARCHAR" => Some(false),
                _ if k == 0 => Some(false),
                "BOOLEAN" => (k == xlsx::BOOL).then_some(false),
                _ if k == xlsx::NUMBER => Some(false),
                _ if k & xlsx::NUMBER == 0 => Some(true),
                _ => None,
            }
        })
        .collect();
    let checks: Vec<(usize, String)> =
        typed.iter().enumerate().filter(|(i, _)| known[*i].is_none()).filter_map(|(i, (n, t))| misfit(n, t).map(|w| (i, w))).collect();
    let all_null = typed.iter().map(|(n, _)| format!("({0} IS NULL OR trim({0}) = '')", qi(n))).collect::<Vec<_>>().join(" AND ");
    let known_empty = layout.map(|l| l.empty_rows);
    let mut aggs: Vec<String> = checks.iter().map(|(_, w)| format!("count(*) FILTER (WHERE {w})")).collect();
    if known_empty.is_none() {
        aggs.push(format!("count(*) FILTER (WHERE {all_null})"));
    }
    // A full read of the sheet: only when the scan could not tell.
    let counts: Vec<i64> = if aggs.is_empty() {
        Vec::new()
    } else {
        match conn
            .prepare(&format!("SELECT {} FROM {text}", aggs.join(", ")))
            .and_then(|mut st| st.query_row([], |r| (0..aggs.len()).map(|i| r.get::<_, i64>(i)).collect::<std::result::Result<Vec<_>, _>>()))
        {
            Ok(b) => b,
            Err(_) => return plain,
        }
    };
    let empty_rows = known_empty.unwrap_or_else(|| counts.last().copied().unwrap_or(0) > 0);
    let mut to_text: Vec<bool> = known.iter().map(|k| k.unwrap_or(false)).collect();
    for ((i, _), n) in checks.iter().zip(&counts) {
        to_text[*i] = *n > 0;
    }
    if !to_text.contains(&true) && !renamed && !empty_rows {
        return plain;
    }
    let skip_empty = |src: &str| if empty_rows { format!("{src} WHERE NOT ({all_null})") } else { src.to_string() };
    if !to_text.contains(&true) {
        // Types fit: only rename / drop empty rows.
        let cols =
            typed.iter().zip(&out_names).map(|((n, _), o)| if n == o { qi(n) } else { format!("{} AS {}", qi(n), qi(o)) }).collect::<Vec<_>>().join(", ");
        return format!("(SELECT {cols} FROM {})", skip_empty(&plain));
    }
    // Read everything as text once and convert the columns that fit.
    let cols = typed
        .iter()
        .zip(to_text)
        .zip(&out_names)
        .map(|(((n, t), as_text), out)| {
            let v = qi(n);
            let num = format!("TRY_CAST({v} AS DOUBLE)");
            // Excel serial numbers count days from 1899-12-30.
            let expr = match t.as_str() {
                // Mixed date column: show date cells as dates, keep the text.
                "DATE" if as_text => format!("coalesce(strftime(DATE '1899-12-30' + CAST(floor({num}) AS INTEGER), '%Y-%m-%d'), {v})"),
                "TIMESTAMP" if as_text => format!(
                    "coalesce(strftime(TIMESTAMP '1899-12-30' + to_microseconds(CAST(round({num} * 86400000000) AS BIGINT)), '%Y-%m-%d %H:%M:%S'), {v})"
                ),
                _ if as_text => v.clone(),
                "VARCHAR" => v.clone(),
                "BOOLEAN" => format!("CASE lower(trim({v})) WHEN 'true' THEN true WHEN '1' THEN true WHEN 'false' THEN false WHEN '0' THEN false END"),
                "DATE" => format!("(DATE '1899-12-30' + CAST(floor({num}) AS INTEGER))"),
                "TIMESTAMP" => format!("(TIMESTAMP '1899-12-30' + to_microseconds(CAST(round({num} * 86400000000) AS BIGINT)))"),
                "TIME" => format!("CAST(TIMESTAMP '1899-12-30' + to_microseconds(CAST(round(({num} - floor({num})) * 86400000000) AS BIGINT)) AS TIME)"),
                "DOUBLE" => num,
                other => format!("TRY_CAST({v} AS {other})"),
            };
            format!("{expr} AS {}", qi(out))
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("(SELECT {cols} FROM {})", skip_empty(&text))
}

/// Cells of the one-row query `sql`: None for a cell that is not text,
/// Some(None) for a blank one, Some(Some(text)) otherwise.
fn first_row_text(conn: &duckdb::Connection, sql: &str) -> Option<Vec<Option<Option<String>>>> {
    use duckdb::arrow::array::{Array, AsArray};
    use duckdb::arrow::datatypes::DataType;
    let mut st = conn.prepare(sql).ok()?;
    let batch = st.query_arrow([]).ok()?.find(|b| b.num_rows() > 0)?;
    Some(
        batch
            .columns()
            .iter()
            .map(|col| {
                if col.is_null(0) {
                    return Some(None);
                }
                let text = match col.data_type() {
                    DataType::Utf8 => col.as_string::<i32>().value(0).to_string(),
                    DataType::LargeUtf8 => col.as_string::<i64>().value(0).to_string(),
                    DataType::Utf8View => col.as_string_view().value(0).to_string(),
                    _ => return None,
                };
                Some(Some(text).filter(|t| !t.trim().is_empty()))
            })
            .collect(),
    )
}

#[cfg(test)]
mod excel_types {
    use super::*;
    use crate::files::attach_files;
    use rust_xlsxwriter::{ExcelDateTime, Format, Workbook};

    /// Sheet whose first data row suggests numbers/dates for columns that
    /// hold text further down.
    fn mixed_sheet(path: &std::path::Path) {
        let mut wb = Workbook::new();
        let ws = wb.add_worksheet();
        let date = Format::new().set_num_format("yyyy-mm-dd");
        for (c, h) in ["id", "code", "name", "when", "amount", "day"].iter().enumerate() {
            ws.write_string(0, c as u16, *h).unwrap();
        }
        let d = |n: u8| ExcelDateTime::from_ymd(2023, 3, n).unwrap();
        ws.write_number(1, 0, 1).unwrap();
        ws.write_number(1, 1, 100).unwrap();
        ws.write_number(1, 4, 1.5).unwrap();
        ws.write_datetime_with_format(1, 3, d(15), &date).unwrap();
        ws.write_datetime_with_format(1, 5, d(15), &date).unwrap();
        ws.write_number(2, 0, 2).unwrap();
        ws.write_string(2, 1, "A-12").unwrap();
        ws.write_string(2, 2, "Bob").unwrap();
        ws.write_string(2, 3, "n/a").unwrap();
        ws.write_number(2, 4, 2).unwrap();
        ws.write_datetime_with_format(2, 5, d(16), &date).unwrap();
        ws.write_number(3, 0, 3).unwrap();
        ws.write_string(3, 1, "0042").unwrap();
        ws.write_string(3, 2, "Ann").unwrap();
        ws.write_datetime_with_format(3, 3, d(17), &date).unwrap();
        ws.write_number(3, 4, 3.25).unwrap();
        ws.write_datetime_with_format(3, 5, d(17), &date).unwrap();
        wb.save(path).unwrap();
    }

    fn conn() -> Option<duckdb::Connection> {
        let c = duckdb::Connection::open_in_memory().unwrap();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../app/resources/duckdb-extensions");
        if dir.is_dir() {
            c.execute_batch(&format!("SET extension_directory = {}", quote_literal(&dir.to_string_lossy()))).unwrap();
        }
        load_extension(&c, "excel");
        c.execute_batch("SELECT excel_text(1, '0')").ok().map(|_| c)
    }

    #[test]
    fn mixed_columns_are_read_as_text() {
        let Some(c) = conn() else { return eprintln!("skipped: excel extension unavailable") };
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("mixed.xlsx");
        mixed_sheet(&p);
        let path = p.to_string_lossy().to_string();
        // DuckDB's own inference (first row only) fails on this sheet.
        let plain = c.prepare(&format!("SELECT * FROM read_xlsx({})", quote_literal(&path))).and_then(|mut s| s.query_arrow([]).map(|r| r.count()));
        assert!(plain.is_err(), "expected read_xlsx to fail on mixed columns");
        let src = excel_sources_with(&c, &path, false).unwrap().remove(0).sql;
        let types: Vec<(String, String)> =
            c.prepare(&format!("DESCRIBE SELECT * FROM {src}")).unwrap().query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(|r| r.unwrap()).collect();
        let ty = |n: &str| types.iter().find(|t| t.0 == n).map(|t| t.1.as_str()).unwrap();
        assert_eq!(ty("id"), "DOUBLE", "{src}");
        assert_eq!(ty("code"), "VARCHAR");
        assert_eq!(ty("name"), "VARCHAR", "empty first cell, text below");
        assert_eq!(ty("when"), "VARCHAR");
        assert_eq!(ty("amount"), "DOUBLE");
        assert_eq!(ty("day"), "DATE", "consistent dates stay dates");
        let rows: Vec<(String, Option<String>, String, String)> = c
            .prepare(&format!("SELECT code, name, \"when\", CAST(day AS VARCHAR) FROM {src}"))
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(rows[0], ("100".into(), None, "2023-03-15".into(), "2023-03-15".into()));
        assert_eq!(rows[1].0, "A-12");
        assert_eq!(rows[1].2, "n/a");
        assert_eq!(rows[2].0, "0042", "leading zeros kept");
    }

    #[test]
    fn consistent_sheet_uses_plain_read_xlsx() {
        let Some(c) = conn() else { return };
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("ok.xlsx");
        c.execute_batch(&format!(
            "COPY (SELECT range AS n, 'x' || range AS s FROM range(5)) TO {} (FORMAT xlsx, HEADER true)",
            quote_literal(&p.to_string_lossy())
        ))
        .unwrap();
        let src = excel_sources_with(&c, &p.to_string_lossy(), false).unwrap().remove(0).sql;
        assert!(src.starts_with("read_xlsx("), "no wrapper needed: {src}");
        assert!(src.contains("range = 'A1:B6'"), "{src}");
    }

    fn rows(c: &duckdb::Connection, sql: &str) -> Vec<Vec<Option<String>>> {
        let mut st = c.prepare(sql).unwrap();
        let n = st.query([]).map(|_| ()).ok().map(|_| st.column_count()).unwrap_or(0);
        st.query_map([], |r| (0..n).map(|i| r.get::<_, Option<String>>(i)).collect::<std::result::Result<Vec<_>, _>>()).unwrap().map(|r| r.unwrap()).collect()
    }

    fn names(c: &duckdb::Connection, src: &str) -> Vec<String> {
        c.prepare(&format!("DESCRIBE SELECT * FROM {src}")).unwrap().query_map([], |r| r.get::<_, String>(0)).unwrap().map(|r| r.unwrap()).collect()
    }

    /// Header with blank cells, a column wider than the header, an empty row
    /// in the middle, and a second sheet.
    fn ragged_book(path: &std::path::Path) {
        let mut wb = Workbook::new();
        let ws = wb.add_worksheet().set_name("Sales").unwrap();
        ws.write_string(0, 0, "id").unwrap();
        ws.write_string(0, 2, "amount").unwrap();
        for r in 1..4u32 {
            ws.write_number(r, 0, r as f64).unwrap();
            ws.write_string(r, 1, format!("x{r}")).unwrap();
            ws.write_number(r, 2, r as f64 * 1.5).unwrap();
        }
        ws.write_string(2, 3, "wide").unwrap();
        // Row 5 empty, data continues on row 6.
        ws.write_number(5, 0, 9).unwrap();
        ws.write_string(5, 1, "after gap").unwrap();
        let w2 = wb.add_worksheet().set_name("Q2 & plan").unwrap();
        w2.write_string(0, 0, "k").unwrap();
        w2.write_number(1, 0, 7).unwrap();
        wb.add_worksheet().set_name("Empty").unwrap();
        wb.save(path).unwrap();
    }

    #[test]
    fn blank_headers_gaps_and_wide_columns_are_read() {
        let Some(c) = conn() else { return };
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("ragged.xlsx");
        ragged_book(&p);
        let path = p.to_string_lossy().to_string();
        let src = excel_sources_with(&c, &path, false).unwrap().remove(0).sql;
        assert_eq!(names(&c, &src), vec!["id", "column_B", "amount", "column_D"], "{src}");
        let r = rows(&c, &format!("SELECT CAST(id AS VARCHAR), column_B, column_D FROM {src}"));
        assert_eq!(r.len(), 4, "empty row dropped, row after the gap kept: {r:?}");
        assert_eq!(r[1], vec![Some("2.0".into()), Some("x2".into()), Some("wide".into())]);
        assert_eq!(r[3][1].as_deref(), Some("after gap"));
    }

    #[test]
    fn lists_and_reads_every_sheet() {
        let Some(c) = conn() else { return };
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("book.xlsx");
        ragged_book(&p);
        let path = p.to_string_lossy().to_string();
        let sheets = xlsx::sheets(&path).unwrap();
        assert_eq!(
            sheets.iter().map(|s| (s.name.as_str(), s.range.as_deref())).collect::<Vec<_>>(),
            vec![("Sales", Some("A1:D6")), ("Q2 & plan", Some("A1:A2")), ("Empty", None)]
        );
        let all = excel_sources_with(&c, &path, true).unwrap();
        assert_eq!(all.iter().map(|s| s.sheet.as_str()).collect::<Vec<_>>(), vec!["Sales", "Q2 & plan"], "empty sheets skipped");
        assert_eq!(rows(&c, &format!("SELECT CAST(k AS VARCHAR) FROM {}", all[1].sql)), vec![vec![Some("7.0".into())]]);
        // Attached as views: first sheet keeps the file name.
        c.execute_batch("CREATE SCHEMA IF NOT EXISTS files").unwrap();
        attach_files(&c, std::slice::from_ref(&path), true).unwrap();
        let views = rows(&c, "SELECT view_name FROM duckdb_views() WHERE schema_name = 'files' ORDER BY 1");
        assert_eq!(views, vec![vec![Some("book".into())], vec![Some("book_q2___plan".into())]]);
    }

    /// Many sheets are read several at a time; sources keep workbook order.
    #[test]
    fn many_sheets_keep_their_order() {
        let Some(c) = conn() else { return };
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("many.xlsx");
        let mut wb = Workbook::new();
        wb.add_worksheet().set_name("Cover").unwrap();
        for s in 0..20u32 {
            let ws = wb.add_worksheet().set_name(format!("S{s}")).unwrap();
            ws.write_string(0, 0, "n").unwrap();
            ws.write_number(1, 0, s as f64).unwrap();
            if s % 2 == 0 {
                ws.write_string(2, 0, "text below").unwrap(); // read as text
            }
        }
        wb.save(&p).unwrap();
        let path = p.to_string_lossy().to_string();
        let all = excel_sources_with(&c, &path, true).unwrap();
        assert_eq!(all.iter().map(|s| s.sheet.clone()).collect::<Vec<_>>(), (0..20).map(|s| format!("S{s}")).collect::<Vec<_>>(), "empty cover skipped");
        for (s, ExcelSource { sql: src, .. }) in all.iter().enumerate() {
            let first = rows(&c, &format!("SELECT CAST(n AS VARCHAR) FROM {src}"));
            assert_eq!(first[0][0].as_deref().map(|v| v.trim_end_matches(".0")), Some(s.to_string().as_str()), "{src}");
        }
        // The first sheet with data, not the empty cover.
        assert_eq!(excel_sources_with(&c, &path, false).unwrap()[0].sheet, "S0");
    }

    #[test]
    fn one_sheet_by_name() {
        if conn().is_none() {
            return;
        }
        if let Some(dir) = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../app/resources/duckdb-extensions").to_str() {
            crate::set_extension_dir(Some(dir.to_string()));
        }
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("book.xlsx");
        ragged_book(&p);
        let path = p.to_string_lossy().to_string();
        let ExcelSource { sheet: name, sql: src, .. } = excel_sheet_source(&path, Some("Q2 & plan")).unwrap();
        assert_eq!(name, "Q2 & plan");
        assert!(src.contains("sheet = 'Q2 & plan'") && !src.contains("Sales"), "{src}");
        assert_eq!(excel_sheet_source(&path, None).unwrap().sheet, "Sales");
        assert!(excel_sheet_source(&path, Some("Empty")).unwrap_err().to_string().contains("empty"));
        assert!(excel_sheet_source(&path, Some("Nope")).unwrap_err().to_string().contains("no sheet Nope"));
    }

    /// A title above the table and a stray cell far from it are left out of
    /// the read, and the SQL says so.
    #[test]
    fn title_rows_and_stray_cells_are_left_out() {
        let Some(c) = conn() else { return };
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("report.xlsx");
        let mut wb = Workbook::new();
        let ws = wb.add_worksheet().set_name("Report").unwrap();
        ws.merge_range(0, 0, 0, 3, "Quarterly report", &Format::new()).unwrap();
        ws.write_string(1, 0, "Generated 2024-01-01").unwrap();
        for (i, h) in ["id", "name", "amount", "day"].iter().enumerate() {
            ws.write_string(3, i as u16, *h).unwrap();
        }
        for r in 4..30u32 {
            ws.write_number(r, 0, r as f64).unwrap();
            ws.write_string(r, 1, format!("n{r}")).unwrap();
            ws.write_number(r, 2, r as f64 * 2.0).unwrap();
        }
        ws.write_string(5000, 300, "far away").unwrap();
        wb.save(&p).unwrap();
        let path = p.to_string_lossy().to_string();
        let s = &xlsx::sheets(&path).unwrap()[0];
        assert_eq!((s.used.as_deref(), s.range.as_deref()), (Some("A1:KO5001"), Some("A4:D30")));
        assert_eq!(s.left_out().as_deref(), Some("reads A4:D30 of A1:KO5001"));
        let l = s.layout.as_ref().unwrap();
        assert_eq!(l.header, Some(vec![true, true, true, true]));
        assert_eq!(l.kinds, vec![xlsx::NUMBER, xlsx::TEXT, xlsx::NUMBER, 0]);
        assert!(!l.empty_rows);
        let src = excel_sources_with(&c, &path, false).unwrap().remove(0);
        assert_eq!(src.left_out.as_deref(), Some("reads A4:D30 of A1:KO5001"));
        assert_eq!(names(&c, &src.sql), vec!["id", "name", "amount", "day"], "{}", src.sql);
        assert_eq!(rows(&c, &format!("SELECT CAST(count(*) AS VARCHAR) FROM {}", src.sql)), vec![vec![Some("26".into())]]);
    }

    /// The scan tells which columns fit their type, so a clean sheet needs no
    /// read of every row; a column mixing numbers and text is still checked.
    #[test]
    fn the_scan_decides_types_without_a_full_read() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("mixed.xlsx");
        mixed_sheet(&p);
        let s = &xlsx::sheets(&p.to_string_lossy()).unwrap()[0];
        let l = s.layout.as_ref().unwrap();
        // id, code (number then text), name (blank then text), when (date then text), amount, day
        assert_eq!(l.kinds, vec![xlsx::NUMBER, xlsx::NUMBER | xlsx::TEXT, xlsx::TEXT, xlsx::NUMBER | xlsx::TEXT, xlsx::NUMBER, xlsx::NUMBER]);
        // The same sheet without a layout (the scan failed) gives the same SQL.
        let Some(c) = conn() else { return };
        let path = p.to_string_lossy().to_string();
        let with = excel_scan(&c, &path, Some(s));
        let without = excel_scan(&c, &path, Some(&xlsx::SheetInfo { layout: None, ..s.clone() }));
        assert_eq!(with, without);
    }
}
