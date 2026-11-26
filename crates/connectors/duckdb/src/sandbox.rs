use databrain_connector_core::external::RESULTS_SCHEMA;
use databrain_connector_core::{Result, quote_literal};

use crate::error::map_err;
use crate::files::detect_format;

/// Lock a database instance down for SQL the user didn't write: no file or
/// network access (`read_csv`, `COPY … TO`, `ATTACH`, `httpfs`, extension
/// loading) and no setting can be changed back. `results` (DataBrain's
/// outputs, in memory) is attached first, as attaching needs access.
///
/// The attached `files` stay readable: their views are bound again on
/// every query, so each file (and each Parquet / Delta / Iceberg folder)
/// is allowed by name, and nothing else on disk.
pub(crate) fn seal(conn: &duckdb::Connection, files: &[String]) -> Result<()> {
    let attached: bool = conn.query_row("SELECT count(*) > 0 FROM duckdb_databases() WHERE database_name = 'results'", [], |r| r.get(0)).map_err(map_err)?;
    if !attached {
        conn.execute_batch(&format!("ATTACH ':memory:' AS {RESULTS_SCHEMA} (READ_WRITE)")).map_err(map_err)?;
    }
    let (dirs, paths) = sandbox_allowed(files);
    let list = |v: &[String]| v.iter().map(|p| quote_literal(p)).collect::<Vec<_>>().join(", ");
    if !dirs.is_empty() {
        conn.execute_batch(&format!("SET allowed_directories = [{}]", list(&dirs))).map_err(map_err)?;
    }
    if !paths.is_empty() {
        conn.execute_batch(&format!("SET allowed_paths = [{}]", list(&paths))).map_err(map_err)?;
    }
    // A spill folder of its own: DuckDB lets a sealed instance read and
    // write its temp_directory, which would otherwise be another one's (or
    // DuckDB's default, `.tmp` in the current folder). Not sealed without it.
    if let Some(dir) = crate::engine::sandbox_spill_dir() {
        conn.execute_batch(&format!("SET temp_directory = {}", quote_literal(&dir))).map_err(map_err)?;
        let set: String = conn.query_row("SELECT current_setting('temp_directory')", [], |r| r.get(0)).map_err(map_err)?;
        let norm = |p: &str| p.replace('\\', "/").trim_end_matches('/').to_string();
        if norm(&set) != norm(&dir) {
            return Err(databrain_connector_core::ConnectorError::internal(format!("the sandbox's spill folder could not be set (DuckDB uses {set})")));
        }
    }
    conn.execute_batch("SET enable_external_access = false; SET lock_configuration = true;").map_err(map_err)
}

/// Statements a sealed session doesn't run at all. The attached files and
/// folders are allowed by name so that they can be read; DuckDB then lets
/// them be written too (`COPY … TO` a new file in an attached folder, or
/// over an attached file with `USE_TMP_FILE false`), and `ATTACH` would
/// create a database file there. Checked on the words of the whole text,
/// strings and comments included: a query that merely mentions them is
/// refused as well, which errs on the safe side.
pub(crate) fn refuse_file_writes(sql: &str) -> Result<()> {
    let lower = sql.to_ascii_lowercase();
    let has = |w: &str| lower.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).any(|x| x == w);
    let what = if has("copy") && has("to") {
        "COPY … TO"
    } else if has("export") && has("database") {
        "EXPORT DATABASE"
    } else if has("attach") {
        "ATTACH"
    } else {
        return Ok(());
    };
    Err(databrain_connector_core::ConnectorError::query(format!(
        "{what} is not run here: SQL from the assistant runs in a DuckDB sandbox that reads the attached files and never writes files. Use a job's export step, or run it yourself."
    )))
}

/// For a sealed instance on a database file other chats share: no view or
/// macro is kept in the file (nor anywhere but this session's `temp`) that
/// could read DataBrain's outputs. `results.<name>` holds what was loaded
/// last, by any chat, and only a statement that names `results.<name>` is
/// checked: a kept view over it would let another chat read an output it
/// was never given. Read on the words of the whole text, strings and
/// comments included (`query('select * from results.x')` counts), and
/// `query` / `query_table` (which can build the name) count as well: a
/// statement that merely mentions them is refused too, which errs on the
/// safe side. `CREATE TEMP VIEW` stays: only this session sees it.
pub(crate) fn refuse_shared_views(sql: &str) -> Result<()> {
    let code: Vec<String> = databrain_connector_core::sql::code_text(sql, databrain_connector_core::ConnectorKind::Duckdb)
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .map(str::to_ascii_uppercase)
        .collect();
    // CREATE [OR REPLACE] [TEMP | TEMPORARY] {VIEW | MACRO | FUNCTION}, or ALTER VIEW.
    let mut w = code.iter().map(String::as_str);
    let kept = match w.next() {
        Some("CREATE") => {
            let mut rest: Vec<&str> = w.take(4).collect();
            if rest.first() == Some(&"OR") {
                rest.drain(..rest.len().min(2));
            }
            match rest.first() {
                Some(&("TEMP" | "TEMPORARY")) => false,
                Some(&("VIEW" | "MACRO" | "FUNCTION")) => true,
                _ => false,
            }
        }
        Some("ALTER") => w.next() == Some("VIEW"),
        _ => false,
    };
    if !kept {
        return Ok(());
    }
    let lower = sql.to_ascii_lowercase();
    let words: Vec<&str> = lower.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).collect();
    if !words.iter().any(|w| matches!(*w, "results" | "query" | "query_table")) {
        return Ok(());
    }
    Err(databrain_connector_core::ConnectorError::query(
        "A view or macro over DataBrain outputs (results.<name>) is not kept in a database file other chats use: they would read the output through it. Make it temporary (CREATE TEMP VIEW …), which only this chat sees.",
    ))
}

/// (folders, files) a sealed instance may still read: the attached files
/// [`attach_files`](crate::files::attach_files) made views of. A folder ends with a separator so that
/// `/data/sales/` does not also allow `/data/sales_private/`.
pub(crate) fn sandbox_allowed(files: &[String]) -> (Vec<String>, Vec<String>) {
    let (mut dirs, mut paths) = (Vec::new(), Vec::new());
    for f in files {
        if detect_format(f).is_none() {
            continue;
        }
        if std::path::Path::new(f).is_dir() {
            let base = f.trim_end_matches(['/', '\\']);
            dirs.push(format!("{base}{}", std::path::MAIN_SEPARATOR));
        } else {
            paths.push(f.clone());
        }
    }
    (dirs, paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_util::*;
    use databrain_auth::AuthMethod;

    use databrain_connector_core::{ConnectionConfig, ConnectorError, ConnectorKind, ExecOptions, SANDBOX_OPTION, Session, quote_literal};

    /// No sealed instance without a spill folder of its own: when it can't
    /// be set (here the configuration is locked already), sealing fails
    /// instead of keeping the folder it had; with it set, sealing works.
    #[test]
    fn sealing_needs_its_own_spill_folder() {
        let _g = crate::engine::SPILL_TEST_LOCK.blocking_lock();
        let dir = tempfile::tempdir().unwrap();
        crate::set_spill_dir(Some(dir.path().to_string_lossy().into_owned()));
        crate::engine::engine().unwrap();
        let ok = duckdb::Connection::open_in_memory().unwrap();
        let sealed = seal(&ok, &[]);
        let locked = duckdb::Connection::open_in_memory().unwrap();
        locked.execute_batch("SET lock_configuration = true").unwrap();
        let refused = seal(&locked, &[]);
        crate::set_spill_dir(None);
        sealed.unwrap();
        let set: String = ok.query_row("SELECT current_setting('temp_directory')", [], |r| r.get(0)).unwrap();
        assert!(set.contains("sandbox-"), "{set}");
        let e = refused.unwrap_err().to_string();
        assert!(e.contains("temp_directory"), "{e}");
    }

    async fn q_err(s: &dyn Session, sql: &str) -> ConnectorError {
        match s.execute(databrain_connector_core::Statement::internal(sql), ExecOptions::default()).await {
            Ok(st) => st.collect().await.err().unwrap_or_else(|| panic!("{sql} ran in the sandbox")),
            Err(e) => e,
        }
    }

    /// Attached files and a Parquet folder, with neighbours the sandbox must not open.
    fn sandbox_files(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let csv = dir.join("sales.csv");
        std::fs::write(&csv, "region,amount\nEU,10\nUS,5\n").unwrap();
        std::fs::write(dir.join("secret.csv"), "k\nhidden\n").unwrap();
        let parts = dir.join("parts");
        let near = dir.join("parts_private");
        let c = duckdb::Connection::open_in_memory().unwrap();
        for d in [&parts, &near] {
            std::fs::create_dir_all(d).unwrap();
            c.execute_batch(&format!("COPY (SELECT range AS id FROM range(3)) TO {} (FORMAT parquet)", quote_literal(&d.join("p0.parquet").to_string_lossy())))
                .unwrap();
        }
        (csv, parts)
    }

    #[tokio::test]
    async fn sandbox_reads_attached_files_only() {
        let dir = tempfile::tempdir().unwrap();
        let (csv, parts) = sandbox_files(dir.path());
        let mut cfg = ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None);
        cfg.options.insert("files".into(), format!("{}\n{}", csv.display(), parts.display()));
        cfg.options.insert(SANDBOX_OPTION.into(), "1".into());
        let s = session(cfg).await;

        // The views are bound again on each query: the files they read stay readable.
        let r = q(&*s, "select sum(amount) from files.sales").await;
        assert_eq!(r.num_rows(), 1);
        assert_eq!(q(&*s, "select * from files.parts").await.num_rows(), 3);
        assert_eq!(q(&*s, &format!("select * from read_csv({})", quote_literal(&csv.to_string_lossy()))).await.num_rows(), 2);
        // The `results` catalog was attached before the lock.
        assert_eq!(q(&*s, "select database_name from duckdb_databases() where database_name = 'results'").await.num_rows(), 1);

        // Anything else on disk, the network and settings are refused.
        let other = dir.path().join("secret.csv");
        let e = q_err(&*s, &format!("select * from read_csv({})", quote_literal(&other.to_string_lossy()))).await;
        assert!(e.message.contains("sandbox"), "{}", e.message);
        let near = dir.path().join("parts_private").join("p0.parquet");
        let e = q_err(&*s, &format!("select * from read_parquet({})", quote_literal(&near.to_string_lossy()))).await;
        assert!(e.message.contains("sandbox"), "a folder with the same prefix: {}", e.message);
        let out = dir.path().join("out.csv");
        q_err(&*s, &format!("copy (select 1) to {}", quote_literal(&out.to_string_lossy()))).await;
        assert!(!out.exists());
        q_err(&*s, &format!("attach {} as other", quote_literal(&dir.path().join("x.duckdb").to_string_lossy()))).await;
        // The attached file and folder are readable by name, never writable:
        // not over the file, not a new file beside the folder's, not a
        // database created there, however the statement is wrapped.
        let in_parts = parts.join("z.parquet");
        let planted = quote_literal(&in_parts.to_string_lossy());
        let over = quote_literal(&csv.to_string_lossy());
        for sql in [
            format!("copy (select 1 as id) to {planted} (format parquet)"),
            format!("COPY (select 'x' as region) TO {over} (use_tmp_file false)"),
            format!("copy/**/(select 1 as id)/**/to {planted} (format parquet)"),
            format!("explain analyze copy (select 1 as id) to {planted} (format parquet)"),
            format!("prepare p as copy (select 1 as id) to {planted} (format parquet)"),
            format!("export database {}", quote_literal(&parts.to_string_lossy())),
            format!("attach {} as planted", quote_literal(&parts.join("new.duckdb").to_string_lossy())),
        ] {
            let e = q_err(&*s, &sql).await;
            assert!(e.message.contains("sandbox"), "{sql}: {}", e.message);
        }
        assert!(!in_parts.exists() && !parts.join("new.duckdb").exists());
        assert_eq!(std::fs::read_to_string(&csv).unwrap(), "region,amount\nEU,10\nUS,5\n");
        assert_eq!(std::fs::read_dir(&parts).unwrap().count(), 1, "only the file that was there");
        // Reading them still works, also into a table of the session.
        assert_eq!(q(&*s, "create temp table mine as select * from files.sales").await.num_rows(), 0);
        assert_eq!(q(&*s, "select * from mine").await.num_rows(), 2);
        q_err(&*s, "select * from read_csv('https://example.com/a.csv')").await;
        for set in
            ["SET enable_external_access = true", "SET allowed_directories = ['/']", "SET allowed_paths = ['/etc/passwd']", "SET lock_configuration = false"]
        {
            let e = q_err(&*s, set).await;
            assert!(e.message.contains("sandbox"), "{set}: {}", e.message);
        }
    }

    #[tokio::test]
    async fn sandbox_on_a_database_file() {
        let dir = tempfile::tempdir().unwrap();
        let (csv, _) = sandbox_files(dir.path());
        let db = dir.path().join("local.duckdb");
        duckdb::Connection::open(&db).unwrap().execute_batch("CREATE TABLE t AS SELECT 1 AS a").unwrap();
        let mut cfg = ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None);
        cfg.file_path = Some(db.to_string_lossy().into_owned());
        cfg.options.insert("files".into(), csv.to_string_lossy().into_owned());
        cfg.options.insert(SANDBOX_OPTION.into(), "1".into());
        let s = session(cfg.clone()).await;
        assert_eq!(q(&*s, "select * from t").await.num_rows(), 1);
        assert_eq!(q(&*s, "select * from files.sales").await.num_rows(), 2);
        // A second sandboxed session shares the sealed instance.
        let s2 = session(cfg).await;
        assert_eq!(q(&*s2, "select * from files.sales").await.num_rows(), 2);
        let e = q_err(&*s2, &format!("select * from read_csv({})", quote_literal(&dir.path().join("secret.csv").to_string_lossy()))).await;
        assert!(e.message.contains("sandbox"), "{}", e.message);
    }

    /// What a sealed session on a shared file may keep: views and macros
    /// that can't read `results`, and temporary ones of any kind.
    #[test]
    fn kept_views_over_outputs_are_refused() {
        for sql in [
            "create temp view v as select * from results.sales",
            "CREATE OR REPLACE TEMPORARY VIEW v AS SELECT * FROM results.sales",
            "create temp macro m() as table select * from results.sales",
            "create view v as select * from t",
            "create table t2 as select * from t",
            "select * from results.sales",
            "create macro twice(x) as x * 2",
            // All of it a comment in DuckDB (comments nest): nothing is kept.
            "/* a /* b */ create view v as select * from results.sales */ select 1",
        ] {
            assert!(refuse_shared_views(sql).is_ok(), "{sql}");
        }
        for sql in [
            "create view v as select * from results.sales",
            "CREATE OR REPLACE VIEW v AS SELECT * FROM results /* x */ . sales",
            "create view v as select * from \"results\".sales",
            "create view v as select * from query('select * from results.sales')",
            "create view v as select * from query_table('res' || 'ults.sales')",
            "create macro m() as table select * from results.sales",
            "create or replace function f() as table select * from results.sales",
            "/* note */ create view v as select * from results.sales",
            // A nested comment's first `*/` doesn't end it, and what follows it runs.
            "/* a /* b */ c */ create view v as select * from results.sales",
            "alter view v rename to results",
        ] {
            let e = refuse_shared_views(sql).expect_err(sql);
            assert!(e.message.contains("CREATE TEMP VIEW"), "{sql}: {}", e.message);
        }
    }

    /// Two chats' sandboxed sessions on one database file: a kept view that
    /// could read outputs is refused there (not in a private in-memory
    /// sandbox), and a temporary one is seen only by the session that made it.
    #[tokio::test]
    async fn chats_sharing_a_file_cant_leave_views_over_outputs() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("local.duckdb");
        duckdb::Connection::open(&db).unwrap().execute_batch("CREATE TABLE t AS SELECT 1 AS a").unwrap();
        let mut cfg = ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None);
        cfg.file_path = Some(db.to_string_lossy().into_owned());
        cfg.options.insert(SANDBOX_OPTION.into(), "1".into());
        let a = session(cfg.clone()).await;
        let b = session(cfg).await;
        let e = q_err(&*a, "create view leak as select 1 as results").await;
        assert!(e.message.contains("CREATE TEMP VIEW"), "{}", e.message);
        q(&*a, "create view plain as select * from t").await;
        q(&*a, "create temp view mine as select 1 as results").await;
        assert_eq!(q(&*a, "select * from mine").await.num_rows(), 1);
        assert!(b.execute(databrain_connector_core::Statement::internal("select * from mine"), ExecOptions::default()).await.unwrap().collect().await.is_err());
        assert_eq!(q(&*b, "select * from plain").await.num_rows(), 1);
        // A private sandbox (in memory, one session) may keep it.
        let mut mem = ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None);
        mem.options.insert(SANDBOX_OPTION.into(), "1".into());
        q(&*session(mem).await, "create view kept as select 1 as results").await;
    }

    #[test]
    fn sandbox_allows_attached_files_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let (csv, parts) = sandbox_files(dir.path());
        let sep = std::path::MAIN_SEPARATOR;
        let files = vec![csv.to_string_lossy().into_owned(), format!("{}{sep}", parts.display()), "/x/notes.bin".into()];
        let (dirs, paths) = sandbox_allowed(&files);
        assert_eq!(dirs, vec![format!("{}{sep}", parts.display())], "one separator at the end");
        assert_eq!(paths, vec![csv.to_string_lossy().into_owned()], "files DataBrain makes no view of are not allowed");
    }
}
