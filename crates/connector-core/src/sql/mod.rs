//! Client-side SQL helpers: splitting scripts into statements and classifying
//! statements (read vs. write) for safety checks.

use sqlparser::dialect::{
    BigQueryDialect, ClickHouseDialect, DatabricksDialect, Dialect, DuckDbDialect, GenericDialect, MsSqlDialect, MySqlDialect, OracleDialect,
    PostgreSqlDialect, SQLiteDialect, SnowflakeDialect,
};

use crate::traits::SqlParser;
use crate::types::ConnectorKind;

mod classify;
mod combined;
mod mark;
mod params;
mod parsed;
mod read_only;
mod redact;
mod splitter;
mod statement;

pub(crate) use classify::keyword_kind;
pub use classify::{Classification, StatementKind, classify, delta_describe};
pub use combined::{Combined, CombinedError, CombinedPart, ConnectionRef, looks_combined, parse_combined, with_list_end};
pub use mark::MarkType;
pub use params::{
    ParamMap, ParamSpan, ParamValue, find_parameters, quote_string, render_param, substitute_known_parameters, substitute_parameters, substitute_with_map,
    written_position,
};
pub use parsed::{ParsedStatement, parse_script};
pub use read_only::{read_only_allows, unknown_functions};
pub use redact::{redact_key_values, redact_secrets, redact_text, redact_url_passwords};
pub use splitter::{StatementSpan, code_text, leading_keyword, split_statements, split_statements_with_parts, split_with, statement_at};
pub(crate) use splitter::{code_text_values, skip_token, words};
pub use statement::{Checked, Statement};

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
