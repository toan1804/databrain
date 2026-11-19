//! `databrain query` (also started as `dbr`): run SQL from a terminal on a
//! saved connection, with its credentials and rules, and print the rows.
//!
//! ```text
//! dbr -c "Sales PG" -i "select * from orders"      a table of the first rows (100; --limit up to 1,000)
//! dbr -c sales -i --csv "select …" > out.csv       every row as CSV (also --json, --tsv)
//! dbr -i data.parquet orders.xlsx                  data files, read by the local DuckDB
//! dbr -c "Sales PG" -i report.sql -p day=2026-11-18
//! ```
//!
//! A table in the terminal shows up to 1,000 rows (more is unreadable and
//! fills the scrollback): CSV, JSON and TSV, for files and pipes, print
//! every row (`--limit` caps them). When stdout is not a terminal the
//! default is CSV. The connection's read-only setting and production
//! confirmation apply (asked here, `--yes` without a terminal), the table
//! access rules too, and every run goes to the query history (Terminal).
//! Exit codes: 0 done, 1 a statement failed, 2 bad arguments, 130 stopped.

use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use databrain_connector_core::sql::ParamValue;
use databrain_query_engine::{EngineError, RunRequest};
use databrain_result_store::{ColumnMeta, TypeFamily, ViewSpec};
use databrain_workspace::{ConnectionProfile, Origin};

use crate::api::AppState;
use crate::background;

const USAGE: &str = "usage: dbr [-c <connection>] -i [--csv|--json|--tsv] [--limit N] [-p name=value]… [--yes] <sql | file.sql | data file>…
  -c, --conn <name>    a saved connection (name, any case); default: the local DuckDB
  -i, --inline         print the rows here (opening them in the app comes later)
  -e, --execute <sql>  this text is SQL (when it could be read as a file name)
  --csv, --json, --tsv every row in that format (CSV by default when piped)
  --limit <n>          rows to show: a table shows 100 by default, at most 1,000
  -p, --param <n=v>    a value for :n (repeatable)
  -y, --yes            run on a production connection without asking";

/// Most rows a table in the terminal shows.
pub const TABLE_MAX: usize = 1_000;
/// Rows a table shows when `--limit` isn't given.
pub const TABLE_DEFAULT: usize = 100;
/// Widest a column of a table gets (longer values are cut with `…`).
const CELL_MAX: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Table,
    Csv,
    Json,
    Tsv,
}

/// What a command line asks for.
#[derive(Debug, Default, PartialEq)]
pub struct Opts {
    pub conn: Option<String>,
    pub inline: bool,
    /// SQL texts and files, in order.
    pub sources: Vec<Source>,
    /// Asked for (`--csv` …); `None`: a table, or CSV when piped.
    pub format: Option<Format>,
    pub limit: Option<usize>,
    pub params: HashMap<String, ParamValue>,
    pub yes: bool,
    pub help: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    Sql(String),
    File(PathBuf),
}

/// Read the command line (`exists`: whether a path names a file, so SQL
/// that is no file is SQL).
pub fn parse(args: &[String], exists: impl Fn(&Path) -> bool) -> Result<Opts, String> {
    let mut o = Opts::default();
    let mut it = args.iter();
    let value = |it: &mut std::slice::Iter<'_, String>, flag: &str| it.next().cloned().ok_or_else(|| format!("{flag} needs a value"));
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => o.help = true,
            "-i" | "--inline" => o.inline = true,
            "-y" | "--yes" => o.yes = true,
            "-c" | "--conn" => o.conn = Some(value(&mut it, a)?),
            "-e" | "--execute" => o.sources.push(Source::Sql(value(&mut it, a)?)),
            "--csv" => o.format = Some(Format::Csv),
            "--json" => o.format = Some(Format::Json),
            "--tsv" => o.format = Some(Format::Tsv),
            "--limit" => {
                let v = value(&mut it, a)?;
                o.limit = Some(v.parse().map_err(|_| format!("--limit takes a number of rows, not {v:?}"))?);
            }
            "-p" | "--param" => {
                let v = value(&mut it, a)?;
                let (k, val) = v.split_once('=').ok_or_else(|| format!("{a} takes name=value, not {v:?}"))?;
                o.params.insert(k.trim().trim_start_matches(':').to_string(), ParamValue { value: val.to_string(), raw: false });
            }
            f if f.starts_with('-') && f.len() > 1 && !f.contains(' ') => return Err(format!("unknown option {f:?}")),
            s if exists(Path::new(s)) => o.sources.push(Source::File(PathBuf::from(s))),
            s => o.sources.push(Source::Sql(s.to_string())),
        }
    }
    if !o.help && o.sources.is_empty() {
        return Err("nothing to run: give SQL, a .sql file or a data file".into());
    }
    Ok(o)
}

/// The format to print in: the one asked for, else a table on a terminal
/// and CSV in a pipe or file.
pub fn format_for(asked: Option<Format>, terminal: bool) -> Format {
    asked.unwrap_or(if terminal { Format::Table } else { Format::Csv })
}

/// Rows to read: a table's `--limit` (100 by default, refused past 1,000),
/// a format's `--limit` or every row.
pub fn row_limit(format: Format, limit: Option<usize>) -> Result<Option<usize>, String> {
    match format {
        Format::Table => match limit.unwrap_or(TABLE_DEFAULT) {
            0 => Err(format!("a table shows at most {TABLE_MAX} rows: use --csv, --json or --tsv for every row")),
            n if n > TABLE_MAX => Err(format!("a table shows at most {TABLE_MAX} rows (more is unreadable here): use --csv, --json or --tsv for {n}")),
            n => Ok(Some(n)),
        },
        _ => Ok(limit.filter(|n| *n > 0)),
    }
}

/// The saved connection `name` names: the same name (any case), else the
/// only one whose name contains it; else why not, with the close ones.
pub fn find_connection<'a>(all: &'a [ConnectionProfile], name: &str) -> Result<&'a ConnectionProfile, String> {
    let low = name.trim().to_lowercase();
    if let Some(c) = all.iter().find(|c| c.name.to_lowercase() == low || c.id == name) {
        return Ok(c);
    }
    let close: Vec<&ConnectionProfile> = all.iter().filter(|c| c.name.to_lowercase().contains(&low)).collect();
    match close.as_slice() {
        [one] => Ok(one),
        [] => Err(format!(
            "no connection named {name:?}; the saved ones: {}",
            if all.is_empty() { "none".to_string() } else { all.iter().map(|c| format!("{:?}", c.name)).collect::<Vec<_>>().join(", ") }
        )),
        many => Err(format!("{name:?} names several connections: {}", many.iter().map(|c| format!("{:?}", c.name)).collect::<Vec<_>>().join(", "))),
    }
}

/// `s` cut to `w` characters (the last one `…` when cut).
fn fit(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n <= w {
        return s.to_string();
    }
    let mut out: String = s.chars().take(w.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// One line of a cell (line breaks and tabs shown as spaces).
fn flat(s: &str) -> String {
    s.chars().map(|c| if c == '\n' || c == '\r' || c == '\t' { ' ' } else { c }).collect()
}

/// The rows as a table that fits `width` characters: columns as wide as
/// their values (at most 40), the widest shrunk until it fits, numbers to
/// the right, NULL as `NULL`.
pub fn render_table(columns: &[ColumnMeta], rows: &[Vec<Option<String>>], width: usize) -> String {
    let cells: Vec<Vec<String>> = rows.iter().map(|r| r.iter().map(|c| c.as_deref().map(flat).unwrap_or_else(|| "NULL".into())).collect()).collect();
    let mut w: Vec<usize> = columns
        .iter()
        .enumerate()
        .map(|(i, c)| cells.iter().map(|r| r.get(i).map_or(0, |s| s.chars().count())).chain([c.name.chars().count()]).max().unwrap_or(1).clamp(1, CELL_MAX))
        .collect();
    // " │ " between columns.
    let total = |w: &[usize]| w.iter().sum::<usize>() + 3 * w.len().saturating_sub(1);
    while total(&w) > width {
        let Some((i, max)) = w.iter().copied().enumerate().max_by_key(|(_, x)| *x) else { break };
        if max <= 4 {
            break;
        }
        w[i] = max - 1;
    }
    let right: Vec<bool> = columns.iter().map(|c| c.family == TypeFamily::Number).collect();
    let line = |vals: Vec<String>| -> String {
        vals.iter()
            .enumerate()
            .map(|(i, v)| {
                let v = fit(v, w[i]);
                if right[i] { format!("{v:>width$}", width = w[i]) } else { format!("{v:<width$}", width = w[i]) }
            })
            .collect::<Vec<_>>()
            .join(" │ ")
            .trim_end()
            .to_string()
    };
    let mut out = line(columns.iter().map(|c| flat(&c.name)).collect());
    out.push('\n');
    out.push_str(&w.iter().map(|n| "─".repeat(*n)).collect::<Vec<_>>().join("─┼─"));
    out.push('\n');
    for r in cells {
        out.push_str(&line(r));
        out.push('\n');
    }
    out
}

/// A CSV field (RFC 4180: quoted when it holds a comma, quote or line break); NULL is empty.
fn csv_field(v: Option<&str>) -> String {
    match v {
        None => String::new(),
        Some(s) if s.contains([',', '"', '\n', '\r']) => format!("\"{}\"", s.replace('"', "\"\"")),
        Some(s) => s.to_string(),
    }
}

/// A TSV field: tabs, line breaks and backslashes escaped; NULL as `\N`.
fn tsv_field(v: Option<&str>) -> String {
    match v {
        None => "\\N".into(),
        Some(s) => s.replace('\\', "\\\\").replace('\t', "\\t").replace('\n', "\\n").replace('\r', "\\r"),
    }
}

/// A JSON value: numbers and booleans as such, NULL as null, the rest strings.
fn json_value(v: Option<&str>, family: TypeFamily) -> serde_json::Value {
    match (v, family) {
        (None, _) => serde_json::Value::Null,
        (Some(s), TypeFamily::Number) => s.parse::<f64>().ok().filter(|f| f.is_finite()).and_then(|_| serde_json::from_str(s).ok()).unwrap_or_else(|| s.into()),
        (Some(s), TypeFamily::Bool) if s.eq_ignore_ascii_case("true") || s.eq_ignore_ascii_case("false") => s.eq_ignore_ascii_case("true").into(),
        (Some(s), _) => s.into(),
    }
}

/// Rows in a format meant for files and pipes (a header line for CSV and TSV).
pub fn render_rows(format: Format, columns: &[ColumnMeta], rows: &[Vec<Option<String>>], header: bool) -> String {
    let mut out = String::new();
    let join = |vals: Vec<String>, sep: &str| vals.join(sep) + "\n";
    match format {
        Format::Csv | Format::Tsv => {
            let (field, sep): (fn(Option<&str>) -> String, &str) = if format == Format::Csv { (csv_field, ",") } else { (tsv_field, "\t") };
            if header {
                out.push_str(&join(columns.iter().map(|c| field(Some(&c.name))).collect(), sep));
            }
            for r in rows {
                out.push_str(&join(r.iter().map(|v| field(v.as_deref())).collect(), sep));
            }
        }
        Format::Json => {
            for r in rows {
                let obj: serde_json::Map<String, serde_json::Value> =
                    columns.iter().zip(r).map(|(c, v)| (c.name.clone(), json_value(v.as_deref(), c.family))).collect();
                out.push_str(&serde_json::Value::Object(obj).to_string());
                out.push('\n');
            }
        }
        Format::Table => unreachable!("tables go through render_table"),
    }
    out
}

pub fn main(args: Vec<String>) -> ExitCode {
    let opts = match parse(&args, |p| p.is_file() || p.is_dir() && p.extension().is_some()) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("dbr: {e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    if opts.help {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    if !opts.inline {
        eprintln!("dbr: opening the rows in the DataBrain app isn't available yet: add -i to print them here\n{USAGE}");
        return ExitCode::from(2);
    }
    let rt = match crate::cli::runtime() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("dbr: {e}");
            return ExitCode::from(2);
        }
    };
    rt.block_on(run(opts))
}

/// A statement's SQL, from where it came.
struct Job {
    label: String,
    sql: String,
    /// Data files are read by the local DuckDB, whatever `-c` says.
    local: bool,
}

async fn run(opts: Opts) -> ExitCode {
    let dir = background::data_dir();
    crate::logging::init("databrain-query", Some(&dir), false);
    let state = match background::headless_state(&dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("dbr: {}", e.message);
            return ExitCode::from(2);
        }
    };
    let terminal = Terminal { out: std::io::stdout().is_terminal(), input: std::io::stdin().is_terminal() };
    let mut stdout = std::io::stdout().lock();
    let code = run_with(&state, opts, &mut stdout, terminal).await;
    let _ = stdout.flush();
    crate::api::remove_spill_folders(&state.engine);
    ExitCode::from(code)
}

/// Where `dbr` runs: rows printed to a terminal (else a pipe or file), and
/// a terminal to ask a question in.
#[derive(Debug, Clone, Copy)]
pub struct Terminal {
    pub out: bool,
    pub input: bool,
}

/// Run what `opts` asks on `state`, the rows to `out`, messages to stderr;
/// the exit code (0 done, 1 a statement failed, 2 bad arguments, 130 stopped).
pub async fn run_with(state: &AppState, opts: Opts, out: &mut dyn Write, terminal: Terminal) -> u8 {
    let format = format_for(opts.format, terminal.out);
    let limit = match row_limit(format, opts.limit) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("dbr: {e}");
            return 2;
        }
    };
    // What to run, in order.
    let mut jobs = Vec::new();
    for s in &opts.sources {
        match s {
            Source::Sql(sql) => jobs.push(Job { label: "SQL".into(), sql: sql.clone(), local: false }),
            Source::File(p) if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("sql")) => match std::fs::read_to_string(p) {
                Ok(sql) => jobs.push(Job { label: p.display().to_string(), sql, local: false }),
                Err(e) => {
                    eprintln!("dbr: cannot read {}: {e}", p.display());
                    return 2;
                }
            },
            Source::File(p) => match file_sql(p) {
                Ok(sql) => jobs.push(Job { label: p.display().to_string(), sql, local: true }),
                Err(e) => {
                    eprintln!("dbr: {}: {e}", p.display());
                    return 2;
                }
            },
        }
    }
    let all = state.workspace.list_connections().unwrap_or_default();
    let named = match opts.conn.as_deref().map(|n| find_connection(&all, n)) {
        Some(Ok(c)) => Some(c.clone()),
        Some(Err(e)) => {
            eprintln!("dbr: {e}");
            return 2;
        }
        None => None,
    };
    let local = || -> Result<ConnectionProfile, EngineError> {
        // The "Local files (DuckDB)" connection the app reads files with, else Results.
        match all.iter().find(|c| c.config.kind == databrain_connector_core::ConnectorKind::Duckdb && c.config.file_path.is_none()) {
            Some(c) => Ok(c.clone()),
            None => state.engine.results_connection(),
        }
    };
    let cancel = databrain_connector_core::CancellationToken::new();
    {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                cancel.cancel();
            }
        });
    }
    let mut failed = false;
    let width = std::env::var("COLUMNS").ok().and_then(|c| c.parse().ok()).unwrap_or(120);
    for job in &jobs {
        let conn = match (&named, job.local) {
            (Some(c), false) => c.clone(),
            _ => match local() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("dbr: {}", e.message);
                    return 2;
                }
            },
        };
        let req = |confirmed: bool| RunRequest {
            connection_id: conn.id.clone(),
            tab_id: format!("cli:{}", uuid::Uuid::new_v4().simple()),
            sql: job.sql.clone(),
            base_offset: 0,
            row_limit: limit,
            confirmed,
            origin: Origin::Cli,
            session_key: None,
            output_name: None,
            params: opts.params.clone(),
            output_masks: None,
        };
        let started = std::time::Instant::now();
        let mut res = state.engine.run_and_wait(&state.hub, req(false), Some(cancel.clone())).await;
        // Production (or a write the connection asks about): asked here, or --yes.
        if let Err(e) = &res
            && e.code.as_deref() == Some("needs_confirmation")
        {
            let go = opts.yes || (terminal.input && ask(&format!("{}\nRun it on \"{}\"? [y/N] ", e.message, conn.name)));
            if !go {
                eprintln!("dbr: not run on \"{}\": {} (--yes runs it without asking)", conn.name, e.message);
                return 2;
            }
            res = state.engine.run_and_wait(&state.hub, req(true), Some(cancel.clone())).await;
        }
        if cancel.is_cancelled() {
            eprintln!("dbr: stopped");
            return 130;
        }
        let outcomes = match res {
            Ok(o) => o,
            Err(e) => {
                eprintln!("dbr: {}: {}", job.label, e.message);
                failed = true;
                continue;
            }
        };
        let n = outcomes.len();
        for o in &outcomes {
            if n > 1 && format == Format::Table {
                let _ = writeln!(out, "-- statement {} of {n}", o.index + 1);
            }
            if let Some(e) = &o.error {
                eprintln!("dbr: {}statement {}: {}", if jobs.len() > 1 { format!("{}: ", job.label) } else { String::new() }, o.index + 1, e.message);
                failed = true;
                continue;
            }
            let Some(r) = &o.result else {
                // Said apart from the rows, so a pipe gets only data.
                let affected = o.rows_affected.map(|r| format!("{r} rows affected")).unwrap_or_else(|| "done".into());
                if format == Format::Table {
                    let _ = writeln!(out, "{affected} · {} · {} ms", conn.name, o.duration_ms);
                } else {
                    eprintln!("{affected}");
                }
                continue;
            };
            let rows = match read_rows(state, &r.id, r.total_rows).await {
                Ok(rows) => rows,
                Err(e) => {
                    eprintln!("dbr: {}", e.message);
                    failed = true;
                    continue;
                }
            };
            if format == Format::Table {
                let _ = out.write_all(render_table(&r.columns, &rows, width).as_bytes());
                let more = if r.truncated { format!(" (more not read: --limit up to {TABLE_MAX}, or --csv for every row)") } else { String::new() };
                let _ =
                    writeln!(out, "{} row{}{more} · {} · {} ms", rows.len(), if rows.len() == 1 { "" } else { "s" }, conn.name, started.elapsed().as_millis());
            } else {
                let _ = out.write_all(render_rows(format, &r.columns, &rows, true).as_bytes());
            }
        }
    }
    let _ = out.flush();
    if failed { 1 } else { 0 }
}

/// Every row of a stored result, a page at a time.
async fn read_rows(state: &AppState, id: &str, total: usize) -> Result<Vec<Vec<Option<String>>>, EngineError> {
    let mut rows = Vec::with_capacity(total.min(100_000));
    while rows.len() < total {
        let page = crate::api::fetch_page(state, id.to_string(), ViewSpec::default(), rows.len(), 5_000).await?;
        if page.rows.is_empty() {
            break;
        }
        rows.extend(page.rows);
    }
    Ok(rows)
}

/// The SQL that reads a data file every row (CSV, Parquet, JSON, Excel's first sheet, Delta/Iceberg folders).
#[cfg(feature = "duckdb-engine")]
fn file_sql(p: &Path) -> Result<String, String> {
    crate::api::file_scan_sql_with(&p.to_string_lossy(), false, false).map_err(|e| e.message)
}

#[cfg(not(feature = "duckdb-engine"))]
fn file_sql(p: &Path) -> Result<String, String> {
    let _ = p;
    Err("this DataBrain reads data files with DuckDB, which isn't built in: query them in the app with a DuckDB library installed".into())
}

/// Ask a yes/no question on the terminal (no means no).
fn ask(question: &str) -> bool {
    eprint!("{question}");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).is_ok() && matches!(line.trim().to_lowercase().as_str(), "y" | "yes")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, family: TypeFamily) -> ColumnMeta {
        ColumnMeta { name: name.into(), data_type: "x".into(), db_type: None, family }
    }

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn command_lines() {
        let files = |p: &Path| p.to_str().is_some_and(|s| s.ends_with(".sql") || s.ends_with(".parquet"));
        let o = parse(&args(&["-c", "Sales PG", "-i", "select 1", "report.sql", "--limit", "500", "-p", ":day=2026-11-18", "--csv", "-y"]), files).unwrap();
        assert_eq!(o.conn.as_deref(), Some("Sales PG"));
        assert!(o.inline && o.yes);
        assert_eq!(o.sources, [Source::Sql("select 1".into()), Source::File("report.sql".into())]);
        assert_eq!((o.format, o.limit), (Some(Format::Csv), Some(500)));
        assert_eq!(o.params["day"].value, "2026-11-18");
        // -e forces SQL; a long option with a space in it is SQL, not an option.
        let o = parse(&args(&["-i", "-e", "data.parquet", "--not an option"]), files).unwrap();
        assert_eq!(o.sources, [Source::Sql("data.parquet".into()), Source::Sql("--not an option".into())]);
        assert!(parse(&args(&["-i"]), files).unwrap_err().contains("nothing to run"));
        assert!(parse(&args(&["-i", "--limit", "many", "x"]), files).unwrap_err().contains("--limit takes"));
        assert!(parse(&args(&["-i", "-p", "day", "x"]), files).unwrap_err().contains("name=value"));
        assert!(parse(&args(&["-x", "x"]), files).unwrap_err().contains("unknown option"));
        assert!(parse(&args(&["--help"]), files).unwrap().help);
    }

    #[test]
    fn a_table_shows_at_most_1000_rows_and_formats_every_row() {
        assert_eq!(format_for(None, true), Format::Table);
        assert_eq!(format_for(None, false), Format::Csv, "piped: CSV");
        assert_eq!(format_for(Some(Format::Json), true), Format::Json);
        assert_eq!(row_limit(Format::Table, None), Ok(Some(100)));
        assert_eq!(row_limit(Format::Table, Some(1000)), Ok(Some(1000)));
        assert!(row_limit(Format::Table, Some(1001)).unwrap_err().contains("at most 1000"));
        assert!(row_limit(Format::Table, Some(0)).is_err());
        assert_eq!(row_limit(Format::Csv, None), Ok(None));
        assert_eq!(row_limit(Format::Csv, Some(0)), Ok(None));
        assert_eq!(row_limit(Format::Json, Some(50)), Ok(Some(50)));
    }

    #[test]
    fn connections_by_name() {
        let p = |name: &str| ConnectionProfile {
            id: format!("id-{name}"),
            name: name.into(),
            config: databrain_connector_core::ConnectionConfig::new(databrain_connector_core::ConnectorKind::Sqlite, databrain_auth::AuthMethod::None),
            color: None,
            env: databrain_workspace::EnvTag::None,
            folder_id: None,
            has_secret: false,
            ai_policy: Default::default(),
            created_at: 0,
            updated_at: 0,
        };
        let all = vec![p("Sales PG"), p("Sales MySQL"), p("Warehouse")];
        assert_eq!(find_connection(&all, "sales pg").unwrap().name, "Sales PG");
        assert_eq!(find_connection(&all, "ware").unwrap().name, "Warehouse");
        assert!(find_connection(&all, "sales").unwrap_err().contains("several"));
        assert!(find_connection(&all, "crm").unwrap_err().contains("\"Warehouse\""));
    }

    #[test]
    fn tables_fit_the_terminal() {
        let cols = [col("id", TypeFamily::Number), col("status", TypeFamily::Text), col("note", TypeFamily::Text)];
        let rows = vec![
            vec![Some("1".into()), Some("paid".into()), None],
            vec![Some("120".into()), Some("open".into()), Some("a\tlong\nnote that goes on and on and on and on and on".into())],
        ];
        let t = render_table(&cols, &rows, 120);
        assert_eq!(
            t.lines().collect::<Vec<_>>(),
            [
                " id │ status │ note",
                "────┼────────┼─────────────────────────────────────────",
                "  1 │ paid   │ NULL",
                "120 │ open   │ a long note that goes on and on and on …",
            ]
        );
        // Narrow: the widest column gives way first, every line fits.
        let narrow = render_table(&cols, &rows, 30);
        assert!(narrow.lines().all(|l| l.chars().count() <= 30), "{narrow}");
    }

    #[test]
    fn formats_for_files_and_pipes() {
        let cols = [col("id", TypeFamily::Number), col("name", TypeFamily::Text), col("ok", TypeFamily::Bool)];
        let rows = vec![vec![Some("1".into()), Some("a, \"b\"".into()), Some("true".into())], vec![Some("2.5".into()), None, Some("false".into())]];
        assert_eq!(render_rows(Format::Csv, &cols, &rows, true), "id,name,ok\n1,\"a, \"\"b\"\"\",true\n2.5,,false\n");
        assert_eq!(render_rows(Format::Tsv, &cols, &rows, true), "id\tname\tok\n1\ta, \"b\"\ttrue\n2.5\t\\N\tfalse\n");
        assert_eq!(
            render_rows(Format::Json, &cols, &rows, true),
            "{\"id\":1,\"name\":\"a, \\\"b\\\"\",\"ok\":true}\n{\"id\":2.5,\"name\":null,\"ok\":false}\n"
        );
        // A number that isn't one JSON reads (NaN, huge text) stays a string.
        assert_eq!(json_value(Some("NaN"), TypeFamily::Number), serde_json::Value::from("NaN"));
    }
}
