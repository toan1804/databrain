use std::io::Cursor;
use std::sync::Arc;

use databrain_connector_core::external::{ExternalTablesSlot, OutputMasks, RESULTS_SCHEMA, referenced_results};
use databrain_connector_core::{ColumnInfo, ConnectorError, ConnectorKind, DbObject, ObjectKind, Result, TableColumns, quote_ident, quote_literal};

use crate::database::Loaded;
use crate::error::map_err;
use crate::session::DuckSession;

/// Explorer schema id for DataBrain outputs (`results.<name>` in SQL).
pub const RESULTS_SCHEMA_ID: &str = "results.main";

impl DuckSession {
    /// Outputs as explorer objects (no data loaded).
    pub(crate) fn output_catalog(&self) -> Vec<databrain_connector_core::external::ExternalInfo> {
        self.outputs.as_ref().and_then(|s| s.get()).map(|t| t.catalog()).unwrap_or_default()
    }

    pub(crate) fn output_objects(&self) -> Vec<DbObject> {
        self.output_catalog()
            .into_iter()
            .map(|i| DbObject { schema: RESULTS_SCHEMA_ID.into(), name: i.name, kind: ObjectKind::Table, comment: i.comment, row_estimate: i.rows })
            .collect()
    }

    pub(crate) fn output_columns(&self) -> Vec<TableColumns> {
        self.output_catalog()
            .into_iter()
            .map(|i| TableColumns {
                table: i.name,
                columns: i
                    .columns
                    .into_iter()
                    .map(|(name, data_type)| ColumnInfo { name, data_type, nullable: true, is_primary_key: false, default: None, comment: None })
                    .collect(),
                foreign_keys: vec![],
            })
            .collect()
    }
}

/// Load (or refresh) the outputs `sql` references into the in-memory
/// `results` catalog of this session. Returns notices to show the user.
///
/// With `masks` (the assistant's runs), each output is loaded with its
/// sensitive columns masked. A database file's instance is shared with the
/// user's sessions (`shared`), whose `results` tables a masked load would
/// replace, so masked outputs are refused there.
#[allow(clippy::too_many_arguments)]
pub(crate) fn load_outputs(
    conn: &duckdb::Connection,
    sql: &str,
    slot: Option<&ExternalTablesSlot>,
    loaded: &Loaded,
    masks: Option<&OutputMasks>,
    shared: bool,
    sandboxed: bool,
) -> Result<Vec<String>> {
    let names = referenced_results(sql);
    if names.is_empty() {
        return Ok(vec![]);
    }
    let Some(tables) = slot.and_then(|s| s.get()) else {
        return Err(ConnectorError::query("results.* refers to DataBrain outputs, which are not available in this session"));
    };
    let mut notices = Vec::new();
    for name in names {
        let t = match tables.resolve(&name) {
            Ok(t) => t,
            Err(e) => {
                let known = tables.names();
                let hint = if known.is_empty() {
                    "No outputs yet — run a query first.".to_string()
                } else {
                    format!("Available: {}", known.iter().take(15).map(|n| format!("results.{n}")).collect::<Vec<_>>().join(", "))
                };
                return Err(ConnectorError::query(format!("results.{name}: {e}. {hint}")));
            }
        };
        let t = match masks {
            None => t,
            Some(m) => {
                if shared && m.0.get(&t.version_key).is_some_and(|c| !c.is_empty()) {
                    return Err(ConnectorError::query(format!(
                        "results.{name} has sensitive columns, which can't be masked in a database file other sessions use; query it with query_outputs (the Results connection) instead"
                    )));
                }
                m.apply(t).map_err(ConnectorError::query)?
            }
        };
        if let Some(n) = &t.notice {
            notices.push(n.clone());
        }
        let key = name.to_ascii_lowercase();
        let current = loaded.lock().map_err(|_| ConnectorError::internal("lock poisoned"))?.get(&key).cloned();
        if current.as_deref() == Some(t.version_key.as_str()) {
            continue;
        }
        let attached: bool =
            conn.query_row("SELECT count(*) > 0 FROM duckdb_databases() WHERE database_name = 'results'", [], |r| r.get(0)).map_err(map_err)?;
        if !attached {
            conn.execute_batch(&format!("ATTACH ':memory:' AS {RESULTS_SCHEMA}")).map_err(map_err)?;
        }
        let table = quote_ident(ConnectorKind::Duckdb, &name);
        // Rows on disk: a view over the files where the session may read
        // them and nothing is masked; otherwise streamed in (masked).
        if !t.files.is_empty() {
            let drop = drop_existing(conn, &name)?;
            if !sandboxed && !t.hide.iter().any(|h| *h) {
                let list = t.files.iter().map(|f| quote_literal(&f.to_string_lossy())).collect::<Vec<_>>().join(", ");
                // The types an output in memory has: exact decimals (text in
                // the files) as DECIMAL, types the appender can't take as text.
                let normal = normalize_schema(&t.schema);
                let cols = normal
                    .fields()
                    .iter()
                    .zip(t.schema.fields())
                    .map(|(f, orig)| {
                        let c = quote_ident(ConnectorKind::Duckdb, f.name());
                        if matches!(f.data_type(), Dt::Decimal128(..) | Dt::Float64) || f.data_type() != orig.data_type() {
                            format!("CAST({c} AS {}) AS {c}", duck_type(f.data_type()))
                        } else {
                            c
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                conn.execute_batch(&format!("{drop} CREATE VIEW {RESULTS_SCHEMA}.main.{table} AS SELECT {cols} FROM read_parquet([{list}])"))
                    .map_err(map_err)?;
            } else {
                let schema = normalize_schema(&t.schema);
                let cols = schema
                    .fields()
                    .iter()
                    .map(|f| format!("{} {}", quote_ident(ConnectorKind::Duckdb, f.name()), duck_type(f.data_type())))
                    .collect::<Vec<_>>()
                    .join(", ");
                conn.execute_batch(&format!("{drop} CREATE TABLE {RESULTS_SCHEMA}.main.{table} ({cols})")).map_err(map_err)?;
                let mut app = conn.appender_to_catalog_and_db(&name, RESULTS_SCHEMA, "main").map_err(map_err)?;
                tables
                    .for_each_batch(&name, &mut |b| {
                        let b = databrain_connector_core::external::mask_batch(&b, &t.hide)?;
                        let b = normalize_for_duckdb(&b).map_err(|e| e.to_string())?;
                        if b.num_rows() > 0 {
                            app.append_record_batch(to_arrow58(&b).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
                        }
                        Ok(())
                    })
                    .map_err(ConnectorError::query)?;
                app.flush().map_err(map_err)?;
            }
            loaded.lock().map_err(|_| ConnectorError::internal("lock poisoned"))?.insert(key, t.version_key);
            continue;
        }
        let batches: Vec<_> = t.batches.iter().map(normalize_for_duckdb).collect::<Result<_>>()?;
        let schema = batches.first().map(|b| b.schema()).unwrap_or_else(|| normalize_schema(&t.schema));
        let cols = schema
            .fields()
            .iter()
            .map(|f| format!("{} {}", quote_ident(ConnectorKind::Duckdb, f.name()), duck_type(f.data_type())))
            .collect::<Vec<_>>()
            .join(", ");
        let drop = drop_existing(conn, &name)?;
        conn.execute_batch(&format!("{drop} CREATE TABLE {RESULTS_SCHEMA}.main.{table} ({cols})")).map_err(map_err)?;
        {
            let mut app = conn.appender_to_catalog_and_db(&name, RESULTS_SCHEMA, "main").map_err(map_err)?;
            for b in &batches {
                if b.num_rows() > 0 {
                    app.append_record_batch(to_arrow58(b)?).map_err(map_err)?;
                }
            }
            app.flush().map_err(map_err)?;
        }
        loaded.lock().map_err(|_| ConnectorError::internal("lock poisoned"))?.insert(key, t.version_key);
    }
    Ok(notices)
}

/// SQL that drops what `results.<name>` is now (a table, or a view over an
/// output's files): DuckDB refuses `DROP VIEW` of a table, even `IF EXISTS`.
fn drop_existing(conn: &duckdb::Connection, name: &str) -> Result<String> {
    let kind: Option<String> = conn
        .query_row(
            "SELECT CASE WHEN EXISTS (SELECT 1 FROM duckdb_views() WHERE database_name = 'results' AND lower(view_name) = lower(?1)) THEN 'VIEW' \
             WHEN EXISTS (SELECT 1 FROM duckdb_tables() WHERE database_name = 'results' AND lower(table_name) = lower(?1)) THEN 'TABLE' END",
            [name],
            |r| r.get(0),
        )
        .map_err(map_err)?;
    Ok(kind.map_or_else(String::new, |k| format!("DROP {k} {RESULTS_SCHEMA}.main.{};", quote_ident(ConnectorKind::Duckdb, name))))
}

use databrain_connector_core::arrow::array::RecordBatch as Batch59;
use databrain_connector_core::arrow::datatypes::{DataType as Dt, Field as Field59, Schema as Schema59, SchemaRef as SchemaRef59, TimeUnit};

/// Column type DuckDB's appender handles natively, or `None` → cast to text.
pub(crate) fn native(dt: &Dt) -> Option<Dt> {
    Some(match dt {
        Dt::Boolean
        | Dt::Int8
        | Dt::Int16
        | Dt::Int32
        | Dt::Int64
        | Dt::UInt8
        | Dt::UInt16
        | Dt::UInt32
        | Dt::UInt64
        | Dt::Float32
        | Dt::Float64
        | Dt::Utf8
        | Dt::Binary
        | Dt::Date32 => dt.clone(),
        Dt::Float16 => Dt::Float32,
        Dt::LargeUtf8 | Dt::Utf8View => Dt::Utf8,
        Dt::LargeBinary | Dt::BinaryView | Dt::FixedSizeBinary(_) => Dt::Binary,
        Dt::Date64 => Dt::Date32,
        Dt::Timestamp(u, None) => Dt::Timestamp(*u, None),
        Dt::Timestamp(_, Some(tz)) => Dt::Timestamp(TimeUnit::Microsecond, Some(tz.clone())),
        Dt::Time32(_) | Dt::Time64(_) => Dt::Time64(TimeUnit::Microsecond),
        Dt::Decimal128(p, _) if *p <= 38 => dt.clone(),
        Dt::Decimal32(p, s) | Dt::Decimal64(p, s) => Dt::Decimal128(*p, *s),
        Dt::Decimal256(..) => Dt::Float64,
        _ => return None,
    })
}

pub(crate) fn normalize_schema(s: &SchemaRef59) -> SchemaRef59 {
    Arc::new(Schema59::new(s.fields().iter().map(|f| Field59::new(f.name(), native(f.data_type()).unwrap_or(Dt::Utf8), true)).collect::<Vec<_>>()))
}

pub(crate) fn normalize_for_duckdb(b: &Batch59) -> Result<Batch59> {
    use databrain_connector_core::arrow::compute::cast;
    let schema = normalize_schema(&b.schema());
    let cols = b
        .columns()
        .iter()
        .zip(schema.fields())
        .map(|(c, f)| if c.data_type() == f.data_type() { Ok(c.clone()) } else { cast(c, f.data_type()) })
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| ConnectorError::internal(e.to_string()))?;
    Batch59::try_new(schema, cols).map_err(|e| ConnectorError::internal(e.to_string()))
}

pub(crate) fn duck_type(dt: &Dt) -> String {
    match dt {
        Dt::Boolean => "BOOLEAN".into(),
        Dt::Int8 => "TINYINT".into(),
        Dt::Int16 => "SMALLINT".into(),
        Dt::Int32 => "INTEGER".into(),
        Dt::Int64 => "BIGINT".into(),
        Dt::UInt8 => "UTINYINT".into(),
        Dt::UInt16 => "USMALLINT".into(),
        Dt::UInt32 => "UINTEGER".into(),
        Dt::UInt64 => "UBIGINT".into(),
        Dt::Float32 => "FLOAT".into(),
        Dt::Float64 => "DOUBLE".into(),
        Dt::Binary => "BLOB".into(),
        Dt::Date32 => "DATE".into(),
        Dt::Timestamp(TimeUnit::Second, None) => "TIMESTAMP_S".into(),
        Dt::Timestamp(TimeUnit::Millisecond, None) => "TIMESTAMP_MS".into(),
        Dt::Timestamp(TimeUnit::Nanosecond, None) => "TIMESTAMP_NS".into(),
        Dt::Timestamp(_, None) => "TIMESTAMP".into(),
        Dt::Timestamp(_, Some(_)) => "TIMESTAMPTZ".into(),
        Dt::Time64(_) => "TIME".into(),
        Dt::Decimal128(p, s) => format!("DECIMAL({p},{s})"),
        _ => "VARCHAR".into(),
    }
}

/// Workspace Arrow (59) → DuckDB's Arrow (58), via IPC.
pub(crate) fn to_arrow58(b: &Batch59) -> Result<arrow58::record_batch::RecordBatch> {
    let mut buf = Vec::new();
    {
        let mut w =
            databrain_connector_core::arrow::ipc::writer::StreamWriter::try_new(&mut buf, &b.schema()).map_err(|e| ConnectorError::internal(e.to_string()))?;
        w.write(b).map_err(|e| ConnectorError::internal(e.to_string()))?;
        w.finish().map_err(|e| ConnectorError::internal(e.to_string()))?;
    }
    let mut r = arrow58::ipc::reader::StreamReader::try_new(Cursor::new(buf), None).map_err(|e| ConnectorError::internal(e.to_string()))?;
    r.next().ok_or_else(|| ConnectorError::internal("empty IPC stream"))?.map_err(|e| ConnectorError::internal(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DuckdbConnector;
    use crate::test_util::*;
    use databrain_auth::{AuthMethod, InlineCredentialSource};
    use databrain_connector_core::arrow::array::AsArray;
    use databrain_connector_core::arrow::datatypes::Int64Type;
    use databrain_connector_core::external::ExternalTablesSlot;
    use databrain_connector_core::{ConnectionConfig, Connector, ConnectorKind, ExecOptions};
    use std::sync::Arc;

    struct FakeOutputs(std::sync::Mutex<(String, i64)>);
    impl databrain_connector_core::external::ExternalTables for FakeOutputs {
        fn resolve(&self, name: &str) -> std::result::Result<databrain_connector_core::external::ExternalTable, String> {
            use databrain_connector_core::arrow::array::{Int64Array, StringArray};
            use databrain_connector_core::arrow::datatypes::{DataType, Field, Schema};
            if name != "sales" {
                return Err("unknown output".into());
            }
            let (version, n) = self.0.lock().unwrap().clone();
            let schema = Arc::new(Schema::new(vec![Field::new("region", DataType::Utf8, true), Field::new("amount", DataType::Int64, true)]));
            let regions: Vec<Option<&str>> = (0..n).map(|i| Some(if i % 2 == 0 { "EU" } else { "US" })).collect();
            let batch = databrain_connector_core::arrow::array::RecordBatch::try_new(
                schema.clone(),
                vec![Arc::new(StringArray::from(regions)), Arc::new(Int64Array::from((0..n).collect::<Vec<_>>()))],
            )
            .unwrap();
            Ok(databrain_connector_core::external::ExternalTable {
                name: name.into(),
                version_key: version,
                schema,
                batches: vec![batch],
                notice: Some("capped".into()),
                files: vec![],
                hide: vec![],
            })
        }
        fn names(&self) -> Vec<String> {
            vec!["sales".into()]
        }
        fn catalog(&self) -> Vec<databrain_connector_core::external::ExternalInfo> {
            vec![databrain_connector_core::external::ExternalInfo {
                name: "sales".into(),
                columns: vec![("region".into(), "VARCHAR".into()), ("amount".into(), "BIGINT".into())],
                rows: Some(4),
                comment: Some("output r1".into()),
            }]
        }
    }

    /// The assistant's runs load outputs with their sensitive columns masked,
    /// refuse outputs that weren't checked, and refuse masking in a database
    /// file shared with other sessions.
    #[tokio::test]
    async fn masks_output_columns_for_the_assistant() {
        use databrain_connector_core::external::OutputMasks;
        let slot = ExternalTablesSlot::new();
        slot.set(Arc::new(FakeOutputs(std::sync::Mutex::new(("v1".into(), 4)))));
        let conn = DuckdbConnector::with_outputs(slot);
        let creds = || Arc::new(InlineCredentialSource::new(AuthMethod::None, None));
        let opts = |m: OutputMasks| ExecOptions { output_masks: Some(Arc::new(m)), ..Default::default() };
        let masked = || OutputMasks([("v1".to_string(), vec!["region".to_string()])].into());
        let mem = conn.connect(&ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None), creds()).await.unwrap();
        // Renamed through a CTE column list: still the mask.
        let sql = "with c(a, b) as (select * from results.sales) select string_agg(distinct a, ',') from c";
        let r = mem.execute(databrain_connector_core::Statement::internal(sql), opts(masked())).await.unwrap().collect().await.unwrap();
        assert_eq!(r.batches[0].column(0).as_string::<i32>().value(0), "***");
        // The same session without masks (the user's run) gets the values back.
        let r = q(mem.as_ref(), "select string_agg(distinct region, ',' order by region) from results.sales").await;
        assert_eq!(r.batches[0].column(0).as_string::<i32>().value(0), "EU,US");
        // An output the caller didn't check is refused.
        let e = mem
            .execute(databrain_connector_core::Statement::internal("select * from results.sales"), opts(OutputMasks::default()))
            .await
            .unwrap()
            .collect()
            .await
            .unwrap_err();
        assert!(e.message.contains("not checked"), "{}", e.message);
        // A database file is shared with other sessions: no masked loads there.
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None);
        cfg.file_path = Some(dir.path().join("m.duckdb").to_string_lossy().into_owned());
        let file = conn.connect(&cfg, creds()).await.unwrap();
        let e = file
            .execute(databrain_connector_core::Statement::internal("select * from results.sales"), opts(masked()))
            .await
            .unwrap()
            .collect()
            .await
            .unwrap_err();
        assert!(e.message.contains("sensitive columns"), "{}", e.message);
        // Nothing to mask: fine there too.
        let none = OutputMasks([("v1".to_string(), vec![])].into());
        assert_eq!(
            file.execute(databrain_connector_core::Statement::internal("select * from results.sales"), opts(none))
                .await
                .unwrap()
                .collect()
                .await
                .unwrap()
                .batches[0]
                .num_rows(),
            4
        );
    }

    /// An output on disk: its file holds 5 rows, the stream gives 3, so a
    /// count tells which way a session read it.
    struct DiskOutput(std::path::PathBuf);
    impl databrain_connector_core::external::ExternalTables for DiskOutput {
        fn resolve(&self, name: &str) -> std::result::Result<databrain_connector_core::external::ExternalTable, String> {
            use databrain_connector_core::arrow::datatypes::{DataType, Field, Schema};
            let schema = Arc::new(Schema::new(vec![Field::new("n", DataType::Int64, true)]));
            Ok(databrain_connector_core::external::ExternalTable {
                name: name.into(),
                version_key: "disk".into(),
                schema,
                batches: vec![],
                notice: None,
                files: vec![self.0.clone()],
                hide: vec![],
            })
        }
        fn names(&self) -> Vec<String> {
            vec!["big".into()]
        }
        fn for_each_batch(
            &self,
            _name: &str,
            f: &mut dyn FnMut(databrain_connector_core::arrow::array::RecordBatch) -> std::result::Result<(), String>,
        ) -> std::result::Result<(), String> {
            use databrain_connector_core::arrow::array::Int64Array;
            use databrain_connector_core::arrow::datatypes::{DataType, Field, Schema};
            let schema = Arc::new(Schema::new(vec![Field::new("n", DataType::Int64, true)]));
            f(databrain_connector_core::arrow::array::RecordBatch::try_new(schema, vec![Arc::new(Int64Array::from(vec![1, 2, 3]))]).unwrap())
        }
    }

    async fn count(s: &dyn databrain_connector_core::Session, sql: &str) -> databrain_connector_core::Result<i64> {
        s.execute(databrain_connector_core::Statement::internal(sql), ExecOptions::default())
            .await?
            .collect()
            .await
            .map(|r| r.batches[0].column(0).as_primitive::<Int64Type>().value(0))
    }

    /// A sandboxed (AI) session gets an output on disk streamed in and can't
    /// read the result's files itself; a plain session reads the files.
    #[tokio::test]
    async fn outputs_on_disk_reach_sandboxed_sessions_only_as_rows() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("000000.parquet");
        let path = file.to_string_lossy().into_owned();
        session(ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None))
            .await
            .execute(
                databrain_connector_core::Statement::internal(&format!("COPY (SELECT range AS n FROM range(5)) TO {} (FORMAT parquet)", quote_literal(&path))),
                ExecOptions::default(),
            )
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        let open = |sandbox: bool| {
            let slot = ExternalTablesSlot::new();
            slot.set(Arc::new(DiskOutput(file.clone())));
            let mut cfg = ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None);
            if sandbox {
                cfg.options.insert(databrain_connector_core::SANDBOX_OPTION.into(), "1".into());
            }
            async move { DuckdbConnector::with_outputs(slot).connect(&cfg, Arc::new(InlineCredentialSource::new(AuthMethod::None, None))).await.unwrap() }
        };
        let plain = open(false).await;
        assert_eq!(count(plain.as_ref(), "select count(*) from results.big").await.unwrap(), 5, "the files");
        let sandboxed = open(true).await;
        assert_eq!(count(sandboxed.as_ref(), "select count(*) from results.big").await.unwrap(), 3, "streamed in");
        let direct = count(sandboxed.as_ref(), &format!("select count(*) from read_parquet({})", quote_literal(&path))).await;
        assert!(direct.is_err(), "a sandboxed session can't read the result's files");
    }

    #[tokio::test]
    async fn queries_outputs_as_results_tables() {
        let slot = ExternalTablesSlot::new();
        let fake = Arc::new(FakeOutputs(std::sync::Mutex::new(("v1".into(), 4))));
        slot.set(fake.clone());
        let s = DuckdbConnector::with_outputs(slot)
            .connect(&ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None), Arc::new(InlineCredentialSource::new(AuthMethod::None, None)))
            .await
            .unwrap();
        let r = q(s.as_ref(), "select region, sum(amount) from results.sales group by 1 order by 1").await;
        assert_eq!(r.num_rows(), 2);
        assert_eq!(r.notices, vec!["capped".to_string()]);
        // New version is reloaded; joins between outputs work.
        *fake.0.lock().unwrap() = ("v2".into(), 10);
        let r = q(s.as_ref(), "select count(*) from results.sales a join results.sales b using (amount)").await;
        assert_eq!(r.batches[0].column(0).as_primitive::<Int64Type>().value(0), 10);
        let e = s
            .execute(databrain_connector_core::Statement::internal("select * from results.nope"), ExecOptions::default())
            .await
            .unwrap()
            .collect()
            .await
            .unwrap_err();
        assert!(e.message.contains("results.nope") && e.message.contains("results.sales"), "{}", e.message);
        // Explorer: outputs are listed (with columns) under results.main,
        // also after `results` was attached by the queries above.
        let schemas = s.list_schemas().await.unwrap();
        assert_eq!(schemas.iter().filter(|x| x.catalog.as_deref() == Some("results")).map(|x| x.name.as_str()).collect::<Vec<_>>(), vec![RESULTS_SCHEMA_ID]);
        let objs = s.list_objects(RESULTS_SCHEMA_ID).await.unwrap();
        assert_eq!(objs.iter().map(|o| o.name.as_str()).collect::<Vec<_>>(), vec!["sales"]);
        let d = s.describe(RESULTS_SCHEMA_ID, "sales").await.unwrap();
        assert_eq!(d.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["region", "amount"]);
        assert_eq!(s.search_objects("sal", 5).await.unwrap()[0].schema, RESULTS_SCHEMA_ID);
        assert_eq!(s.schema_object_counts().await.unwrap().unwrap().get(RESULTS_SCHEMA_ID), Some(&1));
        // Without a provider, results.* is a clear error.
        let plain = session(ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None)).await;
        let e = plain
            .execute(databrain_connector_core::Statement::internal("select * from results.sales"), ExecOptions::default())
            .await
            .unwrap()
            .collect()
            .await
            .unwrap_err();
        assert!(e.message.contains("not available"), "{}", e.message);
    }
}
