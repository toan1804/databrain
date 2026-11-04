//! TabSeparated parsing and ClickHouse type / value conversion.

use databrain_connector_core::ColType;
use databrain_connector_core::Value;
use databrain_connector_core::value::parse_date;
use databrain_connector_core::value::parse_datetime;

/// The type inside `Nullable(…)` / `LowCardinality(…)`.
pub(crate) fn base_type(t: &str) -> &str {
    let mut t = t.trim();
    loop {
        let inner = ["Nullable(", "LowCardinality("].iter().find_map(|w| t.strip_prefix(w).and_then(|r| r.strip_suffix(')')));
        match inner {
            Some(i) => t = i.trim(),
            None => return t,
        }
    }
}

pub(crate) fn col_type(t: &str) -> ColType {
    let b = base_type(t);
    match b.split('(').next().unwrap_or(b) {
        "Bool" => ColType::Bool,
        "Int8" | "Int16" | "Int32" | "Int64" | "UInt8" | "UInt16" | "UInt32" => ColType::Int64,
        "UInt64" => ColType::UInt64,
        "Float32" | "Float64" | "BFloat16" => ColType::Float64,
        "Date" | "Date32" => ColType::Date,
        "DateTime" | "DateTime64" => ColType::Timestamp,
        // Decimals and 128/256-bit integers stay text to keep every digit.
        _ => ColType::Utf8,
    }
}

pub(crate) fn cell(t: ColType, s: Option<String>) -> Value {
    let Some(s) = s else { return Value::Null };
    match t {
        ColType::Bool => match s.as_str() {
            "true" | "1" => Value::Bool(true),
            "false" | "0" => Value::Bool(false),
            _ => Value::Text(s),
        },
        ColType::Int64 => s.parse().map(Value::Int).unwrap_or(Value::Text(s)),
        ColType::UInt64 => s.parse().map(Value::UInt).unwrap_or(Value::Text(s)),
        ColType::Float64 => s.parse().map(Value::Float).unwrap_or(Value::Text(s)),
        ColType::Date => parse_date(&s).map(Value::Date).unwrap_or(Value::Text(s)),
        ColType::Timestamp => parse_datetime(&s).map(Value::Timestamp).unwrap_or(Value::Text(s)),
        _ => Value::Text(s),
    }
}

/// Fields of a TabSeparated line (`\N` = NULL, backslash escapes).
pub(crate) fn split_tsv(line: &[u8]) -> Vec<Option<String>> {
    line.split(|b| *b == b'\t')
        .map(|f| {
            if f == b"\\N" {
                return None;
            }
            let mut out = Vec::with_capacity(f.len());
            let mut it = f.iter();
            while let Some(&b) = it.next() {
                if b != b'\\' {
                    out.push(b);
                    continue;
                }
                match it.next() {
                    Some(b'n') => out.push(b'\n'),
                    Some(b't') => out.push(b'\t'),
                    Some(b'r') => out.push(b'\r'),
                    Some(b'b') => out.push(8),
                    Some(b'f') => out.push(12),
                    Some(b'0') => out.push(0),
                    Some(&c) => out.push(c),
                    None => out.push(b'\\'),
                }
            }
            Some(String::from_utf8_lossy(&out).into_owned())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::column_info;

    #[test]
    fn tsv_and_types() {
        assert_eq!(split_tsv(b"a\\tb\t\\N\tx\\\\y\t\\'q"), vec![Some("a\tb".into()), None, Some("x\\y".into()), Some("'q".into())]);
        assert_eq!(col_type("Nullable(UInt64)"), ColType::UInt64);
        assert_eq!(col_type("LowCardinality(Nullable(String))"), ColType::Utf8);
        assert_eq!(col_type("DateTime64(3, 'UTC')"), ColType::Timestamp);
        assert_eq!(col_type("Decimal(18, 4)"), ColType::Utf8);
        assert_eq!(cell(ColType::UInt64, Some("18446744073709551615".into())), Value::UInt(u64::MAX));
        assert_eq!(cell(ColType::Int64, None), Value::Null);
        let c = column_info(&[
            Some("a".into()),
            Some("Nullable(Int32)".into()),
            Some("MATERIALIZED".into()),
            Some("b + 1".into()),
            Some("".into()),
            Some("0".into()),
        ]);
        assert!(c.nullable);
        assert_eq!(c.default.as_deref(), Some("MATERIALIZED b + 1"));
        assert!(!column_info(&[Some("a".into()), Some("Int32".into())]).nullable);
    }
}
