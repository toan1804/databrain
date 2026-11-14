//! Catalog reads for the explorer, completion and the AI index.

use super::*;

/// Where an unqualified table name resolves on a connection
/// ([`QueryEngine::default_namespace`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultNamespace {
    /// Outermost first (database or catalog, then schema); `None` where the
    /// server has none set.
    pub levels: Vec<Option<String>>,
    /// The innermost level is only the first place searched: a name not
    /// found there resolves elsewhere (a Postgres `search_path` of several
    /// schemas, SQL Server falling back from the user's schema to `dbo`).
    pub ambiguous: bool,
}
impl QueryEngine {
    /// Connect (if needed) and return the server version.
    pub async fn connect(&self, connection_id: &str) -> Result<String> {
        self.with_meta(connection_id, |s| async move { s.server_version().await }).await
    }

    pub async fn list_schemas(&self, connection_id: &str) -> Result<Vec<SchemaInfo>> {
        self.with_meta(connection_id, |s| async move { s.list_schemas().await }).await
    }

    pub async fn list_objects(&self, connection_id: &str, schema: &str) -> Result<Vec<DbObject>> {
        let schema = schema.to_string();
        self.with_meta(connection_id, |s| {
            let schema = schema.clone();
            async move { s.list_objects(&schema).await }
        })
        .await
    }

    /// Catalogs of three-level engines (`None` for two-level ones).
    pub async fn list_catalogs(&self, connection_id: &str) -> Result<Option<Vec<databrain_connector_core::CatalogInfo>>> {
        self.with_meta(connection_id, |s| async move { s.list_catalogs().await }).await
    }

    pub async fn list_catalog_schemas(&self, connection_id: &str, catalog: &str) -> Result<Vec<SchemaInfo>> {
        let catalog = catalog.to_string();
        self.with_meta(connection_id, |s| {
            let catalog = catalog.clone();
            async move { s.list_catalog_schemas(&catalog).await }
        })
        .await
    }

    /// [`Session::check_sql`] on the metadata session, at most 10 s. Only
    /// a query that reads: the check sends `EXPLAIN <sql>`, and what the
    /// classifier can't call a read could run (a second statement, a
    /// statement `EXPLAIN` executes).
    pub async fn check_sql(&self, connection_id: &str, sql: &str) -> Result<bool> {
        let sql = databrain_connector_core::Checked::new(sql, self.workspace.get_connection(connection_id)?.config.kind);
        if !sql.kind().is_read() {
            return Err(EngineError::new("invalid", format!("Only queries that read are checked with the database ({} is not)", sql.classification().keyword)));
        }
        match tokio::time::timeout(
            Duration::from_secs(10),
            self.with_meta(connection_id, |s| {
                let sql = sql.clone();
                async move { s.check_sql(&sql).await }
            }),
        )
        .await
        {
            Ok(r) => r,
            Err(_) => Err(EngineError::new("timeout", "The database took more than 10 s to check the query")),
        }
    }

    /// Where an unqualified table name resolves on this connection right
    /// now, outermost first (database or catalog, then schema, as far as
    /// the engine has them): asked of the server, since a role's
    /// `search_path` or default schema may not be the engine's usual one.
    /// `None` for engines not asked (their default is fixed or unknown).
    /// At most 10 s.
    pub async fn default_namespace(&self, connection_id: &str) -> Result<Option<DefaultNamespace>> {
        use databrain_connector_core::ConnectorKind as K;
        let kind = self.workspace.get_connection(connection_id)?.config.kind;
        let sql = match kind {
            // Postgres looks through every schema of the search_path that exists.
            K::Postgres => "select current_database(), current_schema(), cardinality(current_schemas(false))::text",
            K::Snowflake => "select current_database(), current_schema()",
            K::Mssql => "select db_name(), schema_name()",
            K::Databricks => "select current_catalog(), current_schema()",
            K::Mysql => "select database()",
            K::Clickhouse => "select currentDatabase()",
            K::Oracle => "select sys_context('userenv', 'current_schema') from dual",
            _ => return Ok(None),
        };
        let read = self.with_meta(connection_id, |s| async move {
            databrain_connector_core::query_table(s.as_ref(), databrain_connector_core::Statement::internal(sql)).await
        });
        let row = match tokio::time::timeout(Duration::from_secs(10), read).await {
            Ok(r) => r?.1.into_iter().next(),
            Err(_) => return Err(EngineError::new("timeout", "The database took more than 10 s to say its current schema")),
        };
        let Some(row) = row else { return Ok(None) };
        let mut levels: Vec<Option<String>> = row.into_iter().map(|v| v.filter(|v| !v.trim().is_empty())).collect();
        let ambiguous = match kind {
            // More than one schema searched: a name missing from the first
            // is found in a later one (none: only pg_catalog is).
            K::Postgres => levels.pop().flatten().and_then(|n| n.trim().parse::<u32>().ok()) != Some(1),
            // A default schema other than `dbo` falls back to `dbo`.
            K::Mssql => levels.get(1).cloned().flatten().is_some_and(|s| !s.eq_ignore_ascii_case("dbo")),
            _ => false,
        };
        Ok(Some(DefaultNamespace { levels, ambiguous }))
    }

    pub async fn explorer_fingerprints(&self, connection_id: &str) -> Result<Option<HashMap<String, String>>> {
        self.with_meta(connection_id, |s| async move { s.explorer_fingerprints().await }).await
    }

    /// [`Self::explorer_fingerprints`] on the indexing session, so an index
    /// run's catalog query doesn't hold up the explorer.
    pub async fn index_fingerprints(&self, connection_id: &str) -> Result<Option<HashMap<String, String>>> {
        self.with_tab(connection_id, &self.index_tab(connection_id), |s| async move { s.explorer_fingerprints().await }).await
    }

    pub async fn object_versions(&self, connection_id: &str, schema: &str) -> Result<Option<HashMap<String, String>>> {
        let schema = schema.to_string();
        self.with_meta(connection_id, |s| {
            let schema = schema.clone();
            async move { s.object_versions(&schema).await }
        })
        .await
    }

    pub async fn list_objects_named(&self, connection_id: &str, schema: &str, names: &[String]) -> Result<Vec<DbObject>> {
        let (schema, names) = (schema.to_string(), names.to_vec());
        self.with_meta(connection_id, |s| {
            let (schema, names) = (schema.clone(), names.clone());
            async move { s.list_objects_named(&schema, &names).await }
        })
        .await
    }

    /// Find tables/views by name across the connection's schemas.
    pub async fn search_objects(&self, connection_id: &str, query: &str, limit: usize) -> Result<Vec<DbObject>> {
        let query = query.to_string();
        let limit = limit.clamp(1, 500);
        self.with_meta(connection_id, |s| {
            let query = query.clone();
            async move { s.search_objects(&query, limit).await }
        })
        .await
    }

    /// Functions/procedures/packages matching `query` (editor completion).
    pub async fn search_routines(&self, connection_id: &str, schema: Option<&str>, query: &str, limit: usize) -> Result<Vec<DbObject>> {
        let (schema, query) = (schema.map(str::to_string), query.to_string());
        let limit = limit.clamp(1, 1000);
        self.with_meta(connection_id, |s| {
            let (schema, query) = (schema.clone(), query.clone());
            async move { s.search_routines(schema.as_deref(), &query, limit).await }
        })
        .await
    }

    /// Full DDL of an object (`None`: not available for this engine/kind).
    pub async fn object_ddl(&self, connection_id: &str, schema: &str, name: &str, kind: databrain_connector_core::ObjectKind) -> Result<Option<String>> {
        let (schema, name) = (schema.to_string(), name.to_string());
        self.with_meta(connection_id, |s| {
            let (schema, name) = (schema.clone(), name.clone());
            async move { s.object_ddl(&schema, &name, kind).await }
        })
        .await
    }

    /// Functions/procedures of a package (Oracle).
    pub async fn package_members(&self, connection_id: &str, schema: &str, package: &str) -> Result<Vec<DbObject>> {
        let (schema, package) = (schema.to_string(), package.to_string());
        self.with_meta(connection_id, |s| {
            let (schema, package) = (schema.clone(), package.clone());
            async move { s.package_members(&schema, &package).await }
        })
        .await
    }

    /// Tables/views of one schema matching `query` (editor completion).
    pub async fn search_schema(&self, connection_id: &str, schema: &str, query: &str, limit: usize) -> Result<Vec<DbObject>> {
        let (schema, query) = (schema.to_string(), query.to_string());
        let limit = limit.clamp(1, 1000);
        self.with_meta(connection_id, |s| {
            let (schema, query) = (schema.clone(), query.clone());
            async move { s.search_schema(&schema, &query, limit).await }
        })
        .await
    }

    pub async fn bulk_metadata(&self, connection_id: &str, schemas: &[String]) -> Result<Vec<databrain_connector_core::SchemaMetadata>> {
        let schemas = schemas.to_vec();
        self.with_tab(connection_id, &self.index_tab(connection_id), |s| {
            let schemas = schemas.clone();
            async move { s.bulk_metadata(&schemas).await }
        })
        .await
    }

    pub async fn schema_object_counts(&self, connection_id: &str) -> Result<Option<std::collections::HashMap<String, usize>>> {
        self.with_meta(connection_id, |s| async move { s.schema_object_counts().await }).await
    }

    /// Columns of every table of a schema, and whether the read covered
    /// foreign keys ([`Session::schema_columns_read_keys`]): then a table
    /// without keys has none.
    pub async fn schema_columns(&self, connection_id: &str, schema: &str) -> Result<(Vec<databrain_connector_core::TableColumns>, bool)> {
        let schema = schema.to_string();
        self.with_meta(connection_id, |s| {
            let schema = schema.clone();
            async move { Ok((s.schema_columns(&schema).await?, s.schema_columns_read_keys())) }
        })
        .await
    }

    pub async fn describe(&self, connection_id: &str, schema: &str, name: &str) -> Result<ObjectDetail> {
        let (schema, name) = (schema.to_string(), name.to_string());
        self.with_meta(connection_id, |s| {
            let (schema, name) = (schema.clone(), name.clone());
            async move { s.describe(&schema, &name).await }
        })
        .await
    }

    /// Indexes, partitioning and clustering of a table (editor query hints).
    pub async fn table_layout(&self, connection_id: &str, schema: &str, name: &str) -> Result<TableLayout> {
        let (schema, name) = (schema.to_string(), name.to_string());
        self.with_meta(connection_id, |s| {
            let (schema, name) = (schema.clone(), name.clone());
            async move { s.table_layout(&schema, &name).await }
        })
        .await
    }
}
