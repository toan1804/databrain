use databrain_connector_core::{ColumnInfo, ConnectorKind};

pub(crate) fn quote(s: &str) -> String {
    databrain_connector_core::quote_ident(ConnectorKind::Postgres, s)
}

/// Approximate CREATE TABLE statement from column metadata.
pub(crate) fn table_ddl(schema: &str, name: &str, columns: &[ColumnInfo]) -> String {
    let mut lines: Vec<String> = columns
        .iter()
        .map(|c| {
            let mut l = format!("    {} {}", quote(&c.name), c.data_type);
            if let Some(d) = &c.default {
                l.push_str(" DEFAULT ");
                l.push_str(d);
            }
            if !c.nullable {
                l.push_str(" NOT NULL");
            }
            l
        })
        .collect();
    let pk: Vec<String> = columns.iter().filter(|c| c.is_primary_key).map(|c| quote(&c.name)).collect();
    if !pk.is_empty() {
        lines.push(format!("    PRIMARY KEY ({})", pk.join(", ")));
    }
    format!("CREATE TABLE {}.{} (\n{}\n);", quote(schema), quote(name), lines.join(",\n"))
}
