//! Column type mapping and value conversion.

use databrain_connector_core::ColType;
use databrain_connector_core::Value;
use databrain_connector_core::value::MICROS_PER_DAY;
use databrain_connector_core::value::days_from_civil;
use oracle::SqlValue;
use oracle::sql_type::OracleType;
use oracle::sql_type::Timestamp;

pub(crate) fn col_type(t: &OracleType) -> ColType {
    match t {
        OracleType::Number(p, s) if *s == 0 && *p > 0 && *p <= 18 => ColType::Int64,
        OracleType::Int64 => ColType::Int64,
        OracleType::UInt64 => ColType::UInt64,
        OracleType::BinaryFloat | OracleType::BinaryDouble | OracleType::Float(_) => ColType::Float64,
        OracleType::Date | OracleType::Timestamp(_) => ColType::Timestamp,
        OracleType::TimestampTZ(_) | OracleType::TimestampLTZ(_) => ColType::TimestampTz,
        OracleType::Raw(_) | OracleType::BLOB | OracleType::LongRaw | OracleType::BFILE => ColType::Binary,
        OracleType::Boolean => ColType::Bool,
        _ => ColType::Utf8,
    }
}

pub(crate) fn db_type(t: &OracleType) -> String {
    match t {
        OracleType::Number(0, -127) => "NUMBER".into(),
        OracleType::Number(p, s) => format!("NUMBER({p},{s})"),
        other => other.to_string(),
    }
}

pub(crate) fn ts_micros(t: &Timestamp) -> i64 {
    let days = days_from_civil(t.year() as i64, t.month(), t.day());
    let secs = t.hour() as i64 * 3600 + t.minute() as i64 * 60 + t.second() as i64;
    days * MICROS_PER_DAY + secs * 1_000_000 + (t.nanosecond() / 1000) as i64
}

pub(crate) fn convert(t: ColType, v: &SqlValue) -> Value {
    if v.is_null().unwrap_or(true) {
        return Value::Null;
    }
    let text = || v.get::<String>().map(Value::Text).unwrap_or_else(|_| Value::Text(v.to_string()));
    match t {
        ColType::Int64 => v.get::<i64>().map(Value::Int).unwrap_or_else(|_| text()),
        ColType::UInt64 => v.get::<u64>().map(Value::UInt).unwrap_or_else(|_| text()),
        ColType::Float64 => v.get::<f64>().map(Value::Float).unwrap_or_else(|_| text()),
        ColType::Bool => v.get::<bool>().map(Value::Bool).unwrap_or_else(|_| text()),
        ColType::Binary => v.get::<Vec<u8>>().map(Value::Bytes).unwrap_or_else(|_| text()),
        ColType::Timestamp => v.get::<Timestamp>().map(|ts| Value::Timestamp(ts_micros(&ts))).unwrap_or_else(|_| text()),
        ColType::TimestampTz => {
            v.get::<Timestamp>().map(|ts| Value::TimestampTz(ts_micros(&ts) - ts.tz_offset() as i64 * 1_000_000)).unwrap_or_else(|_| text())
        }
        _ => text(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_types() {
        assert_eq!(col_type(&OracleType::Number(10, 0)), ColType::Int64);
        assert_eq!(col_type(&OracleType::Number(0, -127)), ColType::Utf8);
        assert_eq!(col_type(&OracleType::Number(10, 2)), ColType::Utf8);
        assert_eq!(db_type(&OracleType::Number(10, 2)), "NUMBER(10,2)");
        assert_eq!(col_type(&OracleType::Date), ColType::Timestamp);
    }
}
