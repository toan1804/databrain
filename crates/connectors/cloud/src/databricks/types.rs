//! Mapping of Databricks column types to connector types.

use databrain_connector_core::ColType;
use databrain_connector_core::value::Column;

pub(super) fn col_type(type_name: &str) -> ColType {
    match type_name.to_ascii_uppercase().as_str() {
        "BOOLEAN" => ColType::Bool,
        "BYTE" | "TINYINT" | "SHORT" | "SMALLINT" | "INT" | "INTEGER" | "LONG" | "BIGINT" => ColType::Int64,
        "FLOAT" | "DOUBLE" => ColType::Float64,
        "DATE" => ColType::Date,
        "TIMESTAMP" => ColType::TimestampTz,
        "TIMESTAMP_NTZ" => ColType::Timestamp,
        "BINARY" => ColType::Utf8,
        _ => ColType::Utf8, // DECIMAL kept exact, complex types as JSON text
    }
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
    }
}
