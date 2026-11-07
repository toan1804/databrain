use databrain_connector_core::{ConnectorError, ConnectorKind, ErrorKind, Result, quote_ident, quote_literal};

use crate::engine::{engine, extension_dir, load_extension};
use crate::error::map_err;
use crate::files::{FileFormat, scan_expr};
use crate::xlsx;

/// Like [`scan_expr`], but Excel files are read with [`excel_scan`] (first
/// sheet). Opens a private in-memory DuckDB for the checks.
pub fn file_scan_expr(path: &str, format: FileFormat) -> String {
    if format != FileFormat::Excel {
        return scan_expr(path, format);
    }
    match excel_sources(path, false) {
        Ok(mut v) if !v.is_empty() => v.remove(0).1,
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

/// (sheet name, SQL source) for the first sheet, or for every non-empty
/// sheet when `all_sheets`.
pub fn excel_sources(path: &str, all_sheets: bool) -> Result<Vec<(String, String)>> {
    let conn = excel_conn()?;
    excel_sources_with(&conn, path, all_sheets)
}

pub(crate) fn excel_sources_with(conn: &duckdb::Connection, path: &str, all_sheets: bool) -> Result<Vec<(String, String)>> {
    let sheets = xlsx::sheets(path).map_err(|e| ConnectorError::new(ErrorKind::Config, e))?;
    let picked: Vec<&xlsx::SheetInfo> = if all_sheets { sheets.iter().filter(|s| s.range.is_some()).collect() } else { sheets.iter().take(1).collect() };
    Ok(picked.into_iter().map(|s| (s.name.clone(), excel_scan(conn, path, Some(s)))).collect())
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
    // Header cells (None = blank), and whether the first row is a header.
    let mut header_cells: Option<Vec<Option<String>>> = None;
    if let Some((r, ((c0, r0), (c1, r1)))) = range {
        let first = format!("{}{r0}:{}{r0}", xlsx::col_letters(c0), xlsx::col_letters(c1));
        let src = format!("read_xlsx({base}, header = false, range = {})", quote_literal(&first));
        let types = describe(&src).unwrap_or_default();
        let vals: Option<Vec<Option<String>>> = conn
            .prepare(&format!("SELECT * FROM read_xlsx({base}, header = false, all_varchar = true, range = {})", quote_literal(&first)))
            .and_then(|mut st| st.query_row([], |row| (0..types.len()).map(|i| row.get::<_, Option<String>>(i)).collect::<std::result::Result<Vec<_>, _>>()))
            .ok();
        if let Some(vals) = vals {
            let vals: Vec<Option<String>> = vals.into_iter().map(|v| v.filter(|x| !x.trim().is_empty())).collect();
            let is_header = r1 > r0 && vals.iter().any(Option::is_some) && vals.iter().zip(&types).all(|(v, (_, t))| v.is_none() || t == "VARCHAR");
            if is_header {
                header_cells = Some(vals);
            }
        }
        base.push_str(&format!(", header = {}, range = {}, stop_at_empty = false", header_cells.is_some(), quote_literal(r)));
    }
    let plain = format!("read_xlsx({base})");
    let text = format!("read_xlsx({base}, all_varchar = true)");
    let Some(typed) = describe(&plain) else { return plain };
    let Some(texts) = describe(&text) else { return plain };
    if typed.iter().map(|c| &c.0).ne(texts.iter().map(|c| &c.0)) {
        return text;
    }
    // Output names: blank header cells (or no header) → column_<letter>.
    let c0 = range.map(|(_, ((c, _), _))| c).unwrap_or(1);
    let out_names: Vec<String> = typed
        .iter()
        .enumerate()
        .map(|(i, (n, _))| match &header_cells {
            Some(h) if h.get(i).is_some_and(|v| v.is_none()) => format!("column_{}", xlsx::col_letters(c0 + i as u32)),
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
    let checks: Vec<(usize, String)> = typed.iter().enumerate().filter_map(|(i, (n, t))| misfit(n, t).map(|w| (i, w))).collect();
    let all_null = typed.iter().map(|(n, _)| format!("({0} IS NULL OR trim({0}) = '')", qi(n))).collect::<Vec<_>>().join(" AND ");
    let mut aggs: Vec<String> = checks.iter().map(|(_, w)| format!("count(*) FILTER (WHERE {w})")).collect();
    aggs.push(format!("count(*) FILTER (WHERE {all_null})"));
    let counts: Vec<i64> = match conn
        .prepare(&format!("SELECT {} FROM {text}", aggs.join(", ")))
        .and_then(|mut st| st.query_row([], |r| (0..aggs.len()).map(|i| r.get::<_, i64>(i)).collect::<std::result::Result<Vec<_>, _>>()))
    {
        Ok(b) => b,
        Err(_) => return plain,
    };
    let empty_rows = counts.last().copied().unwrap_or(0) > 0;
    let mut to_text = vec![false; typed.len()];
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
        let src = excel_sources_with(&c, &path, false).unwrap().remove(0).1;
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
        let src = excel_sources_with(&c, &p.to_string_lossy(), false).unwrap().remove(0).1;
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
        let src = excel_sources_with(&c, &path, false).unwrap().remove(0).1;
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
        assert_eq!(all.iter().map(|s| s.0.as_str()).collect::<Vec<_>>(), vec!["Sales", "Q2 & plan"], "empty sheets skipped");
        assert_eq!(rows(&c, &format!("SELECT CAST(k AS VARCHAR) FROM {}", all[1].1)), vec![vec![Some("7.0".into())]]);
        // Attached as views: first sheet keeps the file name.
        c.execute_batch("CREATE SCHEMA IF NOT EXISTS files").unwrap();
        attach_files(&c, std::slice::from_ref(&path), true).unwrap();
        let views = rows(&c, "SELECT view_name FROM duckdb_views() WHERE schema_name = 'files' ORDER BY 1");
        assert_eq!(views, vec![vec![Some("book".into())], vec![Some("book_q2___plan".into())]]);
    }
}
