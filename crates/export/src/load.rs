//! SQL to copy rows into a table of another database: `CREATE TABLE` from
//! the rows' Arrow types and batched `INSERT` statements, per dialect.

use databrain_connector_core::arrow::array::{Array, RecordBatch};
use databrain_connector_core::arrow::datatypes::{DataType, Field, SchemaRef};
use databrain_connector_core::arrow::util::display::ArrayFormatter;
use databrain_connector_core::{ConnectorKind, quote_ident, quote_literal, quote_path};
use databrain_result_store::display::{format_options, is_decimal_text};

use crate::Result;

/// Most rows per INSERT (SQL Server allows 1000 in one VALUES list).
const ROWS_PER_INSERT: usize = 500;
/// Oracle builds `INSERT ALL`, slower to parse: smaller batches.
const ROWS_PER_INSERT_ORACLE: usize = 100;
/// Statements are cut before this much SQL text.
const MAX_STATEMENT_BYTES: usize = 1 << 20;

#[derive(Clone, Copy)]
enum Lit {
    Number,
    Bool,
    Binary,
    Date,
    Timestamp,
    Text,
}

fn lit_kind(f: &Field) -> Lit {
    match f.data_type() {
        DataType::Boolean => Lit::Bool,
        t if t.is_numeric() => Lit::Number,
        DataType::Binary | DataType::LargeBinary | DataType::FixedSizeBinary(_) => Lit::Binary,
        DataType::Date32 | DataType::Date64 => Lit::Date,
        DataType::Timestamp(_, None) => Lit::Timestamp,
        _ if is_decimal_text(f) => Lit::Number,
        _ => Lit::Text,
    }
}

/// Column type for a `CREATE TABLE` in `dialect`.
pub fn column_type(dialect: ConnectorKind, f: &Field) -> &'static str {
    use ConnectorKind as K;
    let dt = f.data_type();
    if is_decimal_text(f) {
        return match dialect {
            K::Oracle => "NUMBER",
            K::Bigquery => "BIGNUMERIC",
            K::Snowflake | K::Databricks => "DECIMAL(38, 10)",
            _ => "NUMERIC",
        };
    }
    match dt {
        DataType::Boolean => match dialect {
            K::Mssql => "BIT",
            K::Oracle => "NUMBER(1)",
            K::Bigquery => "BOOL",
            _ => "BOOLEAN",
        },
        DataType::Int8 | DataType::Int16 | DataType::Int32 | DataType::Int64 | DataType::UInt8 | DataType::UInt16 | DataType::UInt32 | DataType::UInt64 => match dialect {
            K::Oracle => "NUMBER(19)",
            K::Bigquery => "INT64",
            K::Sqlite => "INTEGER",
            _ => "BIGINT",
        },
        DataType::Float16 | DataType::Float32 | DataType::Float64 => match dialect {
            K::Postgres => "DOUBLE PRECISION",
            K::Mssql => "FLOAT",
            K::Oracle => "BINARY_DOUBLE",
            K::Bigquery => "FLOAT64",
            K::Sqlite => "REAL",
            _ => "DOUBLE",
        },
        DataType::Decimal128(..) | DataType::Decimal256(..) => match dialect {
            K::Oracle => "NUMBER",
            K::Bigquery => "BIGNUMERIC",
            K::Snowflake | K::Databricks => "DECIMAL(38, 10)",
            _ => "NUMERIC",
        },
        DataType::Date32 | DataType::Date64 => "DATE",
        DataType::Timestamp(_, tz) => match (dialect, tz.is_some()) {
            (K::Mssql, false) => "DATETIME2",
            (K::Mssql, true) => "DATETIMEOFFSET",
            (K::Mysql, _) => "DATETIME(6)",
            (K::Postgres, true) => "TIMESTAMPTZ",
            (K::Oracle, true) => "TIMESTAMP WITH TIME ZONE",
            (K::Snowflake, true) => "TIMESTAMP_TZ",
            (K::Bigquery, false) => "DATETIME",
            (K::Bigquery, true) => "TIMESTAMP",
            (K::Duckdb, true) => "TIMESTAMPTZ",
            _ => "TIMESTAMP",
        },
        DataType::Time32(_) | DataType::Time64(_) => match dialect {
            K::Oracle => "VARCHAR2(32)",
            _ => "TIME",
        },
        DataType::Binary | DataType::LargeBinary | DataType::FixedSizeBinary(_) => match dialect {
            K::Postgres => "BYTEA",
            K::Mssql => "VARBINARY(MAX)",
            K::Bigquery => "BYTES",
            K::Snowflake | K::Databricks => "BINARY",
            _ => "BLOB",
        },
        _ => match dialect {
            K::Mssql => "NVARCHAR(MAX)",
            K::Oracle => "CLOB",
            K::Bigquery | K::Databricks => "STRING",
            K::Snowflake => "VARCHAR",
            _ => "TEXT",
        },
    }
}

/// `CREATE TABLE table (col type, …)` for the columns of `schema`.
pub fn create_table_sql(dialect: ConnectorKind, table: &str, schema: &SchemaRef) -> String {
    let cols: Vec<String> = schema.fields().iter().map(|f| format!("{} {}", quote_ident(dialect, f.name()), column_type(dialect, f))).collect();
    format!("CREATE TABLE {} ({})", quote_path(dialect, table), cols.join(", "))
}

/// `DROP TABLE IF EXISTS` in `dialect` (Oracle: a block ignoring "does not exist").
pub fn drop_table_sql(dialect: ConnectorKind, table: &str) -> String {
    let t = quote_path(dialect, table);
    match dialect {
        ConnectorKind::Oracle => format!("BEGIN EXECUTE IMMEDIATE 'DROP TABLE {}'; EXCEPTION WHEN OTHERS THEN IF SQLCODE != -942 THEN RAISE; END IF; END;", t.replace('\'', "''")),
        _ => format!("DROP TABLE IF EXISTS {t}"),
    }
}

fn literal(cell: Option<&str>, kind: Lit, dialect: ConnectorKind) -> String {
    use ConnectorKind as K;
    let Some(s) = cell else { return "NULL".into() };
    match kind {
        Lit::Number if s.parse::<f64>().is_ok_and(|f| f.is_finite()) => s.to_string(),
        Lit::Bool => match dialect {
            K::Postgres | K::Duckdb | K::Snowflake | K::Bigquery | K::Databricks | K::Mysql => s.to_uppercase(),
            _ => if s == "true" { "1".into() } else { "0".into() },
        },
        Lit::Binary => match dialect {
            K::Postgres => format!("'\\x{s}'::bytea"),
            K::Mssql => format!("0x{s}"),
            K::Oracle => format!("HEXTORAW('{s}')"),
            K::Bigquery => format!("FROM_HEX('{s}')"),
            K::Snowflake => format!("TO_BINARY('{s}', 'HEX')"),
            K::Databricks => format!("unhex('{s}')"),
            _ => format!("X'{s}'"),
        },
        // Oracle doesn't read text as a date without NLS settings: typed literals.
        Lit::Date if dialect == K::Oracle => format!("DATE {}", quote_literal(s)),
        Lit::Timestamp if dialect == K::Oracle => format!("TIMESTAMP {}", quote_literal(s)),
        _ => match dialect {
            K::Mssql => format!("N{}", quote_literal(s)),
            K::Mysql | K::Databricks | K::Bigquery => format!("'{}'", s.replace('\\', "\\\\").replace('\'', "''")),
            _ => quote_literal(s),
        },
    }
}

/// INSERT statements for every row, several rows per statement.
pub fn insert_statements(dialect: ConnectorKind, table: &str, schema: &SchemaRef, batches: &[RecordBatch]) -> Result<Vec<String>> {
    let fopts = format_options();
    let t = quote_path(dialect, table);
    let cols = schema.fields().iter().map(|f| quote_ident(dialect, f.name())).collect::<Vec<_>>().join(", ");
    let kinds: Vec<Lit> = schema.fields().iter().map(|f| lit_kind(f)).collect();
    let per = if dialect == ConnectorKind::Oracle { ROWS_PER_INSERT_ORACLE } else { ROWS_PER_INSERT };
    let mut out = Vec::new();
    let mut rows: Vec<String> = Vec::new();
    let mut bytes = 0;
    let flush = |rows: &mut Vec<String>, out: &mut Vec<String>| {
        if rows.is_empty() {
            return;
        }
        out.push(match dialect {
            ConnectorKind::Oracle => format!(
                "INSERT ALL {} SELECT 1 FROM DUAL",
                rows.iter().map(|r| format!("INTO {t} ({cols}) VALUES {r}")).collect::<Vec<_>>().join(" ")
            ),
            _ => format!("INSERT INTO {t} ({cols}) VALUES {}", rows.join(", ")),
        });
        rows.clear();
    };
    for batch in batches {
        let fmts: Vec<ArrayFormatter<'_>> = batch.columns().iter().map(|a| ArrayFormatter::try_new(a.as_ref(), &fopts)).collect::<std::result::Result<_, _>>()?;
        for r in 0..batch.num_rows() {
            let vals: Vec<String> = batch
                .columns()
                .iter()
                .zip(&fmts)
                .zip(&kinds)
                .map(|((a, f), k)| literal((!a.is_null(r)).then(|| f.value(r).to_string()).as_deref(), *k, dialect))
                .collect();
            let row = format!("({})", vals.join(", "));
            if rows.len() >= per || bytes + row.len() > MAX_STATEMENT_BYTES {
                flush(&mut rows, &mut out);
                bytes = 0;
            }
            bytes += row.len() + 2;
            rows.push(row);
        }
    }
    flush(&mut rows, &mut out);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use databrain_connector_core::arrow::array::{BooleanArray, Int64Array, StringArray, TimestampMicrosecondArray};
    use databrain_connector_core::arrow::datatypes::{Schema, TimeUnit};
    use std::sync::Arc;

    fn rows(n: usize) -> (SchemaRef, Vec<RecordBatch>) {
        let schema: SchemaRef = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, true),
            Field::new("ok", DataType::Boolean, true),
            Field::new("at", DataType::Timestamp(TimeUnit::Microsecond, None), true),
        ]));
        let b = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from_iter_values(0..n as i64)),
                Arc::new(StringArray::from_iter((0..n).map(|i| if i == 1 { None } else { Some(format!("it's {i}")) }))),
                Arc::new(BooleanArray::from_iter((0..n).map(|i| Some(i % 2 == 0)))),
                Arc::new(TimestampMicrosecondArray::from_iter((0..n).map(|_| Some(1_767_225_600_000_000)))),
            ],
        )
        .unwrap();
        (schema, vec![b])
    }

    #[test]
    fn create_and_insert_per_dialect() {
        let (schema, batches) = rows(2);
        assert_eq!(create_table_sql(ConnectorKind::Postgres, "app.t", &schema), r#"CREATE TABLE "app"."t" ("id" BIGINT, "name" TEXT, "ok" BOOLEAN, "at" TIMESTAMP)"#);
        assert_eq!(create_table_sql(ConnectorKind::Mssql, "t", &schema), "CREATE TABLE [t] ([id] BIGINT, [name] NVARCHAR(MAX), [ok] BIT, [at] DATETIME2)");
        assert_eq!(
            insert_statements(ConnectorKind::Postgres, "t", &schema, &batches).unwrap(),
            vec![r#"INSERT INTO "t" ("id", "name", "ok", "at") VALUES (0, 'it''s 0', TRUE, '2026-01-01 00:00:00'), (1, NULL, FALSE, '2026-01-01 00:00:00')"#]
        );
        let ora = insert_statements(ConnectorKind::Oracle, "t", &schema, &batches).unwrap();
        assert!(ora[0].starts_with(r#"INSERT ALL INTO "t" ("id", "name", "ok", "at") VALUES (0, 'it''s 0', 1, TIMESTAMP '2026-01-01 00:00:00') INTO"#), "{}", ora[0]);
        assert!(ora[0].ends_with("SELECT 1 FROM DUAL"));
        assert!(insert_statements(ConnectorKind::Mssql, "t", &schema, &batches).unwrap()[0].contains("N'it''s 0', 1,"));
        assert!(drop_table_sql(ConnectorKind::Oracle, "t").contains("-942"));
    }

    #[test]
    fn batches_rows() {
        let (schema, batches) = rows(1201);
        let s = insert_statements(ConnectorKind::Mysql, "t", &schema, &batches).unwrap();
        assert_eq!(s.len(), 3);
        assert_eq!(s[2].matches("), (").count() + 1, 201);
        assert!(insert_statements(ConnectorKind::Mysql, "t", &schema, &[]).unwrap().is_empty());
    }
}
