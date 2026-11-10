use std::io::Write;

use crate::Result;

/// Text a spreadsheet would run as a formula: starting with `=`, `@`, a tab
/// or a CR, or with `+`/`-` unless it is a plain number (`-12.5`, `+3`,
/// `-1e5` open as the number either way). Words Rust reads as floats
/// (`-inf`, `+NaN`) are no numbers to a spreadsheet: they count as formulas.
pub(crate) fn is_formula_like(s: &str) -> bool {
    match s.as_bytes().first() {
        Some(b'=' | b'@' | b'\t' | b'\r') => true,
        Some(b'+' | b'-') => !is_plain_number(&s[1..]),
        _ => false,
    }
}

/// Digits with an optional decimal point and exponent (`12`, `.5`, `5.`, `1e-5`), no sign.
fn is_plain_number(s: &str) -> bool {
    let (mantissa, exp) = match s.find(['e', 'E']) {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    };
    let digits = |t: &str| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit());
    let mantissa_ok = match mantissa.split_once('.') {
        Some((a, b)) => (digits(a) || a.is_empty()) && (digits(b) || b.is_empty()) && !(a.is_empty() && b.is_empty()),
        None => digits(mantissa),
    };
    mantissa_ok && exp.is_none_or(|e| digits(e.strip_prefix(['+', '-']).unwrap_or(e)))
}

pub(crate) fn write_delimited<'a, W: Write>(out: &mut W, cells: impl Iterator<Item = Option<&'a str>>, delim: u8) -> Result<()> {
    let mut first = true;
    for c in cells {
        if !first {
            out.write_all(&[delim])?;
        }
        first = false;
        let Some(s) = c else { continue };
        if delim == b'\t' {
            // TSV: no quoting; replace control characters that break rows.
            let clean: String = s.chars().map(|ch| if matches!(ch, '\t' | '\n' | '\r') { ' ' } else { ch }).collect();
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

#[cfg(test)]
mod tests {
    use super::is_formula_like;
    use crate::test_util::*;
    use crate::*;

    #[test]
    fn csv() {
        assert_eq!(run(ExportFormat::Csv), "id,name,ok,raw\n1,\"a,\"\"b\"\"\",true,00ff\n,it's|x,,\n");
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

    /// Which text opens as a formula: = + - @ and a leading tab or CR, unless
    /// the text is a plain number. Words a parser reads as a float (`-inf`,
    /// `+NaN`) are not numbers to a spreadsheet, so they are escaped.
    #[test]
    fn formula_like_text() {
        for s in ["=1+2", "@sum(A1)", "+x", "-x", "-inf", "+inf", "-Infinity", "+NaN", "-nan", "\t1", "\r=1", "-1+2", "- 1", "-1,000", "-0x10", "=", "-", "+"] {
            assert!(is_formula_like(s), "{s:?}");
        }
        for s in ["-1.5", "+3", "-1e5", "-1E+05", "-.5", "-5.", "12", "plain", "NaN", "inf", "", "1-2", "a=b"] {
            assert!(!is_formula_like(s), "{s:?}");
        }
    }

    /// Excel safe escapes the header row too, and number columns' -inf.
    #[test]
    fn excel_safe_header_and_non_finite_numbers() {
        use databrain_connector_core::arrow::array::Float64Array;
        let schema: SchemaRef = Arc::new(Schema::new(vec![Field::new("=cmd()", DataType::Utf8, true), Field::new("-x", DataType::Float64, true)]));
        let b = RecordBatch::try_new(
            schema.clone(),
            vec![Arc::new(StringArray::from(vec!["a", "b", "c"])), Arc::new(Float64Array::from(vec![f64::NEG_INFINITY, -1.5, f64::NAN]))],
        )
        .unwrap();
        for (format, d) in [(ExportFormat::Csv, ','), (ExportFormat::Tsv, '\t')] {
            let mut o = ExportOptions::new(format);
            o.excel_safe = true;
            assert_eq!(export_to_string(schema.clone(), [&b], o).unwrap(), format!("'=cmd(){d}'-x\na{d}'-inf\nb{d}-1.5\nc{d}NaN\n"), "{format:?}");
            let plain = export_to_string(schema.clone(), [&b], ExportOptions::new(format)).unwrap();
            assert!(plain.starts_with(&format!("=cmd(){d}-x\n")), "off: the header stays as it is");
        }
    }
}
