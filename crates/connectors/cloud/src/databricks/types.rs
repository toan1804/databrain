//! Mapping of Databricks column types to connector types.

use databrain_connector_core::ColType;
use databrain_connector_core::value::{Column, Value};

pub(super) fn col_type(type_name: &str) -> ColType {
    match type_name.to_ascii_uppercase().as_str() {
        "BOOLEAN" => ColType::Bool,
        "BYTE" | "TINYINT" | "SHORT" | "SMALLINT" | "INT" | "INTEGER" | "LONG" | "BIGINT" => ColType::Int64,
        "FLOAT" | "DOUBLE" => ColType::Float64,
        "DATE" => ColType::Date,
        "TIMESTAMP" => ColType::TimestampTz,
        "TIMESTAMP_NTZ" => ColType::Timestamp,
        // Bytes either way: Arrow chunks carry them, JSON cells as base64.
        "BINARY" => ColType::Binary,
        _ => ColType::Utf8, // DECIMAL kept exact, complex types as JSON text
    }
}

/// A JSON result cell as a value of `t`: BINARY comes as base64 text, the
/// rest as [`json_cell`](crate::common::json_cell) reads it.
pub(super) fn cell(t: ColType, v: &serde_json::Value) -> Value {
    use base64::Engine;
    if t == ColType::Binary
        && let Some(s) = v.as_str()
        && let Ok(b) = base64::engine::general_purpose::STANDARD.decode(s)
    {
        return Value::Bytes(b);
    }
    crate::common::json_cell(t, v)
}

/// Result columns from the statement manifest.
pub(super) fn manifest_columns(resp: &serde_json::Value) -> Vec<Column> {
    resp.pointer("/manifest/schema/columns")
        .and_then(|c| c.as_array())
        .map(|cols| {
            cols.iter()
                .map(|c| {
                    let t = c["type_name"].as_str().unwrap_or("STRING");
                    let text = c["type_text"].as_str().unwrap_or(t);
                    Column::new(c["name"].as_str().unwrap_or(""), col_type(t), text.to_ascii_lowercase())
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_types() {
        assert_eq!(col_type("BIGINT"), ColType::Int64);
        assert_eq!(col_type("DECIMAL"), ColType::Utf8);
        assert_eq!(col_type("TIMESTAMP"), ColType::TimestampTz);
        assert_eq!(col_type("binary"), ColType::Binary);
    }

    #[test]
    fn binary_cells_are_decoded_from_base64() {
        assert_eq!(cell(ColType::Binary, &serde_json::json!("aGk=")), Value::Bytes(b"hi".to_vec()));
        assert_eq!(cell(ColType::Binary, &serde_json::Value::Null), Value::Null);
        // Not base64: kept (as the bytes of the text, by the builder).
        assert_eq!(cell(ColType::Binary, &serde_json::json!("not base64!")), Value::Text("not base64!".into()));
        assert_eq!(cell(ColType::Utf8, &serde_json::json!("aGk=")), Value::Text("aGk=".into()));
    }
}
