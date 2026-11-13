//! Minimal .xlsx inspection: sheet names (workbook order), each sheet's used
//! cell range, and where its table is. DuckDB's `read_xlsx` reads one sheet
//! and guesses the range from the first row, so blank header cells or empty
//! rows cut data off.
//!
//! Sheets are read as a stream (a sheet with a million rows is hundreds of MB
//! of XML) and the result is kept per file, so listing the sheets and then
//! reading them looks at the file once.

use std::io::Read;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SheetInfo {
    pub name: String,
    /// The table to read, e.g. `A3:G120` (None for an empty sheet): the used
    /// range less title rows above the table and stray cells far from it.
    pub range: Option<String>,
    /// Every cell with a value, e.g. `A1:G120`; wider than `range` when
    /// cells were left out.
    pub used: Option<String>,
    pub hidden: bool,
    /// What the cells of `range` hold (None when the sheet was not read).
    pub layout: Option<Layout>,
}

impl SheetInfo {
    /// Cells with values that `range` leaves out, as "reads A3:F30 of A1:KO5001".
    pub fn left_out(&self) -> Option<String> {
        match (&self.range, &self.used) {
            (Some(r), Some(u)) if r != u => Some(format!("reads {r} of {u}")),
            _ => None,
        }
    }
}

/// Cell kinds, as bits of [`Layout::kinds`].
pub const NUMBER: u8 = 1;
pub const TEXT: u8 = 2;
pub const BOOL: u8 = 4;
pub const ERROR: u8 = 8;
/// An ISO date stored as text (`t="d"`).
pub const DATE: u8 = 16;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Layout {
    /// The first row is text only, with rows below: per column, whether its
    /// header cell has a value (a blank one is named after its column).
    pub header: Option<Vec<bool>>,
    /// Per column, the kinds of the cells below the header (bits; 0 = all blank).
    pub kinds: Vec<u8>,
    /// Some row inside the range has no value.
    pub empty_rows: bool,
}

/// Column letters for a 1-based index: 1 → A, 27 → AA.
pub fn col_letters(mut n: u32) -> String {
    let mut s = Vec::new();
    while n > 0 {
        let r = ((n - 1) % 26) as u8;
        s.push(b'A' + r);
        n = (n - 1) / 26;
    }
    s.reverse();
    String::from_utf8(s).unwrap_or_default()
}

/// Range `A1:G9` → ((1, 1), (7, 9)).
pub fn parse_range(r: &str) -> Option<((u32, u32), (u32, u32))> {
    let (a, b) = r.split_once(':')?;
    Some((parse_ref(a)?, parse_ref(b)?))
}

/// `B12` → (col 2, row 12).
pub fn parse_ref(r: &str) -> Option<(u32, u32)> {
    let split = r.find(|c: char| c.is_ascii_digit())?;
    let (letters, digits) = r.split_at(split);
    if letters.is_empty() || letters.len() > 3 || !letters.bytes().all(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    let col = letters.bytes().fold(0u32, |a, b| a * 26 + (b.to_ascii_uppercase() - b'A' + 1) as u32);
    Some((col, digits.parse().ok()?))
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

/// Value of attribute `name` in a start tag's text.
fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let mut rest = tag;
    while let Some(i) = rest.find(name) {
        let before = rest[..i].chars().last();
        let after = &rest[i + name.len()..];
        let after_t = after.trim_start();
        if matches!(before, Some(c) if c.is_whitespace()) && after_t.starts_with('=') {
            let v = after_t[1..].trim_start();
            let q = v.chars().next()?;
            if q == '"' || q == '\'' {
                let end = v[1..].find(q)?;
                return Some(&v[1..1 + end]);
            }
        }
        rest = &rest[i + name.len()..];
    }
    None
}

/// Start tags `<prefix:local …>` with the given local name, as (tag text, self-closing, end offset).
fn tags<'a>(xml: &'a str, local: &'a str) -> impl Iterator<Item = (&'a str, bool, usize)> + 'a {
    let mut pos = 0;
    std::iter::from_fn(move || {
        while let Some(i) = xml[pos..].find('<') {
            let start = pos + i;
            let end = start + xml[start..].find('>')?;
            pos = end + 1;
            let tag = &xml[start + 1..end];
            let name = tag.split(|c: char| c.is_whitespace() || c == '/').next().unwrap_or("");
            let lname = name.rsplit(':').next().unwrap_or(name);
            if lname == local && !name.starts_with('/') {
                return Some((tag, tag.ends_with('/'), end + 1));
            }
        }
        None
    })
}

fn read_entry<R: Read + std::io::Seek>(z: &mut zip::ZipArchive<R>, name: &str) -> Option<String> {
    let mut f = z.by_name(name).ok()?;
    let mut s = String::new();
    f.read_to_string(&mut s).ok()?;
    Some(s)
}

/// Largest row and column Excel allows.
const MAX_ROW: u32 = 1 << 20;
const MAX_COL: u32 = 1 << 14;
/// Rows kept cell by cell, to find the header below title rows.
const HEAD_ROWS: usize = 64;
/// Empty rows (or columns) that set stray cells apart from the table.
const GAP: usize = 3;

/// What one pass over a sheet's cells collects.
#[derive(Default)]
struct Scan {
    row: u32,
    col: u32,
    /// Cells with a value, per row and per column (index = number).
    rows: Vec<u32>,
    cols: Vec<u32>,
    /// Kinds per column of the cells not in `head`.
    kinds: Vec<u8>,
    /// The first rows with values, cell by cell: (row, [(col, kind)]).
    head: Vec<(u32, Vec<(u32, u8)>)>,
}

fn bump(v: &mut Vec<u32>, i: u32) {
    let i = i as usize;
    if v.len() <= i {
        v.resize(i + 1, 0);
    }
    v[i] += 1;
}

impl Scan {
    /// Cells of `xml`, a piece of the sheet that ends between rows.
    fn feed(&mut self, xml: &str) {
        let mut pos = 0;
        while let Some(i) = xml[pos..].find('<') {
            let start = pos + i;
            let Some(len) = xml[start..].find('>') else { return };
            let end = start + len;
            pos = end + 1;
            let tag = &xml[start + 1..end];
            let name = tag.split(|c: char| c.is_whitespace() || c == '/').next().unwrap_or("");
            match name.rsplit(':').next().unwrap_or(name) {
                "row" if !name.starts_with('/') => {
                    self.row = attr(tag, "r").and_then(|r| r.parse().ok()).unwrap_or(self.row + 1);
                    self.col = 0;
                }
                "c" if !name.starts_with('/') => {
                    let (col, row) = attr(tag, "r").and_then(parse_ref).unwrap_or((self.col + 1, self.row));
                    self.col = col;
                    if tag.ends_with('/') {
                        continue; // styled but empty
                    }
                    let body_end = xml[pos..].find("</").map_or(xml.len(), |i| pos + i);
                    let body = &xml[pos..body_end];
                    pos = body_end;
                    // `<c><f>…</f></c>` without a cached value counts as empty; `<v></v>` too.
                    // `body` stops at the first closing tag, so `<v></v>` leaves a bare `<v>`.
                    let v_text = body.find("<v>").map(|i| &body[i + 3..]).or_else(|| body.find(":v>").map(|i| &body[i + 3..]));
                    let has_value = v_text.is_some_and(|t| !t.is_empty()) || body.contains("<is>") || body.contains(":is>");
                    if has_value && (1..=MAX_ROW).contains(&row) && (1..=MAX_COL).contains(&col) {
                        let kind = match attr(tag, "t") {
                            None | Some("n") => NUMBER,
                            Some("b") => BOOL,
                            Some("e") => ERROR,
                            Some("d") => DATE,
                            _ => TEXT,
                        };
                        self.add(row, col, kind);
                    }
                }
                _ => {}
            }
        }
    }

    fn add(&mut self, row: u32, col: u32, kind: u8) {
        bump(&mut self.rows, row);
        bump(&mut self.cols, col);
        if let Some(h) = self.head.iter_mut().rev().take(2).find(|h| h.0 == row) {
            h.1.push((col, kind));
        } else if self.head.len() < HEAD_ROWS {
            self.head.push((row, vec![(col, kind)]));
        } else {
            let c = col as usize;
            if self.kinds.len() <= c {
                self.kinds.resize(c + 1, 0);
            }
            self.kinds[c] |= kind;
        }
    }

    /// (used range, table range, layout of the table).
    fn finish(mut self) -> (Option<String>, Option<String>, Option<Layout>) {
        let bounds = |v: &[u32]| Some((v.iter().position(|&n| n > 0)?, v.iter().rposition(|&n| n > 0)?));
        let (Some((c0, c1)), Some((r0, r1))) = (bounds(&self.cols), bounds(&self.rows)) else { return (None, None, None) };
        let a1 = |c0: usize, r0: usize, c1: usize, r1: usize| format!("{}{r0}:{}{r1}", col_letters(c0 as u32), col_letters(c1 as u32));
        let used = a1(c0, r0, c1, r1);
        let (lo, hi) = table_span(&self.cols);
        let (top, bottom) = table_span(&self.rows);
        self.head.sort_by_key(|h| h.0);
        let in_cols = |cells: &[(u32, u8)]| cells.iter().filter(|(c, _)| (lo..=hi).contains(&(*c as usize))).count();
        // Title rows: rows at the top with fewer than half the cells of the widest.
        let head: Vec<&(u32, Vec<(u32, u8)>)> = self.head.iter().filter(|h| (top..=bottom).contains(&(h.0 as usize)) && in_cols(&h.1) > 0).collect();
        let widest = head.iter().map(|h| in_cols(&h.1)).max().unwrap_or(0);
        let start = if widest >= 3 { head.iter().find(|h| in_cols(&h.1) * 2 >= widest).map_or(top, |h| h.0 as usize) } else { top };
        let range = a1(lo, start, hi, bottom);
        // Header: the first row is text only, with rows below it.
        let first: Vec<(u32, u8)> = head
            .iter()
            .find(|h| h.0 as usize == start)
            .map(|h| h.1.iter().filter(|(c, _)| (lo..=hi).contains(&(*c as usize))).copied().collect())
            .unwrap_or_default();
        let header = (bottom > start && !first.is_empty() && first.iter().all(|(_, k)| *k == TEXT))
            .then(|| (lo..=hi).map(|c| first.iter().any(|(fc, _)| *fc as usize == c)).collect::<Vec<_>>());
        let below = if header.is_some() { start + 1 } else { start };
        let mut kinds: Vec<u8> = (lo..=hi).map(|c| self.kinds.get(c).copied().unwrap_or(0)).collect();
        for (row, cells) in &self.head {
            if (below..=bottom).contains(&(*row as usize)) {
                for (c, k) in cells {
                    if let Some(slot) = (*c as usize).checked_sub(lo).and_then(|i| kinds.get_mut(i)) {
                        *slot |= k;
                    }
                }
            }
        }
        let empty_rows = (start..=bottom).any(|r| self.rows.get(r).copied().unwrap_or(0) == 0);
        (Some(used), Some(range), Some(Layout { header, kinds, empty_rows }))
    }
}

/// First and last index of the table in per-row (or per-column) cell counts:
/// runs of [`GAP`] or more empty ones split the counts into blocks, and blocks
/// at either end with under 5% of the cells of the largest block are left out.
fn table_span(counts: &[u32]) -> (usize, usize) {
    let mut blocks: Vec<(usize, usize, u64)> = Vec::new();
    let mut empty = GAP;
    for (i, &n) in counts.iter().enumerate() {
        if n == 0 {
            empty += 1;
            continue;
        }
        match blocks.last_mut() {
            Some(b) if empty < GAP => {
                b.1 = i;
                b.2 += n as u64;
            }
            _ => blocks.push((i, i, n as u64)),
        }
        empty = 0;
    }
    let largest = blocks.iter().map(|b| b.2).max().unwrap_or(0);
    let big = |b: &&(usize, usize, u64)| b.2 * 20 >= largest;
    let first = blocks.iter().find(big).map_or(0, |b| b.0);
    let last = blocks.iter().rev().find(big).map_or(0, |b| b.1);
    (first, last)
}

/// Read a sheet's XML as a stream, a piece (ending between rows) at a time.
fn scan_entry<R: Read + std::io::Seek>(z: &mut zip::ZipArchive<R>, name: &str) -> Option<Scan> {
    let mut f = z.by_name(name).ok()?;
    let mut scan = Scan::default();
    let mut buf: Vec<u8> = Vec::with_capacity(1 << 21);
    let mut chunk = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        let tail = buf.len().saturating_sub(n + 8);
        if let Some(i) =
            buf[tail..].windows(6).rposition(|w| w == b"</row>").map(|i| tail + i + 6).or_else(|| buf.windows(6).rposition(|w| w == b"</row>").map(|i| i + 6))
        {
            scan.feed(&String::from_utf8_lossy(&buf[..i]));
            buf.drain(..i);
        }
    }
    scan.feed(&String::from_utf8_lossy(&buf));
    Some(scan)
}

/// Used range of a worksheet: cells that hold a value (`<v>` or inline string).
pub fn used_range(sheet_xml: &str) -> Option<String> {
    let mut scan = Scan::default();
    scan.feed(sheet_xml);
    scan.finish().0
}

/// Sheets read before, per file (path, size, modified time).
type Key = (String, u64, Option<std::time::SystemTime>);
static CACHE: Mutex<Vec<(Key, Arc<Vec<SheetInfo>>)>> = Mutex::new(Vec::new());

fn key(path: &str) -> Option<Key> {
    let m = std::fs::metadata(path).ok()?;
    Some((path.to_string(), m.len(), m.modified().ok()))
}

/// Sheets of an .xlsx file in workbook order, with their used ranges.
pub fn sheets(path: &str) -> Result<Vec<SheetInfo>, String> {
    sheets_with(path, false)
}

/// Like [`sheets`]; with `first_used`, only up to the first sheet with data
/// (the sheets after it are left out).
pub fn sheets_with(path: &str, first_used: bool) -> Result<Vec<SheetInfo>, String> {
    let key = key(path);
    if let Some(k) = &key
        && let Some(hit) = CACHE.lock().ok().and_then(|c| c.iter().find(|(ck, _)| ck == k).map(|(_, v)| v.clone()))
    {
        return Ok(hit.as_ref().clone());
    }
    let f = std::fs::File::open(path).map_err(|e| format!("cannot open {path}: {e}"))?;
    let mut z = zip::ZipArchive::new(f).map_err(|e| format!("{path} is not an .xlsx file: {e}"))?;
    let wb = read_entry(&mut z, "xl/workbook.xml").ok_or_else(|| format!("{path}: missing xl/workbook.xml"))?;
    let rels = read_entry(&mut z, "xl/_rels/workbook.xml.rels").unwrap_or_default();
    let target = |rid: &str| -> Option<String> {
        tags(&rels, "Relationship").find(|(t, _, _)| attr(t, "Id") == Some(rid)).and_then(|(t, _, _)| attr(t, "Target")).map(|t| {
            let t = unescape(t);
            match t.strip_prefix('/') {
                Some(abs) => abs.to_string(),
                None => format!("xl/{t}"),
            }
        })
    };
    // (name, hidden, XML entry)
    let listed: Vec<(String, bool, Option<String>)> = tags(&wb, "sheet")
        .filter_map(|(tag, _, _)| {
            let name = attr(tag, "name").map(unescape)?;
            let hidden = matches!(attr(tag, "state"), Some("hidden" | "veryHidden"));
            let rid = attr(tag, "r:id").or_else(|| attr(tag, "id"));
            Some((name, hidden, rid.and_then(target)))
        })
        .collect();
    if listed.is_empty() {
        return Err(format!("{path}: no sheets found"));
    }
    let info = |(name, hidden, entry): &(String, bool, Option<String>), z: &mut zip::ZipArchive<std::fs::File>| {
        let (used, range, layout) = entry.as_deref().and_then(|e| scan_entry(z, e)).map_or((None, None, None), Scan::finish);
        SheetInfo { name: name.clone(), range, used, hidden: *hidden, layout }
    };
    if first_used {
        let mut out = Vec::new();
        for s in &listed {
            let i = info(s, &mut z);
            let done = i.range.is_some();
            out.push(i);
            if done {
                break;
            }
        }
        return Ok(out);
    }
    // Several sheets at a time, each worker with its own handle on the file.
    let workers = std::thread::available_parallelism().map_or(1, |n| n.get()).min(8).min(listed.len());
    let next = std::sync::atomic::AtomicUsize::new(0);
    let slots: Mutex<Vec<Option<SheetInfo>>> = Mutex::new(vec![None; listed.len()]);
    let work = |mut z: zip::ZipArchive<std::fs::File>| {
        loop {
            let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let Some(s) = listed.get(i) else { break };
            let got = info(s, &mut z);
            if let Ok(mut o) = slots.lock() {
                o[i] = Some(got);
            }
        }
    };
    std::thread::scope(|scope| {
        for _ in 1..workers {
            if let Ok(z) = std::fs::File::open(path).map_err(|e| e.to_string()).and_then(|f| zip::ZipArchive::new(f).map_err(|e| e.to_string())) {
                scope.spawn(|| work(z));
            }
        }
        work(z);
    });
    let out: Vec<SheetInfo> = slots
        .into_inner()
        .unwrap_or_else(|e| e.into_inner())
        .into_iter()
        .zip(&listed)
        .map(|(got, (name, hidden, _))| got.unwrap_or(SheetInfo { name: name.clone(), range: None, used: None, hidden: *hidden, layout: None }))
        .collect();
    if let (Some(k), Ok(mut c)) = (key, CACHE.lock()) {
        c.retain(|(ck, _)| ck.0 != k.0);
        if c.len() >= 8 {
            c.remove(0);
        }
        c.push((k, Arc::new(out.clone())));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs_and_letters() {
        assert_eq!(col_letters(1), "A");
        assert_eq!(col_letters(26), "Z");
        assert_eq!(col_letters(27), "AA");
        assert_eq!(col_letters(703), "AAA");
        assert_eq!(parse_ref("AB12"), Some((28, 12)));
        assert_eq!(parse_ref("12"), None);
    }

    #[test]
    fn used_range_ignores_empty_styled_cells() {
        let xml = r#"<worksheet><sheetData><row r="2"><c r="B2" t="s"><v>0</v></c><c r="Z2" s="3"/></row>
            <row r="9"><c r="D9"><f>A1</f></c><c r="C9" t="inlineStr"><is><t>x</t></is></c></row></sheetData></worksheet>"#;
        assert_eq!(used_range(xml).as_deref(), Some("B2:C9"));
        assert_eq!(used_range("<worksheet/>"), None);
        assert_eq!(used_range(r#"<sheetData><row><c r="A1"><v>1</v></c><c r="E7"><v></v></c></row></sheetData>"#).as_deref(), Some("A1:A1"));
    }

    #[test]
    fn attributes() {
        assert_eq!(attr(r#"sheet name="A &amp; B" sheetId="1" r:id="rId3""#, "name"), Some("A &amp; B"));
        assert_eq!(attr(r#"sheet name="x" r:id="rId3""#, "r:id"), Some("rId3"));
        assert_eq!(attr(r#"sheet sheetId="1""#, "id"), None);
    }

    #[test]
    fn rows_and_cells_without_references() {
        let xml = r#"<sheetData><row><c t="s"><v>0</v></c><c><v>1</v></c></row><row/><row><c/><c t="b"><v>1</v></c></row></sheetData>"#;
        assert_eq!(used_range(xml).as_deref(), Some("A1:B3"));
    }

    #[test]
    fn table_span_drops_small_blocks_at_the_ends() {
        let mut counts = vec![0u32; 20];
        counts[1] = 1; // a title, two empty rows, then the table
        counts[4..10].fill(5);
        counts[15] = 1; // stray, after a gap
        assert_eq!(table_span(&counts), (1, 9), "a gap of 2 joins the title to the table");
        counts[2] = 0;
        counts[1] = 0;
        counts[0] = 1;
        assert_eq!(table_span(&counts), (4, 9));
        assert_eq!(table_span(&[0, 2, 0, 0, 0, 3]), (1, 5), "blocks of similar size stay");
    }

    /// A sheet larger than one piece of the stream reads like a small one.
    #[test]
    fn large_sheets_are_read_in_pieces() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("big.xlsx");
        let mut wb = rust_xlsxwriter::Workbook::new();
        let ws = wb.add_worksheet();
        ws.write_string(0, 0, "n").unwrap();
        ws.write_string(0, 1, "s").unwrap();
        for r in 1..60_000u32 {
            ws.write_number(r, 0, r as f64).unwrap();
            ws.write_string(r, 1, "x").unwrap();
        }
        ws.write_string(60_000, 1, "last").unwrap();
        wb.save(&p).unwrap();
        let path = p.to_string_lossy().to_string();
        let s = &sheets(&path).unwrap()[0];
        assert_eq!(s.range.as_deref(), Some("A1:B60001"));
        let l = s.layout.as_ref().unwrap();
        assert_eq!((l.header.clone(), l.kinds.clone(), l.empty_rows), (Some(vec![true, true]), vec![NUMBER, TEXT], false));
        // Read again: from the cache, the same.
        assert_eq!(&sheets(&path).unwrap()[0], s);
        assert_eq!(sheets_with(&path, true).unwrap().len(), 1);
    }
}
