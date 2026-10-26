//! Exporting connections (no secrets) and importing them elsewhere.

use std::sync::Arc;

use databrain_app::api::{self, AppState};
use databrain_app::conn_io::{self, ImportSource};
use databrain_auth::{AuthMethod, MemoryStore, SecretRef, SecretStore};
use databrain_connector_core::{ConnectionConfig, ConnectorKind};
use databrain_query_engine::{EventSink, JobEvent};
use databrain_workspace::{ConnectionProfile, EnvTag, Folder, FolderKind, Workspace};

struct Quiet;
impl EventSink for Quiet {
    fn emit(&self, _: JobEvent) {}
}
struct NoUi;
impl databrain_app::ai_api::UiBridge for NoUi {
    fn emit(&self, _: &str, _: serde_json::Value) {}
    fn open_url(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
}

fn state() -> (AppState, Arc<MemoryStore>) {
    let secrets = Arc::new(MemoryStore::default());
    (AppState::new(Arc::new(Workspace::open_in_memory().unwrap()), secrets.clone(), Arc::new(Quiet), Arc::new(NoUi)), secrets)
}

#[test]
fn export_then_import_keeps_folders_and_drops_secrets() {
    let (a, _) = state();
    let prod = a.workspace.save_folder(Folder { id: String::new(), parent_id: None, name: "Prod".into(), kind: FolderKind::Connections }).unwrap();
    let eu = a.workspace.save_folder(Folder { id: String::new(), parent_id: Some(prod.id.clone()), name: "EU".into(), kind: FolderKind::Connections }).unwrap();
    let mut c = ConnectionConfig::new(ConnectorKind::Postgres, AuthMethod::Password { user: "app".into() });
    c.host = Some("pg.eu".into());
    c.database = Some("orders".into());
    c.options.insert("application_name".into(), "x".into());
    let profile = ConnectionProfile { id: String::new(), name: "Orders EU".into(), config: c, color: Some("#f00".into()), env: EnvTag::Prod, folder_id: Some(eu.id), has_secret: false, ai_policy: Default::default(), created_at: 0, updated_at: 0 };
    api::save_connection(&a, api::SaveConnectionArgs { profile, secret: Some("hunter2".into()), clear_secret: false, extra_secrets: Default::default() }).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("c.databrain-connections.json").to_string_lossy().into_owned();
    assert_eq!(conn_io::export_connections(&a, None, &file).unwrap(), 1);
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(!text.contains("hunter2"));

    // The same app sees it as already there.
    let p = conn_io::import_preview(&a, ImportSource::File, Some(file.clone())).unwrap();
    assert_eq!(p.candidates[0].existing.as_deref(), Some("Orders EU"));

    // Another app: folders are created, nothing secret comes along.
    let (b, secrets) = state();
    let p = conn_io::import_preview(&b, ImportSource::File, Some(file.clone())).unwrap();
    assert_eq!((p.candidates[0].existing.clone(), p.candidates[0].folder.clone()), (None, vec!["Prod".to_string(), "EU".to_string()]));
    let r = conn_io::import_apply(&b, ImportSource::File, Some(file), &[0]).unwrap();
    assert_eq!((r.imported, r.errors.len()), (1, 0));
    let got = b.workspace.list_connections().unwrap().into_iter().find(|c| c.name == "Orders EU").unwrap();
    assert_eq!((got.env, got.config.host.as_deref(), got.has_secret), (EnvTag::Prod, Some("pg.eu"), false));
    let folders = b.workspace.list_folders(FolderKind::Connections).unwrap();
    let leaf = folders.iter().find(|f| Some(&f.id) == got.folder_id.as_ref()).unwrap();
    assert_eq!(leaf.name, "EU");
    assert_eq!(folders.iter().find(|f| Some(&f.id) == leaf.parent_id.as_ref()).unwrap().name, "Prod");
    assert!(secrets.get(&SecretRef::for_connection(&got.id)).unwrap().is_none());
}

#[test]
fn pgpass_import_saves_the_password() {
    let (a, secrets) = state();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("pgpass");
    std::fs::write(&file, "db.local:5432:shop:ann:pw1\n").unwrap();
    let path = Some(file.to_string_lossy().into_owned());
    let p = conn_io::import_preview(&a, ImportSource::Pgpass, path.clone()).unwrap();
    assert!(p.candidates[0].has_password);
    conn_io::import_apply(&a, ImportSource::Pgpass, path, &[0]).unwrap();
    let c = a.workspace.list_connections().unwrap().into_iter().find(|c| c.name == "ann@db.local/shop").unwrap();
    assert!(c.has_secret);
    assert_eq!(secrets.get(&SecretRef::for_connection(&c.id)).unwrap().map(|s| secrecy::ExposeSecret::expose_secret(&s).to_string()).as_deref(), Some("pw1"));
}

/// A DBeaver connection without its user (kept encrypted elsewhere) and a
/// DataGrip data source are imported, to be completed in their form.
#[test]
fn imports_incomplete_dbeaver_and_datagrip_connections() {
    let (a, _) = state();
    let dir = tempfile::tempdir().unwrap();
    let dbeaver = dir.path().join("data-sources.json");
    std::fs::write(&dbeaver, r#"{"connections": {"pg-1": {"provider": "postgresql", "driver": "postgres-jdbc", "name": "No user", "configuration": {"host": "pg", "port": "5432", "database": "d"}}}}"#).unwrap();
    let path = Some(dbeaver.to_string_lossy().into_owned());
    let p = conn_io::import_preview(&a, ImportSource::Dbeaver, path.clone()).unwrap();
    assert!(p.candidates[0].note.as_deref().unwrap().contains("user and password"), "{:?}", p.candidates[0].note);
    let r = conn_io::import_apply(&a, ImportSource::Dbeaver, path, &[0]).unwrap();
    assert_eq!((r.imported, r.errors.clone()), (1, vec![]), "no 'User name is required'");

    let grip = dir.path().join("dataSources.xml");
    std::fs::write(&grip, r#"<project version="4"><component name="DataSourceManagerImpl" format="xml" multifile-model="true">
        <data-source source="LOCAL" name="mysql@db" uuid="x"><driver-ref>mysql.8</driver-ref><jdbc-url>jdbc:mysql://db:3306/app</jdbc-url></data-source>
      </component></project>"#).unwrap();
    std::fs::write(dir.path().join("dataSources.local.xml"), r#"<project version="4"><component name="dataSourceStorageLocal"><data-source name="mysql@db" uuid="x"><user-name>root</user-name></data-source></component></project>"#).unwrap();
    let path = Some(grip.to_string_lossy().into_owned());
    let p = conn_io::import_preview(&a, ImportSource::Datagrip, path.clone()).unwrap();
    assert_eq!((p.candidates[0].kind, p.candidates[0].user.as_deref(), p.candidates[0].target.as_str()), (ConnectorKind::Mysql, Some("root"), "db:3306/app"));
    let r = conn_io::import_apply(&a, ImportSource::Datagrip, path, &[0]).unwrap();
    assert_eq!(r.imported, 1);
    let got = a.workspace.list_connections().unwrap();
    assert!(got.iter().any(|c| c.name == "No user") && got.iter().any(|c| c.name == "mysql@db"));
}
