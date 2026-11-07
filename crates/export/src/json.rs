use crate::kind::{ArrayElem, Kind};

pub(crate) fn json_value(cell: Option<&str>, kind: Kind) -> serde_json::Value {
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
pub(crate) fn pg_array_json(s: &str, elem: ArrayElem) -> Option<serde_json::Value> {
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

#[cfg(test)]
mod tests {
    use crate::test_util::*;
    use crate::*;

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
    fn postgres_arrays_become_json_arrays() {
        use databrain_connector_core::value::META_DB_TYPE;
        let f =
            |n: &str, t: &str| Field::new(n, DataType::Utf8, true).with_metadata(std::collections::HashMap::from([(META_DB_TYPE.to_string(), t.to_string())]));
        let schema: SchemaRef =
            Arc::new(Schema::new(vec![f("ints", "integer[]"), f("tags", "text[]"), f("grid", "numeric(4,1)[]"), f("flags", "boolean[]"), f("odd", "text[]")]));
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
}
