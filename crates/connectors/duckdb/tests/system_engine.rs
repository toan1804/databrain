//! The system engine (builds without bundled DuckDB): run with a DuckDB
//! library, e.g.
//!   DATABRAIN_DUCKDB_LIB=/path/to/libduckdb.dylib \
//!     node scripts/build-system-duckdb.mjs --cargo test -p databrain-connector-duckdb --no-default-features --features system --test system_engine
#![cfg(feature = "system")]

use std::sync::Arc;

use databrain_auth::{AuthMethod, InlineCredentialSource};
use databrain_connector_core::{ConnectionConfig, Connector, ConnectorKind, ExecOptions};
use databrain_connector_duckdb::{DuckdbConnector, engine_status, load_engine};

#[allow(non_snake_case)]
fn NoCreds() -> InlineCredentialSource {
    InlineCredentialSource::new(AuthMethod::None, None)
}

#[tokio::test]
async fn missing_then_loaded_library() {
    let cfg = ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None);
    let c = DuckdbConnector::new();
    // Nothing loaded yet: a clear error, no crash.
    let st = load_engine(Some("/nonexistent/libduckdb.dylib"));
    if !st.available {
        let e = c.connect(&cfg, Arc::new(NoCreds())).await.err().expect("refused without an engine");
        assert!(e.to_string().contains("DuckDB engine not found"), "{e}");
    }
    let Ok(lib) = std::env::var("DATABRAIN_DUCKDB_LIB") else { return };
    let st = load_engine(Some(&lib));
    assert!(st.available, "{st:?}");
    assert!(st.version.as_deref().is_some_and(|v| v.starts_with("v1.")), "{st:?}");
    let s = c.connect(&cfg, Arc::new(NoCreds())).await.unwrap();
    let got = s.execute("select 40 + 2 as answer, 'ok' as t", ExecOptions::default()).await.unwrap().collect().await.unwrap();
    assert_eq!(got.batches.iter().map(|b| b.num_rows()).sum::<usize>(), 1);
    // Parquet/JSON come with the library (built in or autoloaded).
    let p = std::env::temp_dir().join("databrain-system-engine.parquet");
    let p = p.to_string_lossy().replace('\\', "/");
    s.execute(&format!("copy (select 1 as x) to '{p}' (format parquet)"), ExecOptions::default()).await.unwrap().collect().await.unwrap();
    let got = s.execute(&format!("select * from '{p}'"), ExecOptions::default()).await.unwrap().collect().await.unwrap();
    assert_eq!(got.batches.iter().map(|b| b.num_rows()).sum::<usize>(), 1);
    assert!(engine_status().path.is_some());
}
