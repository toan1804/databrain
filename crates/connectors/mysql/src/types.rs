//! Column type mapping and value conversion.

use databrain_connector_core::ColType;
use databrain_connector_core::Value;
use databrain_connector_core::value::parse_date;
use databrain_connector_core::value::parse_datetime;
use mysql_async::consts::ColumnFlags;
use mysql_async::consts::ColumnType;

/// Character set id MySQL uses for binary strings.
pub(crate) const BINARY_CHARSET: u16 = 63;

pub(crate) fn col_type(c: &mysql_async::Column) -> ColType {
    let unsigned = c.flags().contains(ColumnFlags::UNSIGNED_FLAG);
    let binary = c.character_set() == BINARY_CHARSET;
    use ColumnType::*;
    match c.column_type() {
        MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_LONG | MYSQL_TYPE_INT24 | MYSQL_TYPE_YEAR => ColType::Int64,
        MYSQL_TYPE_LONGLONG if unsigned => ColType::UInt64,
        MYSQL_TYPE_LONGLONG => ColType::Int64,
        MYSQL_TYPE_FLOAT | MYSQL_TYPE_DOUBLE => ColType::Float64,
        MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => ColType::Date,
        MYSQL_TYPE_DATETIME | MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_DATETIME2 | MYSQL_TYPE_TIMESTAMP2 => ColType::Timestamp,
        MYSQL_TYPE_BIT => ColType::Binary,
        MYSQL_TYPE_TINY_BLOB
        | MYSQL_TYPE_MEDIUM_BLOB
        | MYSQL_TYPE_LONG_BLOB
        | MYSQL_TYPE_BLOB
        | MYSQL_TYPE_VAR_STRING
        | MYSQL_TYPE_STRING
        | MYSQL_TYPE_VARCHAR
        | MYSQL_TYPE_GEOMETRY
            if binary =>
        {
            ColType::Binary
        }
        // DECIMAL stays text to keep exact precision; TIME can exceed 24h.
        _ => ColType::Utf8,
    }
}

/// The type as MySQL writes it (`varchar(255)`, `decimal(12,2)`,
/// `int unsigned`), from the result's column definition.
pub(crate) fn type_name(c: &mysql_async::Column) -> String {
    let binary = c.character_set() == BINARY_CHARSET;
    // Lengths are in bytes; utf8mb4 = 4 per character, utf8mb3 = 3.
    let per_char = match c.character_set() {
        45 | 46 | 224..=247 | 255 => 4,
        33 | 83 | 192..=215 => 3,
        _ => 1,
    };
    sql_type_name(c.column_type(), c.column_length(), c.decimals(), c.flags().contains(ColumnFlags::UNSIGNED_FLAG), binary, per_char)
}

pub(crate) fn sql_type_name(t: ColumnType, len: u32, decimals: u8, unsigned: bool, binary: bool, per_char: u32) -> String {
    use ColumnType::*;
    let chars = len / per_char.max(1);
    let fsp = if decimals > 0 && decimals <= 6 { format!("({decimals})") } else { String::new() };
    let name = match t {
        MYSQL_TYPE_TINY => "tinyint".to_string(),
        MYSQL_TYPE_SHORT => "smallint".into(),
        MYSQL_TYPE_INT24 => "mediumint".into(),
        MYSQL_TYPE_LONG => "int".into(),
        MYSQL_TYPE_LONGLONG => "bigint".into(),
        MYSQL_TYPE_FLOAT => "float".into(),
        MYSQL_TYPE_DOUBLE => "double".into(),
        MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => {
            // Display length counts the sign and the point.
            let p = len.saturating_sub(u32::from(decimals > 0) + u32::from(!unsigned));
            format!("decimal({p},{decimals})")
        }
        MYSQL_TYPE_YEAR => "year".into(),
        MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => "date".into(),
        MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => format!("time{fsp}"),
        MYSQL_TYPE_DATETIME | MYSQL_TYPE_DATETIME2 => format!("datetime{fsp}"),
        MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_TIMESTAMP2 => format!("timestamp{fsp}"),
        MYSQL_TYPE_BIT => format!("bit({len})"),
        MYSQL_TYPE_JSON => "json".into(),
        MYSQL_TYPE_ENUM => "enum".into(),
        MYSQL_TYPE_SET => "set".into(),
        MYSQL_TYPE_GEOMETRY => "geometry".into(),
        MYSQL_TYPE_VARCHAR | MYSQL_TYPE_VAR_STRING if binary => format!("varbinary({len})"),
        MYSQL_TYPE_VARCHAR | MYSQL_TYPE_VAR_STRING => format!("varchar({chars})"),
        MYSQL_TYPE_STRING if binary => format!("binary({len})"),
        MYSQL_TYPE_STRING => format!("char({chars})"),
        MYSQL_TYPE_TINY_BLOB | MYSQL_TYPE_MEDIUM_BLOB | MYSQL_TYPE_LONG_BLOB | MYSQL_TYPE_BLOB => {
            // The protocol says "blob" for all; the length (in characters for text) tells which.
            let size = match if binary { len } else { chars } {
                0..=255 => "tiny",
                256..=65_535 => "",
                65_536..=16_777_215 => "medium",
                _ => "long",
            };
            format!("{size}{}", if binary { "blob" } else { "text" })
        }
        other => format!("{other:?}").trim_start_matches("MYSQL_TYPE_").to_ascii_lowercase(),
    };
    let int = matches!(
        t,
        MYSQL_TYPE_TINY
            | MYSQL_TYPE_SHORT
            | MYSQL_TYPE_INT24
            | MYSQL_TYPE_LONG
            | MYSQL_TYPE_LONGLONG
            | MYSQL_TYPE_FLOAT
            | MYSQL_TYPE_DOUBLE
            | MYSQL_TYPE_DECIMAL
            | MYSQL_TYPE_NEWDECIMAL
    );
    if unsigned && int { format!("{name} unsigned") } else { name }
}

pub(crate) fn convert(t: ColType, v: mysql_async::Value) -> Value {
    use mysql_async::Value as M;
    match v {
        M::NULL => Value::Null,
        M::Int(i) => Value::Int(i),
        M::UInt(u) => Value::UInt(u),
        M::Float(f) => Value::Float(f as f64),
        M::Double(f) => Value::Float(f),
        M::Bytes(b) => match t {
            ColType::Binary => Value::Bytes(b),
            _ => {
                let s = String::from_utf8(b).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
                match t {
                    ColType::Date => parse_date(&s).map(Value::Date).unwrap_or(Value::Text(s)),
                    ColType::Timestamp => parse_datetime(&s).map(Value::Timestamp).unwrap_or(Value::Text(s)),
                    _ => Value::Text(s),
                }
            }
        },
        other => Value::Text(other.as_sql(true).trim_matches('\'').to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_text_values() {
        assert_eq!(convert(ColType::Date, mysql_async::Value::Bytes(b"2024-01-02".to_vec())), Value::Date(parse_date("2024-01-02").unwrap()));
        assert_eq!(convert(ColType::Date, mysql_async::Value::Bytes(b"0000-00-00".to_vec())), Value::Text("0000-00-00".into()));
        assert_eq!(convert(ColType::Utf8, mysql_async::Value::Bytes(b"12.50".to_vec())), Value::Text("12.50".into()));
    }
}

#[cfg(test)]
mod type_name_tests {
    use super::*;
    use ColumnType::*;
    #[test]
    fn names_types_like_mysql() {
        assert_eq!(sql_type_name(MYSQL_TYPE_VAR_STRING, 1020, 0, false, false, 4), "varchar(255)");
        assert_eq!(sql_type_name(MYSQL_TYPE_NEWDECIMAL, 14, 2, false, false, 1), "decimal(12,2)");
        assert_eq!(sql_type_name(MYSQL_TYPE_NEWDECIMAL, 12, 2, true, false, 1), "decimal(11,2) unsigned");
        assert_eq!(sql_type_name(MYSQL_TYPE_LONGLONG, 20, 0, true, false, 1), "bigint unsigned");
        assert_eq!(sql_type_name(MYSQL_TYPE_DATETIME, 26, 6, false, false, 1), "datetime(6)");
        assert_eq!(sql_type_name(MYSQL_TYPE_BLOB, 262_140, 0, false, false, 4), "text");
        assert_eq!(sql_type_name(MYSQL_TYPE_BLOB, 4_294_967_295, 0, false, true, 1), "longblob");
        assert_eq!(sql_type_name(MYSQL_TYPE_STRING, 16, 0, false, true, 1), "binary(16)");
    }
}
