//! `databrain query` (also started as `dbr`): run SQL from a terminal on a
//! saved connection, with its credentials and rules, and print the rows
//! (`-i`); without `-i`, the running app opens them ([`crate::handoff`]).
//!
//! ```text
//! dbr orders.csv                                   the file's rows in a tab of the app
//! dbr -c Sales --run report.sql                    the file in a tab, run on "Sales"
//! dbr -c Sales -i "select * from orders"           a table of the first rows (100; --limit up to 1,000)
//! dbr -c Sales -i --csv "select …" > out.csv       every row as CSV (also --json, --tsv)
//! dbr -i data.parquet orders.xlsx                  data files, read by the local DuckDB
//! dbr -c Sales -i report.sql -p day=2026-11-18
//! ```
//!
//! A table in the terminal shows up to 1,000 rows (more is unreadable and
//! fills the scrollback): CSV, JSON and TSV, for files and pipes, print
//! every row (`--limit` caps them), a page at a time as they are read. When
//! stdout is not a terminal the default is CSV. It runs as SQL run in the
//! app's editor does: the connection's read-only setting and production
//! confirmation apply (asked here, `--yes` without a terminal); the table
//! access rules and PII masking are the AI's and don't. Every run goes to
//! the query history (Terminal). Exit codes: 0 done, 1 a statement failed,
//! 2 bad arguments or a question answered no, 130 stopped (Ctrl-C).

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

const USAGE: &str = "usage: dbr [-c <connection>] [--run] [-p name=value]… <sql | file.sql | data file>…   (opens them in DataBrain)
       dbr [-c <connection>] -i [--csv|--json|--tsv|-x] [--limit N] [-p name=value]… [--yes] <sql | file.sql | data file>…
       dbr jobs list | run <job> | runs <job> | stop <job>   (dbr jobs --help)
       dbr --help | --version";

/// `dbr --help`: what it does, its options, examples and exit codes.
const HELP: &str = "dbr: open data files and SQL in DataBrain, or run SQL on a DataBrain
connection and print the rows here (-i).

usage: dbr [-c <connection>] [--run] [-p name=value]… <sql | file.sql | data file>…
       dbr [-c <connection>] -i [options] <sql | file.sql | data file>…
       dbr jobs list | run <job> | runs <job> | stop <job>

Without -i, the running DataBrain (started when it isn't) opens each one in a
tab: a data file's rows as a result; a .sql file in its file tab (on -c's
connection when given; run too with --run); SQL run on the connection (the
local DuckDB without -c). They run as the editor runs them.

options:
  -c, --conn <name>    a saved connection: its name (any case) or its id;
                       checked before anything runs; default: the local
                       DuckDB
  -i, --inline         print the rows here instead of opening them in the app
  --run                without -i: run the .sql files the app opens
  -e, --execute <sql>  this text is SQL (when it could be read as a file name)
  --csv, --json, --tsv with -i: every row in that format (CSV by default
                       when piped)
  -x, --expanded       with -i: each row as a record, a line per column,
                       long values whole (a table cuts them at 40
                       characters); rows as for a table
  --limit <n>          with -i: at most n rows: a table shows 100 by default
                       and at most 1,000; CSV, JSON and TSV every row unless
                       given
  -p, --param <n=v>    a value for :n in the SQL (repeatable)
  -y, --yes            with -i: run on a production connection without asking
  --connections        the saved connections' names, one a line
  -h, --help           this help
  -v, --version        the version

With -i it runs as the app's editor does: a read-only connection refuses
writes, and what the app would confirm on a production connection is asked
here ([y/N]; refused without a terminal unless --yes). Each run is in the
app's query history as \"Terminal\".

examples:
  dbr orders.csv                               (opens it in DataBrain)
  dbr -c Sales report.sql --run                (opens and runs it there)
  dbr -c Sales -i \"select * from orders limit 10\"
  dbr -c Sales -i report.sql -p day=2026-11-20
  dbr -c Sales -i -x \"select * from orders where id = 42\"
  dbr -c Sales -i --csv \"select * from orders\" > orders.csv
  dbr -i data.parquet orders.xlsx events/      (CSV, Parquet, JSON, Excel,
                                               Delta/Iceberg folders)
  dbr jobs run \"Daily load\"

exit codes: 0 done (opened in the app), 1 a statement failed, 2 bad
arguments (an unknown connection too), a question answered no or the app
not reached, 130 stopped (Ctrl-C).

where it is: macOS DataBrain.app/Contents/MacOS/dbr (link it into your
PATH); Windows the install folder (add it to PATH); Linux /usr/bin/dbr
from the .deb or .rpm.";

/// Most rows a table in the terminal shows.
pub const TABLE_MAX: usize = 1_000;
/// Rows a table shows when `--limit` isn't given.
pub const TABLE_DEFAULT: usize = 100;
/// Widest a column of a table gets (longer values are cut with `…`).
const CELL_MAX: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Table,
    /// `-x`: each row as a record, a line per column, values whole.
    Expanded,
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
    /// `--run`: the app runs the `.sql` files it opens.
    pub run: bool,
    pub help: bool,
    pub version: bool,
    /// `--connections`: print the saved connections' names (for `-c` and
    /// tab completion).
    pub connections: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    Sql(String),
    File(PathBuf),
}

/// Extensions of the files dbr reads (data files and `.sql`;
/// `detect_format`'s, gzipped and zstd ones too; `.xls` to say it isn't there).
const FILE_EXTENSIONS: [&str; 14] = ["sql", "csv", "tsv", "psv", "txt", "json", "jsonl", "ndjson", "parquet", "pq", "xlsx", "xls", "gz", "zst"];

/// Words a SQL statement starts with: text starting with one is SQL,
/// whatever it ends with (`select * from s.csv`: the table `csv`).
const SQL_STARTS: [&str; 40] = [
    "select",
    "from",
    "with",
    "show",
    "describe",
    "desc",
    "summarize",
    "pragma",
    "insert",
    "update",
    "delete",
    "merge",
    "upsert",
    "replace",
    "create",
    "drop",
    "alter",
    "truncate",
    "copy",
    "export",
    "import",
    "explain",
    "call",
    "exec",
    "execute",
    "set",
    "reset",
    "use",
    "values",
    "table",
    "attach",
    "detach",
    "install",
    "load",
    "checkpoint",
    "vacuum",
    "analyze",
    "begin",
    "commit",
    "rollback",
];

/// Whether `s` can only be a file's name (or a URL): it ends in an
/// extension dbr reads (`my file.csv`, `year=2024/p.parquet`) or is one
/// word that is a path (`./x`, `data/x`, `~/x`), and starts with no SQL
/// word and holds no quotes or `;` (`select * from 'a.csv'` is SQL).
fn names_a_file(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() || s.contains(['\'', '"', ';']) || s.starts_with('\\') {
        return false;
    }
    if s.contains("://") {
        return true;
    }
    let first = s.split(|c: char| c.is_whitespace() || c == '(').next().unwrap_or_default();
    if SQL_STARTS.iter().any(|k| k.eq_ignore_ascii_case(first)) {
        return false;
    }
    let ext = Path::new(s).extension().and_then(|e| e.to_str()).is_some_and(|e| FILE_EXTENSIONS.iter().any(|x| x.eq_ignore_ascii_case(e)));
    let word = !s.contains(|c: char| c.is_whitespace() || "()*=,".contains(c));
    ext || (word && (s.contains('/') || s.contains('\\') || s.starts_with('~')))
}

/// Why `s`, a file's name that names no file or folder, can't be read.
fn not_a_file(s: &str) -> String {
    let p = Path::new(s);
    let why = if s.contains("://") {
        return format!("{s}: dbr reads files on this computer; read a URL with SQL (-e \"select * from '<url>'\")");
    } else if p.exists() {
        "not a file or folder".to_string()
    } else if s.starts_with('~') {
        "no such file (a quoted ~ isn't your home folder)".to_string()
    } else if p.is_absolute() {
        "no such file".to_string()
    } else {
        match std::env::current_dir() {
            Ok(d) => format!("no such file in {}", d.display()),
            Err(_) => "no such file".to_string(),
        }
    };
    format!("{s}: {why} (-e before it runs it as SQL)")
}

/// Read the command line (`exists`: whether a path names a file, so SQL
/// that is no file is SQL; a word that can only be a file, not there, is
/// an error).
pub fn parse(args: &[String], exists: impl Fn(&Path) -> bool) -> Result<Opts, String> {
    let mut o = Opts::default();
    let mut missing = None;
    let mut it = args.iter();
    let value = |it: &mut std::slice::Iter<'_, String>, flag: &str| it.next().cloned().ok_or_else(|| format!("{flag} needs a value"));
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => o.help = true,
            "-v" | "-V" | "--version" => o.version = true,
            "--connections" => o.connections = true,
            "-i" | "--inline" => o.inline = true,
            "-y" | "--yes" => o.yes = true,
            "--run" => o.run = true,
            "-c" | "--conn" => o.conn = Some(value(&mut it, a)?),
            "-e" | "--execute" => o.sources.push(Source::Sql(value(&mut it, a)?)),
            "--csv" | "--json" | "--tsv" | "-x" | "--expanded" => {
                let f = match a.as_str() {
                    "--csv" => Format::Csv,
                    "--json" => Format::Json,
                    "--tsv" => Format::Tsv,
                    _ => Format::Expanded,
                };
                if o.format.is_some_and(|g| g != f) {
                    return Err("give one of --csv, --json, --tsv and -x".into());
                }
                o.format = Some(f);
            }
            "--limit" => {
                let v = value(&mut it, a)?;
                o.limit = Some(v.parse().map_err(|_| format!("--limit takes a number of rows, not {v:?}"))?);
            }
            "-p" | "--param" => {
                let v = value(&mut it, a)?;
                let (k, val) = v.split_once('=').ok_or_else(|| format!("{a} takes name=value, not {v:?}"))?;
                o.params.insert(k.trim().trim_start_matches(':').to_string(), ParamValue { value: val.to_string(), raw: false });
            }
            // An option is one word: SQL starting with a `--` comment is SQL.
            f if f.starts_with('-') && f.len() > 1 && !f.contains(char::is_whitespace) => return Err(format!("unknown option {f:?}")),
            s if exists(Path::new(s)) => o.sources.push(Source::File(PathBuf::from(s))),
            // `orders.csv` that isn't there is a file not found, not SQL to run.
            // (Said after the rest is read: --help still helps.)
            s if names_a_file(s) => {
                missing.get_or_insert_with(|| not_a_file(s));
            }
            s => o.sources.push(Source::Sql(s.to_string())),
        }
    }
    if o.help || o.version || o.connections {
        return Ok(o);
    }
    if let Some(e) = missing {
        return Err(e);
    }
    if o.sources.is_empty() {
        return Err("nothing to run: give SQL, a .sql file or a data file".into());
    }
    Ok(o)
}

impl Format {
    /// A table or `-x`: read up to 1,000 rows, then printed shaped to the terminal.
    pub fn is_table(self) -> bool {
        matches!(self, Format::Table | Format::Expanded)
    }
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
        Format::Table | Format::Expanded => match limit.unwrap_or(TABLE_DEFAULT) {
            0 => Err(format!("a table shows at most {TABLE_MAX} rows: use --csv, --json or --tsv for every row")),
            n if n > TABLE_MAX => Err(format!("a table shows at most {TABLE_MAX} rows (more is unreadable here): use --csv, --json or --tsv for {n}")),
            n => Ok(Some(n)),
        },
        _ => Ok(limit.filter(|n| *n > 0)),
    }
}

/// The saved connection `name` names: its name in any case (spaces around
/// it ignored) or its id, nothing else. A part of a name or a typo picks
/// none, so a command never runs on a connection that wasn't meant: the
/// error offers the names close to it (`dbr --connections` lists them all).
pub fn find_connection<'a>(all: &'a [ConnectionProfile], name: &str) -> Result<&'a ConnectionProfile, String> {
    let typed = name.trim();
    let low = typed.to_lowercase();
    // The id first (what can't be two), then the name as spelled, then in any case.
    if let Some(c) = all.iter().find(|c| c.id == typed) {
        return Ok(c);
    }
    for same in
        [all.iter().filter(|c| c.name.trim() == typed).collect::<Vec<_>>(), all.iter().filter(|c| c.name.trim().to_lowercase() == low).collect::<Vec<_>>()]
    {
        match same.as_slice() {
            [] => {}
            [one] => return Ok(one),
            many => {
                return Err(format!(
                    "several connections are named {name:?}: give one's id ({})",
                    many.iter().map(|c| format!("{:?} is {}", c.name, c.id)).collect::<Vec<_>>().join(", ")
                ));
            }
        }
    }
    if all.is_empty() {
        return Err(format!("no connection named {name:?}: none is saved (add one in DataBrain)"));
    }
    let close = close_names(all.iter().map(|c| c.name.as_str()), &low);
    let hint = match close.as_slice() {
        [] => String::new(),
        [one] => format!("; did you mean {one:?}?"),
        many => format!("; did you mean one of {}?", many.iter().map(|n| format!("{n:?}")).collect::<Vec<_>>().join(", ")),
    };
    Err(format!("no connection named {name:?}{hint} (dbr --connections lists them)"))
}

/// Up to 5 of `names` close to `typed` (lower case): holding it or held in
/// it, or a few letters off; the closest first.
fn close_names<'a>(names: impl Iterator<Item = &'a str>, typed: &str) -> Vec<&'a str> {
    let mut scored: Vec<(usize, &str)> = names
        .filter_map(|n| {
            let low = n.trim().to_lowercase();
            let d = edit_distance(&low, typed);
            // Held in what was typed only when not a letter or two (`a` is in most words).
            let part = !typed.is_empty() && (low.contains(typed) || (low.chars().count() >= 3 && typed.contains(&low)));
            (part || d <= (typed.chars().count() / 3).max(1)).then_some((if part { 0 } else { d }, n))
        })
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
    scored.into_iter().take(5).map(|(_, n)| n).collect()
}

/// Letters to add, remove, change or swap with the next to turn `a` into
/// `b` (Damerau-Levenshtein, optimal string alignment: `saels` is one from
/// `sales`).
fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    // d[i][j]: the distance between a[..i] and b[..j].
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, v) in d[0].iter_mut().enumerate() {
        *v = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut v = (d[i - 1][j] + 1).min(d[i][j - 1] + 1).min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                v = v.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = v;
        }
    }
    d[a.len()][b.len()]
}

/// Columns `s` takes on screen (East Asian wide characters take two).
fn cols(s: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(s)
}

/// `s` cut to `w` screen columns (the last one `…` when cut), padded to
/// `w` on the left (`right`) or the right.
fn fit(s: &str, w: usize, right: bool) -> String {
    let mut out = String::new();
    let mut used = 0;
    if cols(s) <= w {
        out.push_str(s);
        used = cols(s);
    } else {
        for c in s.chars() {
            let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
            if used + cw + 1 > w {
                break;
            }
            out.push(c);
            used += cw;
        }
        out.push('…');
        used += 1;
    }
    let pad = " ".repeat(w.saturating_sub(used));
    if right { format!("{pad}{out}") } else { format!("{out}{pad}") }
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
        .map(|(i, c)| cells.iter().map(|r| r.get(i).map_or(0, |s| cols(s))).chain([cols(&c.name)]).max().unwrap_or(1).clamp(1, CELL_MAX))
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
    let line =
        |vals: Vec<String>| -> String { vals.iter().enumerate().map(|(i, v)| fit(v, w[i], right[i])).collect::<Vec<_>>().join(" │ ").trim_end().to_string() };
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

/// Each row as a record: a `── row n ──` line, then a line per column
/// (`name │ value`), names padded to the longest, values whole (never
/// cut): a long one wraps under the value column within `width`, its line
/// breaks kept, NULL as `NULL`.
pub fn render_expanded(columns: &[ColumnMeta], rows: &[Vec<Option<String>>], width: usize) -> String {
    let names: Vec<String> = columns.iter().map(|c| flat(&c.name)).collect();
    let name_w = names.iter().map(|n| cols(n)).max().unwrap_or(0);
    // Room left for a value; too little and the terminal wraps it instead.
    let room = width.saturating_sub(name_w + 3);
    let room = if room >= 20 { Some(room) } else { None };
    let mut out = String::new();
    for (n, r) in rows.iter().enumerate() {
        out.push_str(&format!("── row {} ──\n", n + 1));
        for (i, name) in names.iter().enumerate() {
            let v = r.get(i).cloned().flatten().unwrap_or_else(|| "NULL".into());
            let v = v.replace("\r\n", "\n").replace(['\r', '\t'], " ");
            let lines: Vec<String> = v.split('\n').flat_map(|l| wrap(l, room)).collect();
            for (k, l) in lines.iter().enumerate() {
                let label = if k == 0 { fit(name, name_w, false) } else { " ".repeat(name_w) };
                // The value as it is (a space at a wrap is part of it).
                if l.is_empty() {
                    out.push_str(&format!("{label} │"))
                } else {
                    out.push_str(&format!("{label} │ {l}"))
                }
                out.push('\n');
            }
        }
    }
    out
}

/// `line` in pieces of at most `room` screen columns (whole without a room).
fn wrap(line: &str, room: Option<usize>) -> Vec<String> {
    let Some(room) = room else { return vec![line.to_string()] };
    let mut pieces = vec![String::new()];
    let mut used = 0;
    for c in line.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + cw > room && used > 0 {
            pieces.push(String::new());
            used = 0;
        }
        pieces.last_mut().expect("one piece").push(c);
        used += cw;
    }
    pieces
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

/// `s` as a JSON number when one holds it exactly: an integer that fits 64
/// bits, or a decimal whose double reads back as the same value (trailing
/// zeros aside). Others (a DECIMAL(38,19), a HUGEINT, NaN) stay text, so
/// nothing is rounded on the way.
fn exact_number(s: &str) -> Option<serde_json::Value> {
    if let Ok(i) = s.parse::<i64>() {
        return Some(i.into());
    }
    if let Ok(u) = s.parse::<u64>() {
        return Some(u.into());
    }
    let f = s.parse::<f64>().ok().filter(|f| f.is_finite())?;
    let trim = |t: &str| -> String {
        if t.contains('.') && !t.contains(['e', 'E']) { t.trim_end_matches('0').trim_end_matches('.').to_string() } else { t.to_string() }
    };
    let back = serde_json::Number::from_f64(f)?;
    (trim(&back.to_string()) == trim(s.trim_start_matches('+'))).then_some(serde_json::Value::Number(back))
}

/// A JSON value: numbers (held exactly) and booleans as such, NULL as null, the rest strings.
fn json_value(v: Option<&str>, family: TypeFamily) -> serde_json::Value {
    match (v, family) {
        (None, _) => serde_json::Value::Null,
        (Some(s), TypeFamily::Number) => exact_number(s).unwrap_or_else(|| s.into()),
        (Some(s), TypeFamily::Bool) if s.eq_ignore_ascii_case("true") || s.eq_ignore_ascii_case("false") => s.eq_ignore_ascii_case("true").into(),
        (Some(s), _) => s.into(),
    }
}

/// JSON keys for the columns: their names, a repeated one numbered
/// (`id`, `id_2`), so no column is lost from an object.
pub fn json_keys(columns: &[ColumnMeta]) -> Vec<String> {
    let mut keys: Vec<String> = Vec::with_capacity(columns.len());
    for c in columns {
        let mut k = c.name.clone();
        let mut n = 1;
        while keys.contains(&k) {
            n += 1;
            k = format!("{}_{n}", c.name);
        }
        keys.push(k);
    }
    keys
}

/// Rows in a format meant for files and pipes (a header line for CSV and
/// TSV when `header`); `keys` name JSON's fields ([`json_keys`]).
pub fn render_rows(format: Format, columns: &[ColumnMeta], keys: &[String], rows: &[Vec<Option<String>>], header: bool) -> String {
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
                    columns.iter().zip(keys).zip(r).map(|((c, k), v)| (k.clone(), json_value(v.as_deref(), c.family))).collect();
                out.push_str(&serde_json::Value::Object(obj).to_string());
                out.push('\n');
            }
        }
        Format::Table | Format::Expanded => unreachable!("tables go through render_table and render_expanded"),
    }
    out
}

pub fn main(args: Vec<String>) -> ExitCode {
    // `dbr jobs …`: the jobs commands (list, run, runs, stop).
    if args.first().map(String::as_str) == Some("jobs") {
        return crate::cli::jobs::main(args[1..].to_vec());
    }
    // Nothing to run: what dbr does, rather than an error.
    if args.is_empty() {
        println!("{HELP}");
        return ExitCode::SUCCESS;
    }
    // A folder is a data source (Delta, Iceberg, Parquet files), whatever its name.
    let opts = match parse(&args, |p| p.is_file() || p.is_dir()) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("dbr: {e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    if opts.help {
        println!("{HELP}");
        return ExitCode::SUCCESS;
    }
    if opts.version {
        println!("dbr {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    if opts.connections {
        // One a line (a line break in a name shown as a space); nothing
        // when the workspace can't be read here.
        let mut names: Vec<String> = saved_connections(&background::data_dir()).unwrap_or_default().iter().map(|c| flat(&c.name)).collect();
        names.sort_by_key(|n| n.to_lowercase());
        names.dedup();
        for n in names {
            println!("{n}");
        }
        return ExitCode::SUCCESS;
    }
    // -c's connection checked first, before anything connects or the app
    // starts: only that one is looked up.
    if let Some(name) = opts.conn.as_deref()
        && let Some(all) = saved_connections(&background::data_dir())
        && let Err(e) = find_connection(&all, name)
    {
        eprintln!("dbr: {e}");
        return ExitCode::from(2);
    }
    if !opts.inline {
        return open_in_app(&opts);
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

/// What the app is asked to open for `opts` (no `-i`): data files and
/// folders as results, `.sql` files in tabs (run with `--run`), SQL in tabs
/// that run it, files by their full paths (the app runs elsewhere).
/// Printing options need `-i`.
pub fn open_request(opts: &Opts) -> Result<crate::handoff::OpenRequest, String> {
    use crate::handoff::{OpenItem, OpenRequest};
    if opts.format.is_some() || opts.limit.is_some() {
        return Err("--csv, --json, --tsv, -x and --limit are for rows printed here: add -i".into());
    }
    if opts.yes {
        return Err("--yes is for runs here (-i): the app asks itself before running on a production connection".into());
    }
    let mut items = Vec::new();
    for s in &opts.sources {
        items.push(match s {
            Source::Sql(text) => OpenItem::Sql { text: text.clone() },
            Source::File(p) => {
                let full = std::path::absolute(p).map_err(|e| format!("{}: {e}", p.display()))?.to_string_lossy().into_owned();
                if p.is_file() && p.extension().is_some_and(|e| e.eq_ignore_ascii_case("sql")) {
                    OpenItem::SqlFile { path: full }
                } else {
                    OpenItem::Data { path: full }
                }
            }
        });
    }
    Ok(OpenRequest { connection: opts.conn.clone(), items, params: opts.params.clone(), run: opts.run })
}

/// The saved connections, read without connecting to any and without
/// writing to the workspace (the app may have it open): `Some(empty)`
/// before DataBrain first ran, `None` when this dbr can't read it so (a
/// workspace of another DataBrain version): the app (or the run) decides.
fn saved_connections(dir: &Path) -> Option<Vec<ConnectionProfile>> {
    let db = dir.join("workspace.db");
    if !db.is_file() {
        return Some(Vec::new());
    }
    databrain_workspace::Workspace::read_connections(&db)
}

/// Longest wait for a DataBrain started here to answer.
const START_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// Hand `opts` to the running app (started when it isn't): 0 when it opened
/// them, 2 when it refused (an unknown connection, a missing file) or
/// can't be reached.
fn open_in_app(opts: &Opts) -> ExitCode {
    use crate::handoff::{SendError, send, start_app};
    let req = match open_request(opts) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("dbr: {e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let rt = match crate::cli::runtime() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("dbr: {e}");
            return ExitCode::from(2);
        }
    };
    let dir = background::data_dir();
    let reply = rt.block_on(async {
        match send(&dir, &req).await {
            Err(SendError::NotRunning) => {}
            other => return other,
        }
        eprintln!("dbr: starting DataBrain…");
        if let Err(e) = start_app() {
            return Err(SendError::Failed(format!("couldn't start DataBrain: {e}")));
        }
        let until = std::time::Instant::now() + START_WAIT;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            match send(&dir, &req).await {
                Err(SendError::NotRunning) if std::time::Instant::now() < until => continue,
                Err(SendError::NotRunning) => {
                    return Err(SendError::Failed(
                        "DataBrain didn't answer in a minute (a DataBrain from before dbr opened files, still open, doesn't take them: quit it and try again)"
                            .into(),
                    ));
                }
                other => return other,
            }
        }
    });
    match reply {
        Ok(r) => match r.error {
            None => {
                eprintln!("dbr: opened in DataBrain");
                ExitCode::SUCCESS
            }
            Some(e) => {
                eprintln!("dbr: {e}");
                ExitCode::from(2)
            }
        },
        Err(e) => {
            let e = match e {
                SendError::NotRunning => "DataBrain isn't running".to_string(),
                SendError::Failed(e) => e,
            };
            eprintln!("dbr: {e} (-i prints the rows here instead)");
            ExitCode::from(2)
        }
    }
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
    // Ctrl-C: the statement stops on the server, reading and printing stop,
    // a question waiting for an answer is left.
    let cancel = databrain_connector_core::CancellationToken::new();
    {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                cancel.cancel();
            }
        });
    }
    let terminal = Terminal { out: std::io::stdout().is_terminal(), width: terminal_width() };
    let ask = |q: &str| ask_on_terminal(q, std::io::stdin().is_terminal());
    let mut stdout = std::io::stdout().lock();
    let mut stderr = std::io::stderr();
    let code = run_with(&state, opts, Io { out: &mut stdout, err: &mut stderr, ask: &ask, cancel: cancel.clone() }, terminal).await;
    let _ = stdout.flush();
    crate::api::remove_spill_folders(&state.engine);
    ExitCode::from(code)
}

/// Where `dbr` prints: to a terminal (else a pipe or file), how wide.
#[derive(Debug, Clone, Copy)]
pub struct Terminal {
    pub out: bool,
    pub width: usize,
}

/// A question's answer: yes, no, or no terminal to ask in (refused).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Yes,
    No,
    NoTerminal,
}

/// What `dbr` reads and writes: the rows, the messages, a yes/no question
/// (production), and Ctrl-C.
pub struct Io<'a> {
    pub out: &'a mut dyn Write,
    pub err: &'a mut dyn Write,
    pub ask: &'a (dyn Fn(&str) -> Answer + Sync),
    pub cancel: databrain_connector_core::CancellationToken,
}

/// Exit codes.
const DONE: u8 = 0;
const FAILED: u8 = 1;
const BAD_ARGS: u8 = 2;
const STOPPED: u8 = 130;

/// A statement's SQL, from where it came.
struct Job {
    label: String,
    sql: String,
    /// Data files are read by the local DuckDB, whatever `-c` says.
    local: bool,
}

/// Run what `opts` asks on `state`: the rows to `io.out`, messages to
/// `io.err`; the exit code (0 done, 1 a statement failed, 2 bad arguments
/// or a question refused, 130 stopped).
pub async fn run_with(state: &AppState, opts: Opts, io: Io<'_>, terminal: Terminal) -> u8 {
    let Io { out, err, ask, cancel } = io;
    macro_rules! say {
        ($($t:tt)*) => {{ let _ = writeln!(err, $($t)*); }};
    }
    let format = format_for(opts.format, terminal.out);
    let limit = match row_limit(format, opts.limit) {
        Ok(l) => l,
        Err(e) => {
            say!("dbr: {e}");
            return BAD_ARGS;
        }
    };
    // What to run, in order.
    let mut jobs = Vec::new();
    for s in &opts.sources {
        match s {
            Source::Sql(sql) => jobs.push(Job { label: "SQL".into(), sql: sql.clone(), local: false }),
            Source::File(p) if p.is_file() && p.extension().is_some_and(|e| e.eq_ignore_ascii_case("sql")) => match std::fs::read_to_string(p) {
                Ok(sql) => jobs.push(Job { label: p.display().to_string(), sql, local: false }),
                Err(e) => {
                    say!("dbr: cannot read {}: {e}", p.display());
                    return BAD_ARGS;
                }
            },
            // A data file or folder (Delta, Iceberg, Parquet): by its full
            // path, so the SQL kept in the history runs again from anywhere.
            Source::File(p) => match std::path::absolute(p).map_err(|e| e.to_string()).and_then(|full| file_sql(&full)) {
                Ok(sql) => jobs.push(Job { label: p.display().to_string(), sql, local: true }),
                Err(e) => {
                    say!("dbr: {}: {e}", p.display());
                    return BAD_ARGS;
                }
            },
        }
    }
    let all = state.workspace.list_connections().unwrap_or_default();
    let named = match opts.conn.as_deref().map(|n| find_connection(&all, n)) {
        Some(Ok(c)) => Some(c.clone()),
        Some(Err(e)) => {
            say!("dbr: {e}");
            return BAD_ARGS;
        }
        None => None,
    };
    // The app's "Local files (DuckDB)" connection (not a user's own
    // in-memory DuckDB), else Results.
    let local = || -> Result<ConnectionProfile, EngineError> {
        match all.iter().find(|c| c.name == LOCAL_FILES && c.config.kind == databrain_connector_core::ConnectorKind::Duckdb && c.config.file_path.is_none()) {
            Some(c) => Ok(c.clone()),
            None => state.engine.results_connection(),
        }
    };
    let mut failed = false;
    let (mut opened, mut conn_down) = (false, false);
    for job in &jobs {
        let conn = match (&named, job.local) {
            (Some(c), false) => c.clone(),
            _ => match local() {
                Ok(c) => c,
                Err(e) => {
                    say!("dbr: {}", e.message);
                    return BAD_ARGS;
                }
            },
        };
        // A `:name` with no value is a bad command line, found before anything runs.
        let missing: Vec<String> = databrain_connector_core::sql::find_parameters(&job.sql, conn.config.kind)
            .into_iter()
            .map(|p| p.name)
            .filter(|n| !opts.params.contains_key(n))
            .fold(Vec::new(), |mut v, n| {
                if !v.contains(&n) {
                    v.push(n);
                }
                v
            });
        if !missing.is_empty() {
            say!("dbr: give a value for {} with -p name=value", missing.iter().map(|n| format!(":{n}")).collect::<Vec<_>>().join(", "));
            return BAD_ARGS;
        }
        // CSV, JSON and TSV are printed as the rows are read (a pipe gets the
        // first ones while the query still runs): the tab's rows are tapped.
        let tab_id = format!("cli:{}", uuid::Uuid::new_v4().simple());
        let (_tap, mut rows_rx) = if format.is_table() {
            (None, None)
        } else {
            let (tap, rx) = state.engine.tap_rows(&tab_id);
            (Some(tap), Some(rx))
        };
        // The named connection is opened (signed in) before its first
        // statement, so an error there is the connection's; the run reuses
        // the session. Once it can't be reached, the rest on it is skipped
        // (said once); data files go on, on the local DuckDB.
        if !job.local && named.is_some() {
            if conn_down {
                continue;
            }
            if !opened {
                let open = tokio::select! {
                    r = state.engine.open_session(&conn.id, &tab_id, Origin::Cli) => r,
                    _ = cancel.cancelled() => {
                        say!("dbr: stopped");
                        return STOPPED;
                    }
                };
                if let Err(e) = open {
                    say!("dbr: couldn't connect to \"{}\": {}", conn.name, e.message);
                    conn_down = true;
                    failed = true;
                    continue;
                }
                opened = true;
            }
        }
        let mut printer = Printer { format, index: None, columns: Vec::new(), keys: Vec::new(), header: true, broken: false };
        let req = |confirmed: bool| RunRequest {
            connection_id: conn.id.clone(),
            tab_id: tab_id.clone(),
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
        let mut res = with_rows(state.engine.run_and_wait(&state.hub, req(false), Some(cancel.clone())), rows_rx.as_mut(), |b| {
            if !printer.print(out, &b) {
                // The reader went away (`| head`): stop the query.
                cancel.cancel();
            }
        })
        .await;
        // Production (or a write the connection asks about): asked here, or --yes.
        if let Err(e) = &res
            && e.code.as_deref() == Some("needs_confirmation")
        {
            let answer = if opts.yes { Answer::Yes } else { asked(ask, &format!("{}\nRun it on \"{}\"? [y/N] ", e.message, conn.name), &cancel).await };
            if cancel.is_cancelled() {
                say!("dbr: stopped");
                return STOPPED;
            }
            if answer != Answer::Yes {
                say!(
                    "dbr: not run on \"{}\": {}{}",
                    conn.name,
                    e.message,
                    if answer == Answer::NoTerminal { " (no terminal to ask in: --yes runs it)" } else { "" }
                );
                return BAD_ARGS;
            }
            res = with_rows(state.engine.run_and_wait(&state.hub, req(true), Some(cancel.clone())), rows_rx.as_mut(), |b| {
                if !printer.print(out, &b) {
                    cancel.cancel();
                }
            })
            .await;
        }
        // The reader went away: nothing more to print, and nothing went wrong.
        if printer.broken {
            return if failed { FAILED } else { DONE };
        }
        if cancel.is_cancelled() {
            say!("dbr: stopped");
            return STOPPED;
        }
        let outcomes = match res {
            Ok(o) => o,
            Err(e) => {
                say!("dbr: {}: {}", job.label, e.message);
                failed = true;
                continue;
            }
        };
        let n = outcomes.len();
        for o in &outcomes {
            if n > 1 && format.is_table() {
                let _ = writeln!(out, "-- statement {} of {n}", o.index + 1);
            }
            if let Some(e) = &o.error {
                say!("dbr: {}statement {}: {}", if jobs.len() > 1 { format!("{}: ", job.label) } else { String::new() }, o.index + 1, e.message);
                failed = true;
                continue;
            }
            let Some(r) = &o.result else {
                // Said apart from the rows, so a pipe gets only data.
                let affected = o.rows_affected.map(|r| format!("{r} rows affected")).unwrap_or_else(|| "done".into());
                if format.is_table() {
                    let _ = writeln!(out, "{affected} · {} · {} ms", conn.name, o.duration_ms);
                } else {
                    say!("{affected}");
                }
                continue;
            };
            let read = if format.is_table() {
                // Up to 1,000 rows: read, then fitted to the terminal.
                let mut rows = Vec::new();
                let r2 = for_each_page(state, &r.id, r.total_rows, &cancel, |page| rows.extend_from_slice(page)).await;
                if r2.is_ok() {
                    let shown = if format == Format::Expanded {
                        render_expanded(&r.columns, &rows, terminal.width)
                    } else {
                        render_table(&r.columns, &rows, terminal.width)
                    };
                    let _ = out.write_all(shown.as_bytes());
                    let more = if r.truncated { format!(" (more not read: --limit up to {TABLE_MAX}, or --csv for every row)") } else { String::new() };
                    let _ = writeln!(out, "{} row{}{more} · {} · {} ms", rows.len(), if rows.len() == 1 { "" } else { "s" }, conn.name, o.duration_ms);
                }
                r2
            } else {
                // Printed while the query ran (the tap).
                let _ = r;
                Ok(())
            };
            match read {
                Ok(()) => {}
                Err(e) if cancel.is_cancelled() || e.kind == "cancelled" => {
                    say!("dbr: stopped");
                    return STOPPED;
                }
                Err(e) => {
                    say!("dbr: {}", e.message);
                    failed = true;
                }
            }
        }
    }
    let _ = out.flush();
    if failed { FAILED } else { DONE }
}

/// Run `run`, handing each batch from `rows` (when tapped) to `each` as it
/// comes; the batches still queued when the run ends are handed on too.
async fn with_rows<R>(
    run: impl std::future::Future<Output = R>,
    rows: Option<&mut tokio::sync::mpsc::Receiver<databrain_query_engine::TappedBatch>>,
    mut each: impl FnMut(databrain_query_engine::TappedBatch),
) -> R {
    let Some(rows) = rows else { return run.await };
    tokio::pin!(run);
    loop {
        tokio::select! {
            biased;
            Some(b) = rows.recv() => each(b),
            r = &mut run => {
                while let Ok(b) = rows.try_recv() {
                    each(b);
                }
                return r;
            }
        }
    }
}

/// Prints tapped rows as CSV, JSON or TSV: a header (CSV, TSV) at each
/// statement's first rows.
struct Printer {
    format: Format,
    /// The statement whose rows came last.
    index: Option<usize>,
    columns: Vec<ColumnMeta>,
    keys: Vec<String>,
    header: bool,
    /// The reader went away: nothing more is printed.
    broken: bool,
}

impl Printer {
    /// Print `b`; false when the reader went away.
    fn print(&mut self, out: &mut dyn Write, b: &databrain_query_engine::TappedBatch) -> bool {
        if self.broken {
            return false;
        }
        if self.index != Some(b.index) {
            self.index = Some(b.index);
            self.columns = b.batch.schema().fields().iter().map(|f| databrain_result_store::column_meta(f)).collect();
            self.keys = json_keys(&self.columns);
            self.header = true;
        }
        let text = render_rows(self.format, &self.columns, &self.keys, &batch_rows(&b.batch), self.header);
        self.header = false;
        if out.write_all(text.as_bytes()).and_then(|()| out.flush()).is_err() {
            self.broken = true;
        }
        !self.broken
    }
}

/// A batch's cells as the grid shows them (NULL as `None`).
fn batch_rows(batch: &databrain_connector_core::arrow::record_batch::RecordBatch) -> Vec<Vec<Option<String>>> {
    use databrain_connector_core::arrow::util::display::ArrayFormatter;
    let opts = databrain_result_store::display::format_options();
    let Ok(formatters) = batch.columns().iter().map(|c| ArrayFormatter::try_new(c.as_ref(), &opts)).collect::<Result<Vec<_>, _>>() else {
        return Vec::new();
    };
    (0..batch.num_rows()).map(|r| formatters.iter().zip(batch.columns()).map(|(f, a)| databrain_result_store::display::cell(f, a, r)).collect()).collect()
}

/// The name of the DuckDB connection the app reads local files with.
const LOCAL_FILES: &str = "Local files (DuckDB)";

/// Each page of a stored result (5,000 rows), in order, until Ctrl-C.
async fn for_each_page(
    state: &AppState,
    id: &str,
    total: usize,
    cancel: &databrain_connector_core::CancellationToken,
    mut each: impl FnMut(&[Vec<Option<String>>]),
) -> Result<(), EngineError> {
    let mut at = 0;
    while at < total {
        if cancel.is_cancelled() {
            return Err(EngineError::new("cancelled", "stopped"));
        }
        let page = crate::api::fetch_page(state, id.to_string(), ViewSpec::default(), at, 5_000).await?;
        if page.rows.is_empty() {
            break;
        }
        at += page.rows.len();
        each(&page.rows);
    }
    Ok(())
}

/// Ask `question`, unless Ctrl-C comes first.
async fn asked(ask: &(dyn Fn(&str) -> Answer + Sync), question: &str, cancel: &databrain_connector_core::CancellationToken) -> Answer {
    // The question reads a line (blocking): off the runtime, raced with Ctrl-C.
    let q = question.to_string();
    let ask_ptr: &(dyn Fn(&str) -> Answer + Sync) = ask;
    tokio::select! {
        a = async { tokio::task::block_in_place(|| ask_ptr(&q)) } => a,
        _ = cancel.cancelled() => Answer::No,
    }
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

/// Ask a yes/no question on the terminal (no means no); none: refused.
fn ask_on_terminal(question: &str, terminal: bool) -> Answer {
    if !terminal {
        return Answer::NoTerminal;
    }
    eprint!("{question}");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_ok() && matches!(line.trim().to_lowercase().as_str(), "y" | "yes") { Answer::Yes } else { Answer::No }
}

/// The terminal's width in columns: `COLUMNS` when set, else what the
/// terminal says (stdout, then stderr), else 120.
pub fn terminal_width() -> usize {
    if let Some(c) = std::env::var("COLUMNS").ok().and_then(|c| c.trim().parse::<usize>().ok()).filter(|c| *c > 0) {
        return c;
    }
    os_width().filter(|w| *w > 0).unwrap_or(120)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn os_width() -> Option<usize> {
    for fd in [libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        let mut ws: libc::winsize = unsafe_zeroed_winsize();
        // SAFETY: TIOCGWINSZ writes a winsize into the struct it is given
        // and reads nothing else; a non-terminal fd makes it fail (checked).
        #[allow(unsafe_code)]
        let ok = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } == 0;
        if ok && ws.ws_col > 0 {
            return Some(ws.ws_col as usize);
        }
    }
    None
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn unsafe_zeroed_winsize() -> libc::winsize {
    libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 }
}

#[cfg(windows)]
fn os_width() -> Option<usize> {
    use windows_sys::Win32::System::Console::{CONSOLE_SCREEN_BUFFER_INFO, GetConsoleScreenBufferInfo, GetStdHandle, STD_OUTPUT_HANDLE};
    // SAFETY: the handle is the process's own stdout; the call fills the
    // struct it is given (zeroed first) and fails when it isn't a console.
    #[allow(unsafe_code)]
    unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
        if GetConsoleScreenBufferInfo(h, &mut info) != 0 {
            let w = i32::from(info.srWindow.Right) - i32::from(info.srWindow.Left) + 1;
            return usize::try_from(w).ok();
        }
    }
    None
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn os_width() -> Option<usize> {
    None
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
        assert!(parse(&args(&["-z", "x"]), files).unwrap_err().contains("unknown option"));
        // -x: records; one format only (the same one twice is fine).
        assert_eq!(parse(&args(&["-i", "-x", "select 1"]), files).unwrap().format, Some(Format::Expanded));
        assert_eq!(parse(&args(&["-i", "--expanded", "-x", "select 1"]), files).unwrap().format, Some(Format::Expanded));
        for a in [&["-x", "--csv"][..], &["--json", "-x"], &["--csv", "--tsv"]] {
            let a: Vec<&str> = ["-i"].iter().chain(a).chain(&["select 1"]).copied().collect();
            assert!(parse(&args(&a), files).unwrap_err().contains("one of"), "{a:?}");
        }
        assert_eq!(row_limit(Format::Expanded, None), Ok(Some(TABLE_DEFAULT)));
        assert!(row_limit(Format::Expanded, Some(TABLE_MAX + 1)).is_err());
        // A file that isn't there is said so, with and without -i: never run as SQL.
        let none = |_: &Path| false;
        for f in [
            "test.csv",
            "test.TSV",
            "a.json",
            "a.jsonl",
            "a.ndjson",
            "a.parquet",
            "a.pq",
            "a.psv",
            "a.csv.gz",
            "a.json.zst",
            "a.xlsx",
            "a.xls",
            "a.txt",
            "q.sql",
            "data/x",
            "./events",
            "~/a.csv",
            r"C:\d\a.csv",
        ] {
            for a in [&[f][..], &["-i", f], &["-c", "sales", "-i", "--csv", f]] {
                let e = parse(&args(a), none).unwrap_err();
                assert!(e.starts_with(&format!("{f}: no such file")) && e.contains("-e"), "{a:?}: {e}");
            }
        }
        // Names with spaces, `=`, `,` or brackets (Finder, Hive folders, copies) too.
        for f in ["my file.csv", "data/year=2024/p.parquet", "report(1).csv", "sales,2024.csv", "Q1 Sales.XLSX"] {
            let e = parse(&args(&["-i", f]), none).unwrap_err();
            assert!(e.starts_with(&format!("{f}: no such file")), "{f}: {e}");
        }
        // Where it looked: the current folder for a relative name, not for a full one.
        let cwd = std::env::current_dir().unwrap();
        assert!(parse(&args(&["a.csv"]), none).unwrap_err().contains(&cwd.display().to_string()));
        let full = cwd.join("gone.csv").to_string_lossy().into_owned();
        assert_eq!(parse(&args(&[&full]), none).unwrap_err(), format!("{full}: no such file (-e before it runs it as SQL)"));
        assert!(parse(&args(&["~/a.csv"]), none).unwrap_err().contains("quoted ~"));
        // A URL isn't a file here; something that is there but no file or folder says so.
        let e = parse(&args(&["https://example.com/x.csv"]), none).unwrap_err();
        assert!(e.contains("files on this computer") && !e.contains("no such file"), "{e}");
        #[cfg(unix)]
        assert!(parse(&args(&["/dev/null"]), |p: &Path| p.is_file() || p.is_dir()).unwrap_err().contains("not a file or folder"));
        // --help and --version still answer.
        assert!(parse(&args(&["gone.csv", "--help"]), none).unwrap().help);
        assert!(parse(&args(&["gone.csv", "--version"]), none).unwrap().version);
        // SQL is SQL, a word with a dot in it too, and a statement ending in a file-like name.
        for s in [
            "select 1",
            "select * from 'a.csv'",
            "show",
            "select 1/2",
            "from 'x.parquet'",
            "read_csv('a.csv')",
            "t.id",
            "select * from s.csv",
            "FROM s.csv",
            "describe t.json",
            r"\dt",
            "select 1;",
        ] {
            assert_eq!(parse(&args(&[s]), none).unwrap().sources, [Source::Sql(s.into())], "{s}");
        }
        assert_eq!(parse(&args(&["-e", "a.csv"]), none).unwrap().sources, [Source::Sql("a.csv".into())]);
        // SQL that starts with a `--` comment is SQL, not an option.
        assert_eq!(parse(&args(&["-i", "--x\nselect\n1"]), files).unwrap().sources, [Source::Sql("--x\nselect\n1".into())]);
        assert!(parse(&args(&["--connections"]), files).unwrap().connections);
        assert!(parse(&args(&["--help"]), files).unwrap().help);
        assert!(parse(&args(&["-h"]), files).unwrap().help);
        assert!(parse(&args(&["--version"]), files).unwrap().version);
        assert!(parse(&args(&["-v"]), files).unwrap().version);
    }

    #[test]
    fn without_i_the_app_opens_them() {
        use crate::handoff::OpenItem;
        let dir = tempfile::tempdir().unwrap();
        let sql = dir.path().join("report.SQL");
        let csv = dir.path().join("orders.csv");
        std::fs::write(&sql, "select 1").unwrap();
        std::fs::write(&csv, "a\n1\n").unwrap();
        let s = |p: &Path| p.to_string_lossy().into_owned();
        let o = parse(&args(&["-c", "sales", &s(&sql), &s(&csv), "select 2", "-p", "day=1", "--run"]), |p| p.exists()).unwrap();
        let r = open_request(&o).unwrap();
        assert_eq!(r.connection.as_deref(), Some("sales"));
        assert!(r.run);
        assert_eq!(r.items, [OpenItem::SqlFile { path: s(&sql) }, OpenItem::Data { path: s(&csv) }, OpenItem::Sql { text: "select 2".into() }]);
        assert_eq!(r.params["day"].value, "1");
        // A folder (Delta, Iceberg) is data; a relative path is sent in full (the app runs elsewhere).
        let o = parse(&args(&[&s(dir.path()), "x.csv"]), |p| p.is_dir() || p.ends_with("x.csv")).unwrap();
        let r = open_request(&o).unwrap();
        assert_eq!(r.items[0], OpenItem::Data { path: s(dir.path()) });
        assert!(matches!(&r.items[1], OpenItem::Data { path } if Path::new(path).is_absolute() && path.ends_with("x.csv")));
        // Each format dbr reads goes to the app as a file (a `.sql` file in its
        // tab), typed relative to the current folder, sent by its full path.
        let here = tempfile::tempdir().unwrap();
        // `p` relative to the current folder, as a user types it.
        let rel = |n: &str| {
            let p = here.path().join(n);
            let mut base = std::env::current_dir().unwrap();
            let mut up = PathBuf::new();
            loop {
                if let Ok(rest) = p.strip_prefix(&base) {
                    return up.join(rest).to_string_lossy().into_owned();
                }
                if !base.pop() {
                    return p.to_string_lossy().into_owned();
                }
                up.push("..");
            }
        };
        let names = [
            "t.csv",
            "t.tsv",
            "t.psv",
            "t.txt",
            "t.json",
            "t.jsonl",
            "t.ndjson",
            "t.parquet",
            "t.pq",
            "t.xlsx",
            "t.csv.gz",
            "t.json.zst",
            "U.TSV.GZ",
            "my file.csv",
            "events",
            "q.sql",
            "Q2.SQL",
        ];
        for n in names {
            if n == "events" {
                std::fs::create_dir(here.path().join(n)).unwrap();
            } else {
                std::fs::write(here.path().join(n), "a\n1\n").unwrap();
            }
        }
        let typed: Vec<String> = names.iter().map(|n| rel(n)).collect();
        #[cfg(unix)]
        assert!(typed.iter().all(|t| Path::new(t).is_relative()), "{typed:?}");
        let r = open_request(&parse(&typed, |p| p.is_file() || p.is_dir()).unwrap()).unwrap();
        assert_eq!(r.items.len(), names.len());
        let real = here.path().canonicalize().unwrap();
        for (item, n) in r.items.iter().zip(names) {
            let (OpenItem::Data { path } | OpenItem::SqlFile { path }) = item else { panic!("{n}: {item:?}") };
            assert!(Path::new(path).is_absolute() && Path::new(path).canonicalize().unwrap() == real.join(n), "{n}: {path}");
            assert_eq!(matches!(item, OpenItem::SqlFile { .. }), n.to_lowercase().ends_with(".sql"), "{n}: {item:?}");
        }
        // Printing options are for -i; --yes too (the app asks itself).
        for a in [&["--csv", "x"][..], &["-x", "x"], &["--limit", "5", "x"], &["-y", "x"]] {
            let e = open_request(&parse(&args(a), |_| false).unwrap()).unwrap_err();
            assert!(e.contains("-i"), "{a:?}: {e}");
        }
    }

    #[test]
    fn the_app_finds_the_connection_and_the_files() {
        use crate::handoff::{OpenItem, OpenRequest, resolve};
        let p = |id: &str, name: &str| ConnectionProfile {
            id: id.into(),
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
        let all = [p("c1", "Sales PG"), p("c2", "Ops")];
        let dir = tempfile::tempdir().unwrap();
        let full = dir.path().to_string_lossy().into_owned();
        let req =
            |conn: Option<&str>, items: Vec<OpenItem>| OpenRequest { connection: conn.map(str::to_string), items, params: Default::default(), run: false };
        let ok = resolve(&all, req(Some("sales pg"), vec![OpenItem::Data { path: full.clone() }, OpenItem::Sql { text: "select 1".into() }])).unwrap();
        assert_eq!(ok.connection_id.as_deref(), Some("c1"));
        assert_eq!(resolve(&all, req(None, vec![OpenItem::Sql { text: "select 1".into() }])).unwrap().connection_id, None);
        assert!(resolve(&all, req(Some("nope"), vec![OpenItem::Sql { text: "x".into() }])).unwrap_err().contains("no connection named"));
        assert!(resolve(&all, req(None, vec![OpenItem::Data { path: "rel.csv".into() }])).unwrap_err().contains("not a full path"));
        let gone = dir.path().join("gone.csv").to_string_lossy().into_owned();
        assert!(resolve(&all, req(None, vec![OpenItem::SqlFile { path: gone }])).unwrap_err().contains("no such file"));
        assert!(resolve(&all, req(None, vec![OpenItem::Sql { text: " ".into() }])).is_err());
        assert!(resolve(&all, req(None, vec![])).is_err());
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
        let all = vec![p("Sales PG"), p("Sales MySQL"), p("Warehouse"), p("Archive PG")];
        // The name in any case, spaces around it ignored, or the id.
        assert_eq!(find_connection(&all, "sales pg").unwrap().name, "Sales PG");
        assert_eq!(find_connection(&all, " SALES PG ").unwrap().name, "Sales PG");
        assert_eq!(find_connection(&all, "id-Warehouse").unwrap().name, "Warehouse");
        // A part of a name or a typo picks nothing (it once picked the only
        // connection holding it): the close names are offered instead.
        assert_eq!(find_connection(&all, "ware").unwrap_err(), "no connection named \"ware\"; did you mean \"Warehouse\"? (dbr --connections lists them)");
        assert_eq!(
            find_connection(&all, "warehose").unwrap_err(),
            "no connection named \"warehose\"; did you mean \"Warehouse\"? (dbr --connections lists them)"
        );
        let e = find_connection(&all, "sales").unwrap_err();
        assert!(e.contains("one of \"Sales MySQL\", \"Sales PG\"") && !e.contains("Archive"), "{e}");
        let e = find_connection(&all, "pg").unwrap_err();
        assert!(e.contains("\"Archive PG\", \"Sales PG\""), "{e}");
        assert_eq!(find_connection(&all, "crm").unwrap_err(), "no connection named \"crm\" (dbr --connections lists them)");
        assert!(find_connection(&[], "x").unwrap_err().contains("none is saved"));
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(edit_distance("", "ab"), 2);
        assert_eq!(edit_distance("saels", "sales"), 1);
        // The same name in two spellings: the exact one wins, any other is
        // refused with their ids (never one picked by the list's order).
        let mut two = vec![p("Prod"), p("prod"), p("Prod2")];
        two[2].id = "prod".into(); // an id that is another's name: the id wins
        assert_eq!(find_connection(&two, "prod").unwrap().name, "Prod2");
        two[2].id = "id-other".into();
        assert_eq!(find_connection(&two, "prod").unwrap().id, "id-prod");
        assert_eq!(find_connection(&two, "Prod").unwrap().id, "id-Prod");
        let e = find_connection(&two, "PROD").unwrap_err();
        assert!(e.contains("several connections are named \"PROD\"") && e.contains("id-Prod") && e.contains("id-prod"), "{e}");
        let same = vec![p("Prod"), ConnectionProfile { id: "id-2".into(), ..p("Prod") }];
        assert!(find_connection(&same, "Prod").unwrap_err().contains("id-2"));
        assert_eq!(find_connection(&same, " id-2 ").unwrap().id, "id-2");
        // A one-letter name isn't offered for everything holding the letter.
        let short = vec![p("a"), p("Analytics")];
        assert_eq!(
            find_connection(&short, "analytic").unwrap_err(),
            "no connection named \"analytic\"; did you mean \"Analytics\"? (dbr --connections lists them)"
        );
        assert_eq!(
            find_connection(&all, "Wraehouse").unwrap_err(),
            "no connection named \"Wraehouse\"; did you mean \"Warehouse\"? (dbr --connections lists them)"
        );
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
        assert!(narrow.lines().all(|l| super::cols(l) <= 30), "{narrow}");
        // Wide characters take two columns: the bars stay in line.
        let cjk = render_table(
            &[col("名前", TypeFamily::Text), col("n", TypeFamily::Number)],
            &[vec![Some("東京".into()), Some("1".into())], vec![Some("ab".into()), Some("22".into())]],
            80,
        );
        let bars: Vec<usize> = cjk.lines().filter(|l| l.contains('│')).map(|l| super::cols(l.split('│').next().unwrap())).collect();
        assert!(bars.windows(2).all(|w| w[0] == w[1]), "{cjk}");
    }

    #[test]
    fn expanded_rows_show_whole_values() {
        let cols = [col("id", TypeFamily::Number), col("comment", TypeFamily::Text), col("note", TypeFamily::Text)];
        let long = "word ".repeat(30).trim_end().to_string(); // 149 characters
        let rows =
            vec![vec![Some("1".into()), Some(long.clone()), None], vec![Some("2".into()), Some("first line\nsecond\tline".into()), Some("日本語".into())]];
        let x = render_expanded(&cols, &rows, 60);
        let lines: Vec<&str> = x.lines().collect();
        assert_eq!(lines[0], "── row 1 ──");
        assert_eq!(lines[1], "id      │ 1");
        // Wrapped under the value column, nothing cut, nothing lost.
        let comment: Vec<&str> = lines.iter().skip(2).take_while(|l| !l.starts_with("note")).copied().collect();
        assert!(comment.len() > 1 && comment[0].starts_with("comment │ ") && comment[1..].iter().all(|l| l.starts_with("        │ ")), "{x}");
        assert!(comment.iter().all(|l| cols_of(l) <= 60), "{x}");
        let joined: String = comment.iter().map(|l| &l[l.find('│').unwrap() + "│ ".len()..]).collect();
        assert_eq!(joined, long);
        assert!(!x.contains('…'));
        assert!(lines.contains(&"note    │ NULL"));
        // Line breaks kept (a tab shown as a space), wide characters whole.
        let i = lines.iter().position(|l| *l == "── row 2 ──").unwrap();
        assert_eq!(&lines[i + 1..], ["id      │ 2", "comment │ first line", "        │ second line", "note    │ 日本語"]);
        // Too narrow to wrap well: the value is left for the terminal to wrap.
        let narrow = render_expanded(&cols, &rows[..1], 20);
        assert!(narrow.contains(&format!("comment │ {long}")), "{narrow}");
        assert_eq!(render_expanded(&cols, &[], 80), "");
    }

    fn cols_of(s: &str) -> usize {
        unicode_width::UnicodeWidthStr::width(s)
    }

    #[test]
    fn formats_for_files_and_pipes() {
        let cols = [col("id", TypeFamily::Number), col("name", TypeFamily::Text), col("ok", TypeFamily::Bool)];
        let rows = vec![vec![Some("1".into()), Some("a, \"b\"".into()), Some("true".into())], vec![Some("2.5".into()), None, Some("false".into())]];
        let keys = json_keys(&cols);
        assert_eq!(render_rows(Format::Csv, &cols, &keys, &rows, true), "id,name,ok\n1,\"a, \"\"b\"\"\",true\n2.5,,false\n");
        assert_eq!(render_rows(Format::Tsv, &cols, &keys, &rows, true), "id\tname\tok\n1\ta, \"b\"\ttrue\n2.5\t\\N\tfalse\n");
        assert_eq!(
            render_rows(Format::Json, &cols, &keys, &rows, true),
            "{\"id\":1,\"name\":\"a, \\\"b\\\"\",\"ok\":true}\n{\"id\":2.5,\"name\":null,\"ok\":false}\n"
        );
        // A page after the first: no header again.
        assert_eq!(render_rows(Format::Csv, &cols, &keys, &rows[1..], false), "2.5,,false\n");
        // Two columns of one name: both kept.
        let twice = [col("id", TypeFamily::Number), col("id", TypeFamily::Number)];
        assert_eq!(render_rows(Format::Json, &twice, &json_keys(&twice), &[vec![Some("1".into()), Some("2".into())]], true), "{\"id\":1,\"id_2\":2}\n");
    }

    #[test]
    fn json_numbers_are_never_rounded() {
        let n = |s: &str| json_value(Some(s), TypeFamily::Number);
        assert_eq!(n("42"), serde_json::json!(42));
        assert_eq!(n("-9223372036854775808"), serde_json::json!(i64::MIN));
        assert_eq!(n("18446744073709551615"), serde_json::json!(u64::MAX));
        assert_eq!(n("120.50"), serde_json::json!(120.5), "trailing zeros aside");
        assert_eq!(n("2.5"), serde_json::json!(2.5));
        // What a double can't hold exactly stays text.
        assert_eq!(n("0.1000000000000000055"), serde_json::json!("0.1000000000000000055"));
        assert_eq!(n("123456789012345678901234"), serde_json::json!("123456789012345678901234"));
        assert_eq!(n("NaN"), serde_json::json!("NaN"));
    }
}
