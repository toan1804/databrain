//! Minimal .xlsx inspection: sheet names (workbook order) and each sheet's
//! used cell range. DuckDB's `read_xlsx` reads one sheet and guesses the
//! range from the first row, so blank header cells or empty rows cut data off.

use std::io::Read;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SheetInfo {
    pub name: String,
    /// Used range with values, e.g. `A1:G120` (None for an empty sheet).
    pub range: Option<String>,
    pub hidden: bool,
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
    if letters.is_empty() || !letters.bytes().all(|b| b.is_ascii_alphabetic()) {
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

fn read_entry(z: &mut zip::ZipArchive<std::fs::File>, name: &str) -> Option<String> {
    let mut f = z.by_name(name).ok()?;
    let mut s = String::new();
    f.read_to_string(&mut s).ok()?;
    Some(s)
}

/// Used range of a worksheet: cells that hold a value (`<v>` or inline string).
pub fn used_range(sheet_xml: &str) -> Option<String> {
    let (mut c0, mut r0, mut c1, mut r1) = (u32::MAX, u32::MAX, 0u32, 0u32);
    for (tag, self_closing, end) in tags(sheet_xml, "c") {
        if self_closing {
            continue; // styled but empty
        }
        let body_end = sheet_xml[end..].find("</").map(|i| end + i).unwrap_or(sheet_xml.len());
        let body = &sheet_xml[end..body_end];
        // `<c><f>…</f></c>` without a cached value counts as empty; `<v></v>` too.
        // `body` stops at the first closing tag, so `<v></v>` leaves a bare `<v>`.
        let v_text = body.find("<v>").map(|i| &body[i + 3..]).or_else(|| body.find(":v>").map(|i| &body[i + 3..]));
        let has_value = v_text.is_some_and(|t| !t.is_empty()) || body.contains("<is>");
        if !has_value {
            continue;
        }
        let Some((c, r)) = attr(tag, "r").and_then(parse_ref) else { continue };
        c0 = c0.min(c);
        r0 = r0.min(r);
        c1 = c1.max(c);
        r1 = r1.max(r);
    }
    (c1 > 0).then(|| format!("{}{}:{}{}", col_letters(c0), r0, col_letters(c1), r1))
}

/// Sheets of an .xlsx file in workbook order, with their used ranges.
pub fn sheets(path: &str) -> Result<Vec<SheetInfo>, String> {
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
    let mut out = Vec::new();
    for (tag, _, _) in tags(&wb, "sheet") {
        let Some(name) = attr(tag, "name").map(unescape) else { continue };
        let hidden = matches!(attr(tag, "state"), Some("hidden" | "veryHidden"));
        let rid = attr(tag, "r:id").or_else(|| attr(tag, "id"));
        let range = rid.and_then(target).and_then(|t| read_entry(&mut z, &t)).and_then(|x| used_range(&x));
        out.push(SheetInfo { name, range, hidden });
    }
    if out.is_empty() {
        return Err(format!("{path}: no sheets found"));
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
}
