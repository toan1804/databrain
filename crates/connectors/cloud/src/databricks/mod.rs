//! Databricks SQL via the Statement Execution API 2.0.
//!
//! Auth: personal access token, OAuth U2M (browser + PKCE against the
//! workspace's `/oidc` endpoints with the `databricks-cli` public client),
//! OAuth M2M (service principal) and existing Databricks CLI profiles.
//! Names are three-level; schemas are exposed as `catalog.schema`.

mod client;
mod connector;
mod download;
mod metadata;
mod session;
mod stream;
#[cfg(test)]
mod testutil;
mod types;

pub use client::DbxSession;
pub use connector::DatabricksConnector;

/// Public OAuth client registered by Databricks for CLI/SDK tools.
pub const DEFAULT_CLIENT_ID: &str = "databricks-cli";
