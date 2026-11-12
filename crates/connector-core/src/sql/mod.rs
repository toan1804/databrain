//! Client-side SQL helpers: splitting scripts into statements and classifying
//! statements (read vs. write) for safety checks.

use sqlparser::dialect::{
    BigQueryDialect, ClickHouseDialect, DatabricksDialect, Dialect, DuckDbDialect, GenericDialect, MsSqlDialect, MySqlDialect, OracleDialect,
    PostgreSqlDialect, SQLiteDialect, SnowflakeDialect,
};

use crate::traits::SqlParser;
use crate::types::ConnectorKind;

mod classify;
mod params;
mod read_only;
mod redact;
mod splitter;

pub(crate) use classify::keyword_kind;
pub use classify::{Classification, StatementKind, classify};
pub use params::{ParamSpan, ParamValue, find_parameters, quote_string, render_param, substitute_parameters};
pub use read_only::{read_only_allows, unknown_functions};
pub use redact::{redact_key_values, redact_secrets, redact_text, redact_url_passwords};
pub use splitter::{StatementSpan, leading_keyword, split_statements, split_with, statement_at};
pub(crate) use splitter::{skip_token, strip_leading_comments, words};

pub fn dialect_for(kind: ConnectorKind) -> Box<dyn Dialect> {
    dialect_of(kind.traits().parser)
}

fn dialect_of(parser: SqlParser) -> Box<dyn Dialect> {
    match parser {
        SqlParser::Postgres => Box::new(PostgreSqlDialect {}),
        SqlParser::Mysql => Box::new(MySqlDialect {}),
        SqlParser::Sqlite => Box::new(SQLiteDialect {}),
        SqlParser::Mssql => Box::new(MsSqlDialect {}),
        SqlParser::Oracle => Box::new(OracleDialect {}),
        SqlParser::Snowflake => Box::new(SnowflakeDialect {}),
        SqlParser::Databricks => Box::new(DatabricksDialect {}),
        SqlParser::Bigquery => Box::new(BigQueryDialect {}),
        SqlParser::Duckdb => Box::new(DuckDbDialect {}),
        SqlParser::Clickhouse => Box::new(ClickHouseDialect {}),
        SqlParser::Generic => Box::new(GenericDialect {}),
    }
}
