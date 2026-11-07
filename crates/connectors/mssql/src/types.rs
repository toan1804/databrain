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
        T::Money => (ColType::Float64, "money"),
        T::Money4 => (ColType::Float64, "smallmoney"),
        T::Decimaln => (ColType::Utf8, "decimal"),
        T::Numericn => (ColType::Utf8, "numeric"),
        T::Datetime | T::Datetimen => (ColType::Timestamp, "datetime"),
        T::Datetime4 => (ColType::Timestamp, "smalldatetime"),
        T::Datetime2 => (ColType::Timestamp, "datetime2"),
        T::Daten => (ColType::Date, "date"),
        T::Timen => (ColType::Time, "time"),
        T::DatetimeOffsetn => (ColType::TimestampTz, "datetimeoffset"),
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

pub(crate) fn convert(v: ColumnData<'static>) -> Value {
    use ColumnData as D;
    match v {
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
        D::Numeric(x) => x.map(|n| Value::Text(n.to_string())).unwrap_or(Value::Null),
        D::Xml(x) => x.map(|x| Value::Text(x.into_owned().into_string())).unwrap_or(Value::Null),
        D::DateTime(x) => x
            .map(|d| {
                let micros = (d.seconds_fragments() as i64 * 1_000_000) / 300;
                Value::Timestamp((DAYS_1900 + d.days() as i64) * MICROS_PER_DAY + micros)
            })
            .unwrap_or(Value::Null),
        D::SmallDateTime(x) => {
            x.map(|d| Value::Timestamp((DAYS_1900 + d.days() as i64) * MICROS_PER_DAY + d.seconds_fragments() as i64 * 60_000_000)).unwrap_or(Value::Null)
        }
        D::Time(x) => x.map(|t| Value::Time(time_micros(t))).unwrap_or(Value::Null),
        D::Date(x) => x.map(|d| Value::Date((DAYS_0001 + d.days() as i64) as i32)).unwrap_or(Value::Null),
        D::DateTime2(x) => x.map(|d| Value::Timestamp((DAYS_0001 + d.date().days() as i64) * MICROS_PER_DAY + time_micros(d.time()))).unwrap_or(Value::Null),
        D::DateTimeOffset(x) => x
            .map(|d| {
                // Stored as UTC plus an offset.
                let dt = d.datetime2();
                Value::TimestampTz((DAYS_0001 + dt.date().days() as i64) * MICROS_PER_DAY + time_micros(dt.time()))
            })
            .unwrap_or(Value::Null),
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
}
