//! Cloud data warehouse connectors over HTTPS APIs: Snowflake, Databricks
//! SQL and Google BigQuery.

#[cfg(feature = "bigquery")]
pub mod bigquery;
mod common;
#[cfg(feature = "databricks")]
pub mod databricks;
#[cfg(feature = "snowflake")]
pub mod snowflake;

#[cfg(feature = "bigquery")]
pub use bigquery::BigQueryConnector;
#[cfg(feature = "databricks")]
pub use databricks::DatabricksConnector;
#[cfg(feature = "snowflake")]
pub use snowflake::SnowflakeConnector;
