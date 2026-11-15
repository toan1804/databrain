//! Column type mapping and value conversion.

use databrain_connector_core::ColType;
use databrain_connector_core::Value;
use databrain_connector_core::value::MICROS_PER_DAY;
use tiberius::ColumnData;
use tiberius::ColumnType;

pub(crate) fn col_type(t: ColumnType) -> (ColType, &'static str) {
    use ColumnType as T;
    match t {
        T::Bit | T::Bitn => (ColType::Bool, "bit"),
        T::Int1 => (ColType::Int64, "tinyint"),
        T::Int2 => (ColType::Int64, "smallint"),
        T::Int4 => (ColType::Int64, "int"),
        T::Int8 => (ColType::Int64, "bigint"),
        T::Intn => (ColType::Int64, "int"),
        T::Float4 => (ColType::Float64, "real"),
        T::Float8 | T::Floatn => (ColType::Float64, "float"),
        // Exact decimals, as text like decimal (see `money_text`).
        T::Money => (ColType::Utf8, "money"),
        T::Money4 => (ColType::Utf8, "smallmoney"),
        T::Decimaln => (ColType::Utf8, "decimal"),
        T::Numericn => (ColType::Utf8, "numeric"),
        T::Datetime | T::Datetimen => (ColType::Timestamp, "datetime"),
        T::Datetime4 => (ColType::Timestamp, "smalldatetime"),
        T::Datetime2 => (ColType::Timestamp, "datetime2"),
        T::Daten => (ColType::Date, "date"),
        T::Timen => (ColType::Time, "time"),
        // Its offset is part of the value: text as SQL Server shows it.
        T::DatetimeOffsetn => (ColType::Utf8, "datetimeoffset"),
        T::BigVarBin => (ColType::Binary, "varbinary"),
        T::BigBinary => (ColType::Binary, "binary"),
        T::Image => (ColType::Binary, "image"),
        T::Guid => (ColType::Utf8, "uniqueidentifier"),
        T::Xml => (ColType::Utf8, "xml"),
        T::NVarchar => (ColType::Utf8, "nvarchar"),
        T::NChar => (ColType::Utf8, "nchar"),
        T::NText => (ColType::Utf8, "ntext"),
        T::BigChar => (ColType::Utf8, "char"),
        T::Text => (ColType::Utf8, "text"),
        _ => (ColType::Utf8, "varchar"),
    }
}

pub(crate) const DAYS_1900: i64 = -25_567; // 1900-01-01
pub(crate) const DAYS_0001: i64 = -719_162; // 0001-01-01

pub(crate) fn time_micros(t: tiberius::time::Time) -> i64 {
    let inc = t.increments() as i128;
    let scale = t.scale() as u32;
    (inc * 1_000_000 / 10i128.pow(scale)) as i64
}

/// A decimal as SQL Server shows it: `12345` at scale 0 (the driver's
/// text gives `12345.0`), else every digit of its scale (`-0.50`).
pub(crate) fn numeric_text(n: tiberius::numeric::Numeric) -> String {
    let v = n.value();
    let scale = n.scale() as u32;
    let sign = if v < 0 { "-" } else { "" };
    let abs = v.unsigned_abs();
    if scale == 0 {
        return format!("{sign}{abs}");
    }
    let p = 10u128.pow(scale);
    format!("{sign}{}.{:0w$}", abs / p, abs % p, w = scale as usize)
}

/// `money` / `smallmoney` with its four decimals. The driver hands it over
/// as a float (`value / 10^4`); the stored integer is recovered by rounding,
/// exact up to about ±900 billion (2^53 ten-thousandths).
pub(crate) fn money_text(f: f64) -> String {
    let raw = (f * 10_000.0).round() as i128;
    let sign = if raw < 0 { "-" } else { "" };
    let abs = raw.unsigned_abs();
    format!("{sign}{}.{:04}", abs / 10_000, abs % 10_000)
}

/// A `datetime`'s time of day in microseconds: SQL Server counts 1/300 s
/// and shows it rounded to the millisecond (`.997`, not `.996666`).
pub(crate) fn datetime_micros(fragments: u32) -> i64 {
    ((fragments as i64 * 1_000 + 150) / 300) * 1_000
}

/// A `datetimeoffset` as SQL Server shows it: the local time with its
/// offset (`2024-05-01 10:30:00.1234567 +07:00`).
pub(crate) fn datetimeoffset_text(d: tiberius::time::DateTimeOffset) -> String {
    let dt = d.datetime2();
    let offset = d.offset() as i64;
    let t = dt.time();
    let scale = t.scale() as u32;
    // Increments of 10^-scale s since midnight UTC; shifted to local time.
    let per_day = 86_400 * 10i128.pow(scale);
    let utc = (DAYS_0001 + dt.date().days() as i64) as i128 * per_day + t.increments() as i128;
    let local = utc + offset as i128 * 60 * 10i128.pow(scale);
    let (day, inc) = (local.div_euclid(per_day), local.rem_euclid(per_day));
    let secs = inc / 10i128.pow(scale);
    let frac = inc % 10i128.pow(scale);
    let date = databrain_connector_core::value::format_date(day as i64);
    let mut out = format!("{date} {:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60);
    if scale > 0 {
        out.push_str(&format!(".{:0w$}", frac, w = scale as usize));
    }
    let (sign, o) = if offset < 0 { ('-', -offset) } else { ('+', offset) };
    out.push_str(&format!(" {sign}{:02}:{:02}", o / 60, o % 60));
    out
}

#[cfg(test)]
pub(crate) fn convert(v: ColumnData<'static>) -> Value {
    convert_typed(v, false)
}

/// [`convert`] knowing the column's type: a `money` column's float is its
/// exact text.
pub(crate) fn convert_typed(v: ColumnData<'static>, money: bool) -> Value {
    use ColumnData as D;
    match v {
        D::F64(Some(f)) if money => Value::Text(money_text(f)),
        D::U8(x) => x.map(|v| Value::Int(v as i64)).unwrap_or(Value::Null),
        D::I16(x) => x.map(|v| Value::Int(v as i64)).unwrap_or(Value::Null),
        D::I32(x) => x.map(|v| Value::Int(v as i64)).unwrap_or(Value::Null),
        D::I64(x) => x.map(Value::Int).unwrap_or(Value::Null),
        D::F32(x) => x.map(|v| Value::Float(v as f64)).unwrap_or(Value::Null),
        D::F64(x) => x.map(Value::Float).unwrap_or(Value::Null),
        D::Bit(x) => x.map(Value::Bool).unwrap_or(Value::Null),
        D::String(x) => x.map(|s| Value::Text(s.into_owned())).unwrap_or(Value::Null),
        D::Guid(x) => x.map(|g| Value::Text(g.to_string().to_uppercase())).unwrap_or(Value::Null),
        D::Binary(x) => x.map(|b| Value::Bytes(b.into_owned())).unwrap_or(Value::Null),
        D::Numeric(x) => x.map(|n| Value::Text(numeric_text(n))).unwrap_or(Value::Null),
        D::Xml(x) => x.map(|x| Value::Text(x.into_owned().into_string())).unwrap_or(Value::Null),
        D::DateTime(x) => {
            x.map(|d| Value::Timestamp((DAYS_1900 + d.days() as i64) * MICROS_PER_DAY + datetime_micros(d.seconds_fragments()))).unwrap_or(Value::Null)
        }
        D::SmallDateTime(x) => {
            x.map(|d| Value::Timestamp((DAYS_1900 + d.days() as i64) * MICROS_PER_DAY + d.seconds_fragments() as i64 * 60_000_000)).unwrap_or(Value::Null)
        }
        D::Time(x) => x.map(|t| Value::Time(time_micros(t))).unwrap_or(Value::Null),
        D::Date(x) => x.map(|d| Value::Date((DAYS_0001 + d.days() as i64) as i32)).unwrap_or(Value::Null),
        D::DateTime2(x) => x.map(|d| Value::Timestamp((DAYS_0001 + d.date().days() as i64) * MICROS_PER_DAY + time_micros(d.time()))).unwrap_or(Value::Null),
        D::DateTimeOffset(x) => x.map(|d| Value::Text(datetimeoffset_text(d))).unwrap_or(Value::Null),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use databrain_connector_core::value::days_from_civil;

    #[test]
    fn converts_dates() {
        use databrain_connector_core::value::{format_date, format_timestamp};
        let d = convert(ColumnData::DateTime(Some(tiberius::time::DateTime::new(45_000, 300 * 3661))));
        let Value::Timestamp(t) = d else { panic!() };
        assert_eq!(format_timestamp(t), "2023-03-17 01:01:01");
        let Value::Date(d) = convert(ColumnData::Date(Some(tiberius::time::Date::new(738_000)))) else { panic!() };
        assert_eq!(format_date(d as i64), "2021-07-30");
        assert_eq!(days_from_civil(1900, 1, 1), DAYS_1900);
        assert_eq!(days_from_civil(1, 1, 1), DAYS_0001);
    }

    #[test]
    fn decimals_show_as_sql_server_shows_them() {
        use tiberius::numeric::Numeric;
        assert_eq!(numeric_text(Numeric::new_with_scale(12345, 0)), "12345");
        assert_eq!(numeric_text(Numeric::new_with_scale(-12345, 0)), "-12345");
        assert_eq!(numeric_text(Numeric::new_with_scale(1250, 2)), "12.50");
        assert_eq!(numeric_text(Numeric::new_with_scale(-5, 2)), "-0.05");
        assert_eq!(numeric_text(Numeric::new_with_scale(i128::MAX / 1000, 38)).len(), 38 + 2);
    }

    #[test]
    fn money_is_exact_text() {
        // 922,337,203.6854 as the driver computes it (raw / 1e4).
        assert_eq!(money_text(9_223_372_036_854.0 / 1e4), "922337203.6854");
        assert_eq!(money_text(0.1), "0.1000");
        assert_eq!(money_text(-12.3456), "-12.3456");
        assert_eq!(money_text(-0.0001), "-0.0001");
        let Value::Text(t) = convert_typed(ColumnData::F64(Some(1.5)), true) else { panic!() };
        assert_eq!(t, "1.5000");
        assert!(matches!(convert_typed(ColumnData::F64(Some(1.5)), false), Value::Float(_)), "a float column stays a float");
        assert_eq!(col_type(ColumnType::Money).0, ColType::Utf8);
    }

    #[test]
    fn datetime_keeps_its_milliseconds() {
        use databrain_connector_core::value::format_timestamp;
        // 23:59:59.997 is 299 three-hundredths after 23:59:59.
        let frag = 300 * 86_399 + 299;
        let Value::Timestamp(t) = convert(ColumnData::DateTime(Some(tiberius::time::DateTime::new(45_000, frag)))) else { panic!() };
        assert!(format_timestamp(t).ends_with("23:59:59.997"), "{}", format_timestamp(t));
        assert_eq!(datetime_micros(1), 3_000);
        assert_eq!(datetime_micros(2), 7_000);
    }

    #[test]
    fn datetimeoffset_keeps_its_offset() {
        use tiberius::time::{Date, DateTime2, DateTimeOffset, Time};
        // 2024-05-01 03:30:00 UTC at +07:00, 7 digits of fraction.
        let days = (days_from_civil(2024, 5, 1) - DAYS_0001) as u32;
        let utc = DateTime2::new(Date::new(days), Time::new((3 * 3600 + 30 * 60) * 10_000_000 + 1_234_567, 7));
        assert_eq!(datetimeoffset_text(DateTimeOffset::new(utc, 7 * 60)), "2024-05-01 10:30:00.1234567 +07:00");
        // Before midnight UTC the local day is the next one; negative offsets.
        let late = DateTime2::new(Date::new(days), Time::new(23 * 3600, 0));
        assert_eq!(datetimeoffset_text(DateTimeOffset::new(late, 90)), "2024-05-02 00:30:00 +01:30");
        assert_eq!(datetimeoffset_text(DateTimeOffset::new(utc, -5 * 60)), "2024-04-30 22:30:00.1234567 -05:00");
        assert_eq!(col_type(ColumnType::DatetimeOffsetn).0, ColType::Utf8);
    }
}
