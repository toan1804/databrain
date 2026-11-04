use databrain_connector_core::value::{parse_date, parse_datetime, parse_time};
use databrain_connector_core::{ColType, Value};
use tokio_postgres::types::Type;

/// The type as Postgres writes it (`format_type`): `character varying(255)`,
/// `numeric(12,2)`, `integer[]`, user types by name.
pub(crate) fn type_name(t: &Type, typmod: i32) -> String {
    let (base, array) = match t.kind() {
        tokio_postgres::types::Kind::Array(inner) => (inner.clone(), true),
        _ => (t.clone(), false),
    };
    let m = typmod - 4; // VARHDRSZ
    let name = match base.name() {
        "int2" => "smallint".into(),
        "int4" => "integer".into(),
        "int8" => "bigint".into(),
        "float4" => "real".into(),
        "float8" => "double precision".into(),
        "bool" => "boolean".into(),
        "varchar" if m >= 0 => format!("character varying({m})"),
        "varchar" => "character varying".into(),
        "bpchar" if m >= 0 => format!("character({m})"),
        "bpchar" => "character".into(),
        "numeric" if m >= 0 => format!("numeric({},{})", (m >> 16) & 0xffff, m & 0xffff),
        "timestamp" if typmod >= 0 => format!("timestamp({typmod}) without time zone"),
        "timestamp" => "timestamp without time zone".into(),
        "timestamptz" if typmod >= 0 => format!("timestamp({typmod}) with time zone"),
        "timestamptz" => "timestamp with time zone".into(),
        "time" => "time without time zone".into(),
        "timetz" => "time with time zone".into(),
        "bit" | "varbit" if typmod >= 0 => format!("{}({typmod})", if base.name() == "bit" { "bit" } else { "bit varying" }),
        other => other.to_string(),
    };
    if array { format!("{name}[]") } else { name }
}

pub(crate) fn col_type(t: &Type) -> ColType {
    match *t {
        Type::BOOL => ColType::Bool,
        Type::INT2 | Type::INT4 | Type::INT8 | Type::OID => ColType::Int64,
        Type::FLOAT4 | Type::FLOAT8 => ColType::Float64,
        Type::BYTEA => ColType::Binary,
        Type::DATE => ColType::Date,
        Type::TIMESTAMP => ColType::Timestamp,
        Type::TIMESTAMPTZ => ColType::TimestampTz,
        Type::TIME => ColType::Time,
        _ => ColType::Utf8,
    }
}

/// Parse a text-protocol value for the given column type.
pub(crate) fn parse_text(t: ColType, s: &str) -> Value {
    match t {
        ColType::Bool => Value::Bool(s == "t"),
        ColType::Int64 => s.parse().map(Value::Int).unwrap_or_else(|_| Value::Text(s.into())),
        ColType::Float64 => s.parse().map(Value::Float).unwrap_or_else(|_| Value::Text(s.into())),
        ColType::Binary => decode_bytea(s).map(Value::Bytes).unwrap_or_else(|| Value::Text(s.into())),
        ColType::Date => parse_date(s).filter(|_| !s.ends_with("BC")).map(Value::Date).unwrap_or_else(|| Value::Text(s.into())),
        ColType::Timestamp => parse_datetime(s).filter(|_| !s.ends_with("BC")).map(Value::Timestamp).unwrap_or_else(|| Value::Text(s.into())),
        ColType::TimestampTz => parse_timestamptz(s).map(Value::TimestampTz).unwrap_or_else(|| Value::Text(s.into())),
        ColType::Time => parse_time(s).map(Value::Time).unwrap_or_else(|| Value::Text(s.into())),
        _ => Value::Text(s.to_string()),
    }
}

pub(crate) fn decode_bytea(s: &str) -> Option<Vec<u8>> {
    let hex = s.strip_prefix("\\x")?;
    if hex.len() % 2 != 0 {
        return None;
    }
    (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok()).collect()
}

/// Parse `YYYY-MM-DD HH:MM:SS[.f]+HH[:MM[:SS]]` into UTC microseconds.
pub(crate) fn parse_timestamptz(s: &str) -> Option<i64> {
    if s.ends_with("BC") {
        return None;
    }
    let idx = s.char_indices().skip(11).find(|(_, c)| *c == '+' || *c == '-').map(|(i, _)| i)?;
    let (dt, off) = s.split_at(idx);
    let local = parse_datetime(dt)?;
    let sign = if off.starts_with('-') { -1 } else { 1 };
    let mut parts = off[1..].split(':');
    let h: i64 = parts.next()?.parse().ok()?;
    let m: i64 = parts.next().unwrap_or("0").parse().ok()?;
    let sec: i64 = parts.next().unwrap_or("0").parse().ok()?;
    let offset_us = sign * ((h * 60 + m) * 60 + sec) * 1_000_000;
    Some(local - offset_us)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_timestamptz_offsets() {
        let base = parse_datetime("2024-03-05 06:04:05").unwrap();
        assert_eq!(parse_timestamptz("2024-03-05 13:04:05+07"), Some(base));
        assert_eq!(parse_timestamptz("2024-03-05 00:34:05-05:30"), Some(base));
        assert_eq!(parse_timestamptz("infinity"), None);
    }
    #[test]
    fn parses_text_values() {
        assert_eq!(parse_text(ColType::Bool, "t"), Value::Bool(true));
        assert_eq!(parse_text(ColType::Int64, "42"), Value::Int(42));
        assert_eq!(parse_text(ColType::Binary, "\\x00ff"), Value::Bytes(vec![0, 255]));
        assert_eq!(parse_text(ColType::Date, "infinity"), Value::Text("infinity".into()));
    }

    #[test]
    fn names_types_like_postgres() {
        assert_eq!(type_name(&Type::INT4, -1), "integer");
        assert_eq!(type_name(&Type::VARCHAR, 259), "character varying(255)");
        assert_eq!(type_name(&Type::NUMERIC, ((12 << 16) | 2) + 4), "numeric(12,2)");
        assert_eq!(type_name(&Type::NUMERIC, -1), "numeric");
        assert_eq!(type_name(&Type::INT4_ARRAY, -1), "integer[]");
        assert_eq!(type_name(&Type::TIMESTAMPTZ, 3), "timestamp(3) with time zone");
        assert_eq!(type_name(&Type::TEXT, -1), "text");
    }
}
