//! Catalog helpers for the explorer (`system` tables).

use databrain_connector_core::ColumnInfo;
use databrain_connector_core::ObjectKind;

pub(crate) fn object_kind(engine: &str) -> ObjectKind {
    match engine {
        "View" | "LiveView" | "WindowView" => ObjectKind::View,
        "MaterializedView" => ObjectKind::MaterializedView,
        e if [
            "MySQL",
            "PostgreSQL",
            "MongoDB",
            "Redis",
            "SQLite",
            "ODBC",
            "JDBC",
            "URL",
            "S3",
            "S3Queue",
            "AzureBlobStorage",
            "AzureQueue",
            "HDFS",
            "File",
            "Kafka",
            "RabbitMQ",
            "NATS",
            "DeltaLake",
            "Hudi",
            "ExternalDistributed",
        ]
        .contains(&e)
            || e.starts_with("Iceberg") =>
        {
            ObjectKind::ForeignTable
        }
        _ => ObjectKind::Table,
    }
}

pub(crate) fn opt_text(v: &Option<String>) -> Option<String> {
    v.clone().filter(|s| !s.trim().is_empty())
}

pub(crate) fn column_info(row: &[Option<String>]) -> ColumnInfo {
    let get = |i: usize| row.get(i).cloned().flatten().unwrap_or_default();
    let ty = get(1);
    let default_kind = get(2);
    let default_expr = get(3);
    let outer = ty.trim();
    let nullable = outer.strip_prefix("LowCardinality(").unwrap_or(outer).starts_with("Nullable(");
    ColumnInfo {
        name: get(0),
        nullable,
        data_type: ty,
        is_primary_key: get(5) == "1",
        default: match default_kind.as_str() {
            "" => None,
            "DEFAULT" => Some(default_expr),
            k => Some(format!("{k} {default_expr}")),
        },
        comment: opt_text(&row.get(4).cloned().flatten()),
    }
}

pub(crate) const COLUMNS: &str = "SELECT name, type, default_kind, default_expression, comment, is_in_primary_key FROM system.columns";
