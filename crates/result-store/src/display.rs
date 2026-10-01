//! Converting Arrow values into display strings shared by the grid, find,
//! copy and export.

use databrain_connector_core::arrow::array::{Array, ArrayRef, StringArray, StringBuilder};
use databrain_connector_core::arrow::datatypes::{DataType, Field};
use databrain_connector_core::arrow::error::ArrowError;
use databrain_connector_core::arrow::util::display::{ArrayFormatter, FormatOptions};
use databrain_connector_core::value::META_DB_TYPE;
use serde::Serialize;

pub(crate) const TS_FORMAT: &str = "%Y-%m-%d %H:%M:%S%.f";
pub(crate) const TS_TZ_FORMAT: &str = "%Y-%m-%d %H:%M:%S%.f%:z";

pub fn format_options() -> FormatOptions<'static> {
    FormatOptions::new()
        .with_null("")
        .with_display_error(true)
        .with_timestamp_format(Some(TS_FORMAT))
        .with_timestamp_tz_format(Some(TS_TZ_FORMAT))
        .with_datetime_format(Some(TS_FORMAT))
}

/// Format every value of `array` into a `StringArray` (nulls stay null).
pub fn to_display(array: &dyn Array) -> Result<StringArray, ArrowError> {
    if let Some(s) = array.as_any().downcast_ref::<StringArray>() {
        return Ok(s.clone());
    }
    let opts = format_options();
    let f = ArrayFormatter::try_new(array, &opts)?;
    let mut b = StringBuilder::with_capacity(array.len(), array.len() * 8);
    for i in 0..array.len() {
        if array.is_null(i) {
            b.append_null();
        } else {
            b.append_value(f.value(i).to_string());
        }
    }
    Ok(b.finish())
}

/// Format a single cell (`None` for SQL NULL).
pub fn cell(formatter: &ArrayFormatter<'_>, array: &ArrayRef, row: usize) -> Option<String> {
    if array.is_null(row) {
        None
    } else {
        Some(formatter.value(row).to_string())
    }
}

/// Coarse type family, used by the UI for alignment/icons and by filters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TypeFamily {
    Number,
    Text,
    Bool,
    Date,
    Time,
    Binary,
    Other,
}

pub fn family(field: &Field) -> TypeFamily {
    match field.data_type() {
        DataType::Boolean => TypeFamily::Bool,
        t if t.is_numeric() => TypeFamily::Number,
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => {
            if is_decimal_text(field) {
                TypeFamily::Number
            } else {
                TypeFamily::Text
            }
        }
        DataType::Date32 | DataType::Date64 | DataType::Timestamp(_, _) => TypeFamily::Date,
        DataType::Time32(_) | DataType::Time64(_) => TypeFamily::Time,
        DataType::Binary | DataType::LargeBinary | DataType::FixedSizeBinary(_) => {
            TypeFamily::Binary
        }
        _ => TypeFamily::Other,
    }
}

/// Exact decimals are transported as text; treat them as numbers for sort/filter.
pub fn is_decimal_text(field: &Field) -> bool {
    matches!(field.data_type(), DataType::Utf8)
        && field.metadata().get(META_DB_TYPE).is_some_and(|t| {
            let t = t.to_ascii_lowercase();
            t.starts_with("numeric") || t.starts_with("decimal") || t == "newdecimal" || t == "money"
        })
}

#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub struct ColumnMeta {
    pub name: String,
    pub data_type: String,
    pub db_type: Option<String>,
    pub family: TypeFamily,
}

pub fn column_meta(field: &Field) -> ColumnMeta {
    ColumnMeta {
        name: field.name().clone(),
        data_type: field.data_type().to_string(),
        db_type: field.metadata().get(META_DB_TYPE).cloned(),
        family: family(field),
    }
}

#[cfg(test)]
mod tz_tests {
    use super::*;
    use databrain_connector_core::arrow::array::TimestampMicrosecondArray;
    use databrain_connector_core::{BatchBuilder, ColType, Column, Value};

    /// Named time zones (Databricks/Snowflake TIMESTAMP → "UTC", Parquet
    /// files with zones like "Asia/Ho_Chi_Minh") must format without errors.
    #[test]
    fn formats_named_timezones() {
        let mut b = BatchBuilder::new(&[Column::new("ts", ColType::TimestampTz, "TIMESTAMP")], 1);
        b.push_row(vec![Value::TimestampTz(1_709_288_430_500_000)]);
        let batch = b.finish().unwrap();
        let s = to_display(batch.column(0).as_ref()).unwrap();
        assert_eq!(s.value(0), "2024-03-01 10:20:30.500+00:00");

        let a = TimestampMicrosecondArray::from(vec![1_709_288_430_000_000i64]).with_timezone("Asia/Ho_Chi_Minh");
        assert_eq!(to_display(&a).unwrap().value(0), "2024-03-01 17:20:30+07:00");
    }
}
