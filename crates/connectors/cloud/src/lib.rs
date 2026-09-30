//! Cloud data warehouse connectors over HTTPS APIs: Snowflake, Databricks
//! SQL and Google BigQuery.

mod common;
#[cfg(feature = "bigquery")]
pub mod bigquery;
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
