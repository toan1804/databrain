//! An incremental job step's mark (the largest value of a column, as the
//! grid shows it) written back into SQL as a literal of the column's type,
//! for each dialect: a text column's mark is always a string (`'20260102'`,
//! never the number), a timestamp keeps every fractional digit and its
//! offset whatever the session's date format, binary is a hex literal.

use arrow::datatypes::{DataType, Field};

use crate::ConnectorKind;

use super::params::{is_number, quote_string};

/// The type a mark is written back as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkType {
    Text,
    Integer,
    /// A double.
    Float,
    /// A single-precision float: compared as one (`0.1` as a double is
    /// smaller than a `real` 0.1, which would read the row again).
    Real,
    /// An exact decimal.
    Decimal,
    Bool,
    Date,
    Time,
    Timestamp,
    TimestampTz,
    Binary,
}

impl MarkType {
    pub const ALL: [MarkType; 11] = [
        MarkType::Text,
        MarkType::Integer,
        MarkType::Float,
        MarkType::Real,
        MarkType::Decimal,
        MarkType::Bool,
        MarkType::Date,
        MarkType::Time,
        MarkType::Timestamp,
        MarkType::TimestampTz,
        MarkType::Binary,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            MarkType::Text => "text",
            MarkType::Integer => "integer",
            MarkType::Float => "float",
            MarkType::Real => "real",
            MarkType::Decimal => "decimal",
            MarkType::Bool => "bool",
            MarkType::Date => "date",
            MarkType::Time => "time",
            MarkType::Timestamp => "timestamp",
            MarkType::TimestampTz => "timestamptz",
            MarkType::Binary => "binary",
        }
    }

    pub fn parse(s: &str) -> Option<MarkType> {
        MarkType::ALL.into_iter().find(|t| t.as_str() == s)
    }

    /// The type of column `field`; `decimal_text`: an exact decimal carried
    /// as text (its database type says so).
    pub fn of(field: &Field, decimal_text: bool) -> MarkType {
        match field.data_type() {
            DataType::Boolean => MarkType::Bool,
            DataType::Float32 | DataType::Float16 => MarkType::Real,
            DataType::Float64 => MarkType::Float,
            DataType::Decimal32(..) | DataType::Decimal64(..) | DataType::Decimal128(..) | DataType::Decimal256(..) => MarkType::Decimal,
            t if t.is_integer() => MarkType::Integer,
            DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View if decimal_text => MarkType::Decimal,
            DataType::Date32 => MarkType::Date,
            // Shown with its time of day (Oracle DATE).
            DataType::Date64 => MarkType::Timestamp,
            DataType::Time32(_) | DataType::Time64(_) => MarkType::Time,
            DataType::Timestamp(_, Some(_)) => MarkType::TimestampTz,
            DataType::Timestamp(_, None) => MarkType::Timestamp,
            DataType::Binary | DataType::LargeBinary | DataType::FixedSizeBinary(_) | DataType::BinaryView => MarkType::Binary,
            _ => MarkType::Text,
        }
    }

    /// `value` (as the grid shows it) as a literal of this type for `kind`.
    /// A value that isn't what the type shows (a number column holding
    /// text) becomes a string, never SQL.
    pub fn literal(self, value: &str, kind: ConnectorKind) -> String {
        use ConnectorKind as K;
        let v = value.trim();
        let s = || quote_string(v, kind);
        match self {
            MarkType::Text => quote_string(value, kind),
            MarkType::Integer | MarkType::Float if is_number(v) => v.to_string(),
            MarkType::Decimal if is_number(v) => match kind {
                // A bare `1.5` is FLOAT64 there: compared as a float.
                K::Bigquery => format!("BIGNUMERIC {}", s()),
                _ => v.to_string(),
            },
            MarkType::Real if is_number(v) => match kind {
                K::Postgres | K::Duckdb | K::Mssql => format!("CAST({v} AS REAL)"),
                K::Mysql | K::Databricks => format!("CAST({v} AS FLOAT)"),
                K::Oracle => format!("CAST({v} AS BINARY_FLOAT)"),
                K::Clickhouse => format!("toFloat32({v})"),
                _ => v.to_string(),
            },
            MarkType::Integer | MarkType::Float | MarkType::Decimal | MarkType::Real => s(),
            MarkType::Bool => {
                let on = v.eq_ignore_ascii_case("true") || v == "1";
                let off = v.eq_ignore_ascii_case("false") || v == "0";
                match (on, off, kind) {
                    (true, _, K::Mssql | K::Oracle) => "1".into(),
                    (_, true, K::Mssql | K::Oracle) => "0".into(),
                    (true, _, _) => "TRUE".into(),
                    (_, true, _) => "FALSE".into(),
                    _ => s(),
                }
            }
            MarkType::Date => match kind {
                K::Sqlite | K::Mongodb | K::Neo4j => s(),
                K::Mssql => format!("CAST({} AS date)", s()),
                K::Clickhouse => format!("toDate({})", s()),
                _ => format!("DATE {}", s()),
            },
            MarkType::Time => match kind {
                K::Mssql => format!("CAST({} AS time(7))", quote_string(&fraction_max(v, 7), kind)),
                K::Postgres | K::Duckdb | K::Mysql | K::Snowflake | K::Bigquery => format!("TIME {}", s()),
                _ => s(),
            },
            MarkType::Timestamp => match kind {
                K::Postgres | K::Mysql | K::Oracle => format!("TIMESTAMP {}", s()),
                K::Duckdb if fraction_digits(v) > 6 => format!("TIMESTAMP_NS {}", s()),
                K::Duckdb => format!("TIMESTAMP {}", s()),
                K::Mssql => format!("CAST({} AS datetime2(7))", quote_string(&fraction_max(v, 7), kind)),
                K::Snowflake => format!("{}::TIMESTAMP_NTZ", s()),
                K::Bigquery => format!("DATETIME {}", s()),
                K::Databricks => format!("TIMESTAMP_NTZ {}", s()),
                K::Clickhouse => format!("parseDateTime64BestEffort({}, 9)", s()),
                _ => s(),
            },
            MarkType::TimestampTz => match kind {
                K::Postgres | K::Duckdb => format!("TIMESTAMPTZ {}", s()),
                K::Mysql | K::Bigquery | K::Databricks => format!("TIMESTAMP {}", s()),
                // Oracle reads the offset after a space, in any NLS setting.
                K::Oracle => format!("TIMESTAMP {}", quote_string(&space_before_offset(v), kind)),
                K::Mssql => format!("CAST({} AS datetimeoffset(7))", quote_string(&fraction_max(v, 7), kind)),
                K::Snowflake => format!("{}::TIMESTAMP_TZ", s()),
                K::Clickhouse => format!("parseDateTime64BestEffort({}, 9)", s()),
                _ => s(),
            },
            MarkType::Binary if v.len().is_multiple_of(2) && v.bytes().all(|b| b.is_ascii_hexdigit()) => match kind {
                K::Postgres => format!("decode('{v}', 'hex')"),
                K::Duckdb | K::Clickhouse => format!("unhex('{v}')"),
                K::Mysql | K::Sqlite | K::Databricks => format!("X'{v}'"),
                K::Mssql => format!("0x{v}"),
                K::Oracle => format!("HEXTORAW('{v}')"),
                K::Snowflake => format!("TO_BINARY('{v}', 'HEX')"),
                K::Bigquery => format!("FROM_HEX('{v}')"),
                _ => s(),
            },
            MarkType::Binary => s(),
        }
    }
}

/// Digits after the seconds' decimal point (`…:05.1234567+02:00` → 7).
fn fraction_digits(v: &str) -> usize {
    let Some(dot) = v.rfind('.') else { return 0 };
    v[dot + 1..].bytes().take_while(u8::is_ascii_digit).count()
}

/// At most `n` fractional digits (the type's precision), the rest of the
/// text (an offset) kept.
fn fraction_max(v: &str, n: usize) -> String {
    let Some(dot) = v.rfind('.') else { return v.to_string() };
    let digits = v[dot + 1..].bytes().take_while(u8::is_ascii_digit).count();
    if digits <= n {
        return v.to_string();
    }
    format!("{}{}", &v[..dot + 1 + n], &v[dot + 1 + digits..])
}

/// `2024-03-01 10:20:30+07:00` → `2024-03-01 10:20:30 +07:00`.
fn space_before_offset(v: &str) -> String {
    let time_at = v.find(' ').or_else(|| v.find('T')).unwrap_or(0);
    match v[time_at..].rfind(['+', '-']) {
        Some(i) if !v[..time_at + i].ends_with(' ') => format!("{} {}", &v[..time_at + i], &v[time_at + i..]),
        _ => v.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ConnectorKind as K;

    #[test]
    fn text_marks_are_strings_everywhere() {
        for k in [K::Postgres, K::Mysql, K::Sqlite, K::Duckdb, K::Mssql, K::Oracle, K::Snowflake, K::Bigquery, K::Databricks] {
            for v in ["20260102", "true", "null", "1e3", "it's"] {
                let l = MarkType::Text.literal(v, k);
                assert!(l.starts_with('\'') && l.ends_with('\''), "{k:?} {v}: {l}");
            }
        }
        assert_eq!(MarkType::Text.literal("it's", K::Postgres), "'it''s'");
    }

    #[test]
    fn numbers_are_raw_unless_not_numbers() {
        assert_eq!(MarkType::Integer.literal("42", K::Postgres), "42");
        assert_eq!(MarkType::Integer.literal("1; drop table t", K::Postgres), "'1; drop table t'");
        assert_eq!(MarkType::Decimal.literal("12.50", K::Mysql), "12.50");
        assert_eq!(MarkType::Decimal.literal("12.50", K::Bigquery), "BIGNUMERIC '12.50'");
        assert_eq!(MarkType::Float.literal("2.5", K::Sqlite), "2.5");
        assert_eq!(MarkType::Real.literal("0.1", K::Postgres), "CAST(0.1 AS REAL)");
        assert_eq!(MarkType::Real.literal("0.1", K::Oracle), "CAST(0.1 AS BINARY_FLOAT)");
        assert_eq!(MarkType::Bool.literal("true", K::Mssql), "1");
        assert_eq!(MarkType::Bool.literal("false", K::Postgres), "FALSE");
    }

    #[test]
    fn dates_and_timestamps_keep_their_precision_and_offset() {
        let ts = "2026-01-02 03:04:05.123456";
        assert_eq!(MarkType::Timestamp.literal(ts, K::Postgres), "TIMESTAMP '2026-01-02 03:04:05.123456'");
        assert_eq!(MarkType::Timestamp.literal("2026-01-02 03:04:05.123456789", K::Duckdb), "TIMESTAMP_NS '2026-01-02 03:04:05.123456789'");
        assert_eq!(MarkType::Timestamp.literal(ts, K::Oracle), "TIMESTAMP '2026-01-02 03:04:05.123456'");
        assert_eq!(MarkType::Timestamp.literal("2026-01-02 03:04:05.123456789", K::Mssql), "CAST('2026-01-02 03:04:05.1234567' AS datetime2(7))");
        assert_eq!(MarkType::Timestamp.literal(ts, K::Bigquery), "DATETIME '2026-01-02 03:04:05.123456'");
        assert_eq!(MarkType::Timestamp.literal(ts, K::Snowflake), "'2026-01-02 03:04:05.123456'::TIMESTAMP_NTZ");
        assert_eq!(MarkType::Timestamp.literal(ts, K::Sqlite), "'2026-01-02 03:04:05.123456'");
        let tz = "2026-01-02 03:04:05.5+07:00";
        assert_eq!(MarkType::TimestampTz.literal(tz, K::Postgres), "TIMESTAMPTZ '2026-01-02 03:04:05.5+07:00'");
        assert_eq!(MarkType::TimestampTz.literal(tz, K::Oracle), "TIMESTAMP '2026-01-02 03:04:05.5 +07:00'");
        assert_eq!(MarkType::TimestampTz.literal("2026-01-02 03:04:05-03:30", K::Oracle), "TIMESTAMP '2026-01-02 03:04:05 -03:30'");
        assert_eq!(MarkType::TimestampTz.literal(tz, K::Mssql), "CAST('2026-01-02 03:04:05.5+07:00' AS datetimeoffset(7))");
        assert_eq!(MarkType::TimestampTz.literal(tz, K::Mysql), "TIMESTAMP '2026-01-02 03:04:05.5+07:00'");
        assert_eq!(MarkType::Date.literal("2026-01-02", K::Mssql), "CAST('2026-01-02' AS date)");
        assert_eq!(MarkType::Date.literal("2026-01-02", K::Mysql), "DATE '2026-01-02'");
        assert_eq!(MarkType::Time.literal("03:04:05.5", K::Postgres), "TIME '03:04:05.5'");
    }

    #[test]
    fn binary_marks_are_hex_literals() {
        assert_eq!(MarkType::Binary.literal("00ff5c", K::Postgres), "decode('00ff5c', 'hex')");
        assert_eq!(MarkType::Binary.literal("00ff5c", K::Mssql), "0x00ff5c");
        assert_eq!(MarkType::Binary.literal("00ff5c", K::Mysql), "X'00ff5c'");
        assert_eq!(MarkType::Binary.literal("00ff5c", K::Oracle), "HEXTORAW('00ff5c')");
        assert_eq!(MarkType::Binary.literal("00ff5c", K::Duckdb), "unhex('00ff5c')");
        // Not hex: a string, never SQL.
        assert_eq!(MarkType::Binary.literal("zz') or 1=1 --", K::Postgres), "'zz'') or 1=1 --'");
    }

    #[test]
    fn types_of_columns() {
        let f = |t: DataType| Field::new("c", t, true);
        assert_eq!(MarkType::of(&f(DataType::Int64), false), MarkType::Integer);
        assert_eq!(MarkType::of(&f(DataType::Utf8), false), MarkType::Text);
        assert_eq!(MarkType::of(&f(DataType::Utf8), true), MarkType::Decimal);
        assert_eq!(MarkType::of(&f(DataType::Float32), false), MarkType::Real);
        assert_eq!(MarkType::of(&f(DataType::Timestamp(arrow::datatypes::TimeUnit::Microsecond, Some("UTC".into()))), false), MarkType::TimestampTz);
        for t in MarkType::ALL {
            assert_eq!(MarkType::parse(t.as_str()), Some(t));
        }
    }
}
