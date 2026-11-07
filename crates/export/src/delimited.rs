use std::io::Write;

use crate::Result;

/// Text a spreadsheet would run as a formula. A number (`-12.5`, `+3`,
/// `-1e5`) is not: it opens as the number either way.
pub(crate) fn is_formula_like(s: &str) -> bool {
    s.starts_with(['=', '+', '-', '@', '\t', '\r']) && s.trim().parse::<f64>().is_err()
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
}
