//! SQL to copy rows into a table of another database: `CREATE TABLE` from
//! the rows' Arrow types and batched `INSERT` statements, per dialect.

use databrain_connector_core::arrow::array::{Array, RecordBatch};
use databrain_connector_core::arrow::datatypes::{DataType, Field, SchemaRef};
use databrain_connector_core::arrow::util::display::ArrayFormatter;
use databrain_connector_core::traits::{CreateIfMissing, LoadTraits, MergeStyle, StageTable, TextLiteral};
use databrain_connector_core::{ConnectorKind, quote_ident, quote_literal, quote_path};
use databrain_result_store::display::{format_options, is_decimal_text};

use crate::Result;

const KB: usize = 1024;

/// How big one INSERT of a load may be: rows in its VALUES list and bytes
/// of SQL text (whichever is reached first ends the statement).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, ts_rs::TS)]
#[ts(export)]
pub struct BatchSize {
    pub rows: usize,
    pub bytes: usize,
}

/// Batch sizes a dialect allows, and what DataBrain uses by default.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, ts_rs::TS)]
#[ts(export)]
pub struct BatchLimits {
    pub default_rows: usize,
    pub max_rows: usize,
    pub default_bytes: usize,
    pub max_bytes: usize,
    /// Where the row maximum comes from.
    pub rows_note: &'static str,
    /// Where the byte maximum comes from.
    pub bytes_note: &'static str,
}

/// How rows are written into `dialect`. Every SQL load target has these;
/// a source without them (`traits().load == None`) is refused before any
/// SQL is built (`is_load_target`), so this falls back to standard SQL.
fn lt(dialect: ConnectorKind) -> &'static LoadTraits {
    static STANDARD: std::sync::OnceLock<LoadTraits> = std::sync::OnceLock::new();
    dialect.traits().load.as_ref().unwrap_or_else(|| STANDARD.get_or_init(|| databrain_connector_core::traits::POSTGRES.load.unwrap()))
}

/// Whether rows can be loaded into this kind of database.
pub fn is_load_target(dialect: ConnectorKind) -> bool {
    dialect.traits().load.is_some()
}

/// Batch limits of `dialect`.
pub fn batch_limits(dialect: ConnectorKind) -> BatchLimits {
    let l = lt(dialect);
    BatchLimits {
        default_rows: l.default_rows,
        max_rows: l.max_rows,
        default_bytes: l.default_bytes,
        max_bytes: l.max_bytes,
        rows_note: l.rows_note,
        bytes_note: l.bytes_note,
    }
}

impl BatchLimits {
    /// The batch size for the user's settings (`None` = default), kept
    /// within the maximums; a note for each value that was lowered.
    pub fn resolve(&self, rows: Option<u32>, kb: Option<u32>) -> (BatchSize, Vec<String>) {
        let mut notes = Vec::new();
        let rows = match rows.filter(|r| *r > 0).map(|r| r as usize) {
            None => self.default_rows,
            Some(r) if r > self.max_rows => {
                notes.push(format!("Rows per INSERT lowered from {r} to {} ({})", self.max_rows, self.rows_note));
                self.max_rows
            }
            Some(r) => r,
        };
        let bytes = match kb.filter(|k| *k > 0).map(|k| k as usize * KB) {
            None => self.default_bytes,
            Some(b) if b > self.max_bytes => {
                notes.push(format!("INSERT size lowered from {} KB to {} KB ({})", b / KB, self.max_bytes / KB, self.bytes_note));
                self.max_bytes
            }
            Some(b) => b,
        };
        (BatchSize { rows, bytes }, notes)
    }

    pub fn default_size(&self) -> BatchSize {
        BatchSize { rows: self.default_rows, bytes: self.default_bytes }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Lit {
    Number,
    /// A float column (NaN and ±inf are values of its type).
    Float,
    Bool,
    Binary,
    Date,
    Timestamp,
    /// With a time zone (`… +02:00`).
    TimestampTz,
    Text,
}

pub(crate) fn lit_kind(f: &Field) -> Lit {
    match f.data_type() {
        DataType::Boolean => Lit::Bool,
        DataType::Float16 | DataType::Float32 | DataType::Float64 => Lit::Float,
        t if t.is_numeric() => Lit::Number,
        DataType::Binary | DataType::LargeBinary | DataType::FixedSizeBinary(_) => Lit::Binary,
        DataType::Date32 | DataType::Date64 => Lit::Date,
        DataType::Timestamp(_, None) => Lit::Timestamp,
        DataType::Timestamp(_, Some(_)) => Lit::TimestampTz,
        _ if is_decimal_text(f) => Lit::Number,
        _ => Lit::Text,
    }
}

/// Column type for a `CREATE TABLE` in `dialect`.
pub fn column_type(dialect: ConnectorKind, f: &Field) -> &'static str {
    let t = &lt(dialect).types;
    if is_decimal_text(f) {
        return t.decimal;
    }
    match f.data_type() {
        DataType::Boolean => t.boolean,
        DataType::Int8 | DataType::Int16 | DataType::Int32 | DataType::Int64 | DataType::UInt8 | DataType::UInt16 | DataType::UInt32 | DataType::UInt64 => {
            t.integer
        }
        DataType::Float16 | DataType::Float32 | DataType::Float64 => t.float,
        DataType::Decimal128(..) | DataType::Decimal256(..) => t.decimal,
        DataType::Date32 | DataType::Date64 => t.date,
        DataType::Timestamp(_, tz) => {
            if tz.is_some() {
                t.timestamp_tz
            } else {
                t.timestamp
            }
        }
        DataType::Time32(_) | DataType::Time64(_) => t.time,
        DataType::Binary | DataType::LargeBinary | DataType::FixedSizeBinary(_) => t.binary,
        _ => t.text,
    }
}

/// `CREATE TABLE table (col type, …)` for the columns of `schema`; columns
/// Arrow marks non-nullable get `NOT NULL`.
pub fn create_table_sql(dialect: ConnectorKind, table: &str, schema: &SchemaRef) -> String {
    let cols: Vec<String> = schema
        .fields()
        .iter()
        .map(|f| {
            let null = match (f.is_nullable(), lt(dialect).explicit_null) {
                (false, _) => " NOT NULL",
                (true, true) => " NULL",
                (true, false) => "",
            };
            format!("{} {}{null}", quote_ident(dialect, f.name()), column_type(dialect, f))
        })
        .collect();
    format!("CREATE TABLE {} ({}){}", quote_path(dialect, table), cols.join(", "), lt(dialect).create_suffix)
}

/// [`create_table_sql`] that does nothing when the table already exists
/// (it may have been created by SQL run just before).
pub fn create_table_if_missing_sql(dialect: ConnectorKind, table: &str, schema: &SchemaRef) -> String {
    let create = create_table_sql(dialect, table, schema);
    match lt(dialect).create_if_missing {
        CreateIfMissing::PlsqlBlock => {
            format!("BEGIN EXECUTE IMMEDIATE '{}'; EXCEPTION WHEN OTHERS THEN IF SQLCODE != -955 THEN RAISE; END IF; END;", create.replace('\'', "''"))
        }
        CreateIfMissing::ObjectIdCheck => format!("IF OBJECT_ID(N'{}', N'U') IS NULL {create}", quote_path(dialect, table).replace('\'', "''")),
        CreateIfMissing::IfNotExists => create.replacen("CREATE TABLE ", "CREATE TABLE IF NOT EXISTS ", 1),
    }
}

/// `SELECT * FROM table` returning no rows: succeeds (with the columns)
/// only when the table exists and can be read.
pub fn probe_sql(dialect: ConnectorKind, table: &str) -> String {
    let t = quote_path(dialect, table);
    if lt(dialect).probe_limit0 { format!("SELECT * FROM {t} LIMIT 0") } else { format!("SELECT * FROM {t} WHERE 1 = 0") }
}

/// `DELETE FROM table` (Truncate mode: transactional everywhere, unlike
/// `TRUNCATE`); `TRUNCATE TABLE` where DELETE needs a WHERE (ClickHouse).
pub fn delete_all_sql(dialect: ConnectorKind, table: &str) -> String {
    lt(dialect).delete_all.replace("{}", &quote_path(dialect, table))
}

/// Whether Update and Merge by key can write into `dialect`.
pub fn keyed_loads(dialect: ConnectorKind) -> bool {
    dialect.traits().load.is_some_and(|l| l.keyed)
}

/// Engines where the staging table of Update/Merge is a session temporary
/// table; elsewhere it is a regular table next to the target, dropped after.
pub fn stage_is_temporary(dialect: ConnectorKind) -> bool {
    !matches!(lt(dialect).stage, StageTable::Regular | StageTable::RegularLimit0)
}

/// Name (unquoted path) of the staging table for `target`, `suffix` making it unique.
pub fn stage_table_name(dialect: ConnectorKind, target: &str, suffix: &str) -> String {
    let name = format!("databrain_stage_{suffix}");
    match lt(dialect).stage {
        StageTable::HashSelectInto => format!("#{name}"),
        _ if stage_is_temporary(dialect) => name,
        _ => match target.rsplit_once('.') {
            Some((q, _)) => format!("{q}.{name}"),
            None => name,
        },
    }
}

/// Create the empty staging table with the target's own types for `cols`
/// (`CREATE … AS SELECT … WHERE 1 = 0`), so key comparisons match exactly.
pub fn create_stage_sql(dialect: ConnectorKind, stage: &str, target: &str, cols: &[String]) -> String {
    let (s, t) = (quote_path(dialect, stage), quote_path(dialect, target));
    let list = cols.iter().map(|c| quote_ident(dialect, c)).collect::<Vec<_>>().join(", ");
    match lt(dialect).stage {
        // UNION ALL drops the IDENTITY property SELECT INTO would copy.
        StageTable::HashSelectInto => format!("SELECT {list} INTO {s} FROM {t} WHERE 1 = 0 UNION ALL SELECT {list} FROM {t} WHERE 1 = 0"),
        StageTable::Temp => format!("CREATE TEMP TABLE {s} AS SELECT {list} FROM {t} WHERE 1 = 0"),
        StageTable::Temporary | StageTable::TemporaryDropTemporary => format!("CREATE TEMPORARY TABLE {s} AS SELECT {list} FROM {t} WHERE 1 = 0"),
        StageTable::RegularLimit0 => format!("CREATE TABLE {s} AS SELECT {list} FROM {t} LIMIT 0"),
        StageTable::Regular => format!("CREATE TABLE {s} AS SELECT {list} FROM {t} WHERE 1 = 0"),
    }
}

/// Drop the staging table (never fails when it is already gone).
pub fn drop_stage_sql(dialect: ConnectorKind, stage: &str) -> String {
    match lt(dialect).stage {
        StageTable::TemporaryDropTemporary => format!("DROP TEMPORARY TABLE IF EXISTS {}", quote_path(dialect, stage)),
        StageTable::HashSelectInto => format!("IF OBJECT_ID('tempdb..{}') IS NOT NULL DROP TABLE {}", stage.replace('\'', "''"), quote_path(dialect, stage)),
        _ => drop_table_sql(dialect, stage),
    }
}

/// `a` equals `b`, NULL equal to NULL: key columns of Update/Merge/Sync
/// may hold NULLs, and `NULL = NULL` is never true (a Merge inserted those
/// rows again on every run).
fn key_eq(dialect: ConnectorKind, a: &str, b: &str) -> String {
    use ConnectorKind as K;
    match dialect {
        K::Postgres | K::Duckdb | K::Snowflake | K::Databricks | K::Bigquery => format!("{a} IS NOT DISTINCT FROM {b}"),
        K::Mysql => format!("{a} <=> {b}"),
        K::Sqlite => format!("{a} IS {b}"),
        // SQL Server before 2022 and Oracle have no null-safe operator.
        _ => format!("({a} = {b} OR ({a} IS NULL AND {b} IS NULL))"),
    }
}

/// The key columns of an Update/Merge/Sync, and those of them the target
/// table may hold NULL in (matched null-safely; the others with `=`, which
/// can use an index or a hash join).
#[derive(Debug, Clone, Copy)]
pub struct Keys<'a> {
    pub all: &'a [String],
    pub nullable: &'a [String],
}

impl<'a> Keys<'a> {
    /// Every key may be NULL (the table's columns aren't known).
    pub fn nullable(all: &'a [String]) -> Self {
        Keys { all, nullable: all }
    }
    fn contains(&self, c: &String) -> bool {
        self.all.contains(c)
    }
}

/// Every key of `left` (an alias or table) matching the same key of `right`
/// (null-safely for keys that may be NULL).
fn keys_match(dialect: ConnectorKind, keys: Keys, left: &str, right: &str) -> String {
    keys.all
        .iter()
        .map(|k| {
            let q = quote_ident(dialect, k);
            let (a, b) = (format!("{left}.{q}"), format!("{right}.{q}"));
            if keys.nullable.iter().any(|n| n == k) { key_eq(dialect, &a, &b) } else { format!("{a} = {b}") }
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// Delete the rows of `target` whose `keys` match no row of `stage` (Sync).
/// Keys match null-safely, as in the merge before it.
pub fn delete_missing_sql(dialect: ConnectorKind, target: &str, stage: &str, keys: Keys) -> String {
    let (t, s) = (quote_path(dialect, target), quote_path(dialect, stage));
    match lt(dialect).merge {
        // SQL Server: the alias form; BigQuery: `DELETE t alias`.
        MergeStyle::MergeSemicolon | MergeStyle::MergeNoInto => {
            let on = keys_match(dialect, keys, "src", "tg");
            let head = if lt(dialect).merge == MergeStyle::MergeSemicolon { format!("DELETE tg FROM {t} AS tg") } else { format!("DELETE {t} tg") };
            format!("{head} WHERE NOT EXISTS (SELECT 1 FROM {s} AS src WHERE {on})")
        }
        // Elsewhere the subquery names the target by its table name (no
        // schema or catalog: four-part column names aren't accepted everywhere).
        _ => {
            let name = databrain_connector_core::split_path(dialect, target).pop().map(|n| quote_ident(dialect, &n)).unwrap_or_else(|| t.clone());
            let on = keys_match(dialect, keys, "src", &name);
            format!("DELETE FROM {t} WHERE NOT EXISTS (SELECT 1 FROM {s} src WHERE {on})")
        }
    }
}

/// Engines that update/merge with a native `MERGE`; the others use
/// `UPDATE … FROM` / `UPDATE … JOIN` and `INSERT … WHERE NOT EXISTS`.
fn native_merge(dialect: ConnectorKind) -> bool {
    !matches!(lt(dialect).merge, MergeStyle::UpdateFrom | MergeStyle::UpdateJoin)
}

fn merge_sql(dialect: ConnectorKind, target: &str, stage: &str, cols: &[String], keys: Keys, insert: bool) -> String {
    let style = lt(dialect).merge;
    let q = |c: &str| quote_ident(dialect, c);
    let (t, s) = (quote_path(dialect, target), quote_path(dialect, stage));
    let on = keys_match(dialect, keys, "tg", "src");
    let set_target = |c: &str| if style == MergeStyle::MergeNoInto { q(c) } else { format!("tg.{}", q(c)) };
    let sets: Vec<String> = cols.iter().filter(|c| !keys.contains(c)).map(|c| format!("{} = src.{}", set_target(c), q(c))).collect();
    let (head, on) = match style {
        MergeStyle::MergeNoAs => (format!("MERGE INTO {t} tg USING {s} src"), format!("({on})")),
        MergeStyle::MergeNoInto => (format!("MERGE {t} AS tg USING {s} AS src"), on),
        _ => (format!("MERGE INTO {t} AS tg USING {s} AS src"), on),
    };
    let mut sql = format!("{head} ON {on}");
    if !sets.is_empty() {
        sql.push_str(&format!(" WHEN MATCHED THEN UPDATE SET {}", sets.join(", ")));
    }
    if insert {
        let list = cols.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ");
        let vals = cols.iter().map(|c| format!("src.{}", q(c))).collect::<Vec<_>>().join(", ");
        sql.push_str(&format!(" WHEN NOT MATCHED THEN INSERT ({list}) VALUES ({vals})"));
    }
    if style == MergeStyle::MergeSemicolon {
        // SQL Server requires MERGE to end with a semicolon.
        sql.push(';');
    }
    sql
}

fn update_join_sql(dialect: ConnectorKind, target: &str, stage: &str, cols: &[String], keys: Keys) -> Option<String> {
    let q = |c: &str| quote_ident(dialect, c);
    let (t, s) = (quote_path(dialect, target), quote_path(dialect, stage));
    let on = keys_match(dialect, keys, "tg", "src");
    let set: Vec<&String> = cols.iter().filter(|c| !keys.contains(c)).collect();
    if set.is_empty() {
        return None;
    }
    Some(match lt(dialect).merge {
        MergeStyle::UpdateJoin => {
            format!("UPDATE {t} AS tg JOIN {s} AS src ON {on} SET {}", set.iter().map(|c| format!("tg.{} = src.{}", q(c), q(c))).collect::<Vec<_>>().join(", "))
        }
        _ => {
            format!("UPDATE {t} AS tg SET {} FROM {s} AS src WHERE {on}", set.iter().map(|c| format!("{} = src.{}", q(c), q(c))).collect::<Vec<_>>().join(", "))
        }
    })
}

/// Update rows of `target` from `stage` where `keys` match (`cols` = every
/// staged column, keys included). Empty when only key columns are given.
pub fn update_from_stage_sql(dialect: ConnectorKind, target: &str, stage: &str, cols: &[String], keys: Keys) -> Vec<String> {
    if !cols.iter().any(|c| !keys.contains(c)) {
        return vec![];
    }
    if native_merge(dialect) {
        return vec![merge_sql(dialect, target, stage, cols, keys, false)];
    }
    update_join_sql(dialect, target, stage, cols, keys).into_iter().collect()
}

/// Upsert `stage` into `target`: update rows whose `keys` match, insert the others.
pub fn merge_from_stage_sql(dialect: ConnectorKind, target: &str, stage: &str, cols: &[String], keys: Keys) -> Vec<String> {
    if native_merge(dialect) {
        return vec![merge_sql(dialect, target, stage, cols, keys, true)];
    }
    let q = |c: &str| quote_ident(dialect, c);
    let (t, s) = (quote_path(dialect, target), quote_path(dialect, stage));
    let on = keys_match(dialect, keys, "tg", "src");
    let list = cols.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ");
    let vals = cols.iter().map(|c| format!("src.{}", q(c))).collect::<Vec<_>>().join(", ");
    // Update first, so the rows inserted next are not updated again.
    let mut out: Vec<String> = update_join_sql(dialect, target, stage, cols, keys).into_iter().collect();
    out.push(format!("INSERT INTO {t} ({list}) SELECT {vals} FROM {s} AS src WHERE NOT EXISTS (SELECT 1 FROM {t} AS tg WHERE {on})"));
    out
}

/// `DROP TABLE IF EXISTS` in `dialect` (Oracle: a block ignoring "does not exist").
pub fn drop_table_sql(dialect: ConnectorKind, table: &str) -> String {
    let t = quote_path(dialect, table);
    match lt(dialect).drop_in_plsql_block {
        true => {
            format!("BEGIN EXECUTE IMMEDIATE 'DROP TABLE {}'; EXCEPTION WHEN OTHERS THEN IF SQLCODE != -942 THEN RAISE; END IF; END;", t.replace('\'', "''"))
        }
        false => format!("DROP TABLE IF EXISTS {t}"),
    }
}

/// The timestamp with its offset as Oracle's literal wants it, after a
/// space and as `±HH:MM`: `…05.6+02:00`, `…05.6+0200`, `…05.6+02` →
/// `…05.6 +02:00`; a trailing `Z` (UTC) → ` +00:00`. Without an offset it
/// is returned as it is.
fn spaced_offset(s: &str) -> String {
    let s = s.trim();
    if let Some(t) = s.strip_suffix(['Z', 'z']) {
        return format!("{} +00:00", t.trim_end());
    }
    // The offset's sign comes after the time (past the date's dashes).
    let Some(i) = s.get(11..).and_then(|t| t.rfind(['+', '-'])).map(|i| i + 11) else { return s.to_string() };
    let (time, off) = (s[..i].trim_end(), &s[i..]);
    let (sign, digits) = off.split_at(1);
    let hm: String = digits.chars().filter(|c| *c != ':').collect();
    let off = match hm.len() {
        2 if hm.chars().all(|c| c.is_ascii_digit()) => format!("{sign}{hm}:00"),
        4 if hm.chars().all(|c| c.is_ascii_digit()) => format!("{sign}{}:{}", &hm[..2], &hm[2..]),
        _ => off.to_string(),
    };
    format!("{time} {off}")
}

/// NaN or ±inf when `s` is one of their words (`NaN`, `inf`, `-Infinity`, …);
/// not for a number too large for an f64 (a decimal stays a number).
fn non_finite(s: &str) -> Option<f64> {
    let w = s.trim();
    let word = w.strip_prefix(['+', '-']).unwrap_or(w).to_ascii_lowercase();
    matches!(word.as_str(), "nan" | "inf" | "infinity").then(|| w.parse::<f64>().ok()).flatten()
}

/// NaN or ±inf as a value: Postgres and DuckDB float columns have them
/// (`'NaN'::float8`), ClickHouse's too (`nan`, `-inf`); a decimal or integer column, and other databases,
/// get NULL. Never quoted text: SQL Server and MySQL (strict) refuse it
/// in a numeric column and SQLite would store the text.
fn non_finite_literal(v: f64, kind: Lit, dialect: ConnectorKind) -> String {
    let word = if v.is_nan() {
        "NaN"
    } else if v > 0.0 {
        "Infinity"
    } else {
        "-Infinity"
    };
    match (kind, dialect) {
        (Lit::Float, ConnectorKind::Postgres) => format!("'{word}'::float8"),
        (Lit::Float, ConnectorKind::Duckdb) => format!("'{word}'::DOUBLE"),
        (Lit::Float, ConnectorKind::Clickhouse) => (if v.is_nan() {
            "nan"
        } else if v > 0.0 {
            "inf"
        } else {
            "-inf"
        })
        .into(),
        _ => "NULL".into(),
    }
}

/// An exact number for BigQuery: `12.5` alone is a FLOAT64 there, which a
/// NUMERIC or BIGNUMERIC column refuses (and a quoted string too). Whole
/// numbers that fit INT64 stay bare; other decimals become `NUMERIC '…'`
/// (it widens to BIGNUMERIC and FLOAT64), or `BIGNUMERIC '…'` past NUMERIC's
/// 29 digits before and 9 after the point.
fn bigquery_number(s: &str) -> String {
    let t = s.trim();
    let digits = t.strip_prefix(['+', '-']).unwrap_or(t);
    if digits.contains(['e', 'E']) {
        return t.to_string();
    }
    if !digits.contains('.') && t.parse::<i64>().is_ok() {
        return t.to_string();
    }
    let (int, frac) = digits.split_once('.').unwrap_or((digits, ""));
    let ty = if int.trim_start_matches('0').len() <= 29 && frac.len() <= 9 { "NUMERIC" } else { "BIGNUMERIC" };
    format!("{ty} '{t}'")
}

pub(crate) fn literal(cell: Option<&str>, kind: Lit, dialect: ConnectorKind) -> String {
    let l = lt(dialect);
    let Some(s) = cell else { return "NULL".into() };
    match kind {
        Lit::Number | Lit::Float => match non_finite(s) {
            Some(v) => return non_finite_literal(v, kind, dialect),
            None if matches!(kind, Lit::Number) && dialect == ConnectorKind::Bigquery && s.parse::<f64>().is_ok() => return bigquery_number(s),
            None if s.parse::<f64>().is_ok() => return s.to_string(),
            None => {}
        },
        _ => {}
    }
    match kind {
        Lit::Bool if l.bool_words => s.to_uppercase(),
        Lit::Bool => {
            if s == "true" {
                "1".into()
            } else {
                "0".into()
            }
        }
        Lit::Binary => l.binary_literal.replace("{}", s),
        // Oracle doesn't read text as a date without NLS settings: typed literals.
        // A Date64 shows its time (`… 00:00:00`): a TIMESTAMP literal (Oracle's DATE holds it).
        Lit::Date if l.typed_datetime_literals && s.trim().len() > 10 => format!("TIMESTAMP {}", quote_literal(s)),
        Lit::Date if l.typed_datetime_literals => format!("DATE {}", quote_literal(s)),
        Lit::Timestamp if l.typed_datetime_literals => format!("TIMESTAMP {}", quote_literal(s)),
        // Oracle reads text as a timestamp only with NLS settings, and its
        // literal wants a space before the offset: `TIMESTAMP '… +02:00'`.
        Lit::TimestampTz if l.typed_datetime_literals => format!("TIMESTAMP {}", quote_literal(&spaced_offset(s))),
        // The same text escaper as parameters (connector-core).
        _ => match l.text_literal {
            TextLiteral::NPrefix => format!("N{}", databrain_connector_core::sql::quote_string(s, dialect)),
            TextLiteral::BackslashEscaped | TextLiteral::Plain => databrain_connector_core::sql::quote_string(s, dialect),
        },
    }
}

/// SQL literal for a value typed into a grid cell of column `f` (`None` =
/// NULL). Numbers are written as numbers only when they are numbers;
/// booleans and binary values are checked, so the typed text never becomes
/// SQL of its own.
pub fn edit_literal(f: &Field, value: Option<&str>, dialect: ConnectorKind) -> std::result::Result<String, String> {
    let Some(v) = value else { return Ok("NULL".into()) };
    match lit_kind(f) {
        Lit::Bool => match v.trim().to_ascii_lowercase().as_str() {
            "true" | "t" | "1" | "yes" | "y" => Ok(literal(Some("true"), Lit::Bool, dialect)),
            "false" | "f" | "0" | "no" | "n" => Ok(literal(Some("false"), Lit::Bool, dialect)),
            _ => Err(format!("{}: “{v}” is not true or false", f.name())),
        },
        Lit::Binary => {
            let hex = v.trim().trim_start_matches("0x").trim_start_matches("\\x");
            if hex.len() % 2 == 0 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                Ok(literal(Some(&hex.to_ascii_lowercase()), Lit::Binary, dialect))
            } else {
                Err(format!("{}: binary values are typed as hex (00ff…)", f.name()))
            }
        }
        Lit::Number | Lit::Float if v.trim().parse::<f64>().is_err() => Err(format!("{}: “{v}” is not a number", f.name())),
        // Typed NaN/inf where the column can't hold it: refused, not written as NULL.
        k @ (Lit::Number | Lit::Float) if non_finite(v).is_some_and(|x| non_finite_literal(x, k, dialect) == "NULL") => {
            Err(format!("{}: “{}” can't be stored in this column (leave it empty for NULL)", f.name(), v.trim()))
        }
        // Only numbers are trimmed: text keeps its whitespace (a key `a\n`
        // must not match the row of `a`).
        k => Ok(literal(Some(if matches!(k, Lit::Number | Lit::Float) { v.trim() } else { v }), k, dialect)),
    }
}

/// Builds batched INSERT statements one at a time, so a large load never
/// holds all of its SQL text in memory.
pub struct InsertSql {
    dialect: ConnectorKind,
    table: String,
    cols: String,
    kinds: Vec<Lit>,
    size: BatchSize,
}

impl InsertSql {
    /// INSERTs of at most `size` (rows and bytes of the whole statement).
    pub fn new(dialect: ConnectorKind, table: &str, schema: &SchemaRef, size: BatchSize) -> Self {
        Self {
            dialect,
            table: quote_path(dialect, table),
            cols: schema.fields().iter().map(|f| quote_ident(dialect, f.name())).collect::<Vec<_>>().join(", "),
            kinds: schema.fields().iter().map(|f| lit_kind(f)).collect(),
            size,
        }
    }

    /// Bytes of the statement around the rows, and added per row.
    fn overhead(&self) -> (usize, usize) {
        let (t, cols) = (&self.table, &self.cols);
        match lt(self.dialect).insert_all {
            // `INSERT ALL INTO t (cols) VALUES (…) … SELECT 1 FROM DUAL`
            true => ("INSERT ALL".len() + " SELECT 1 FROM DUAL".len(), format!(" INTO {t} ({cols}) VALUES ").len()),
            // `INSERT INTO t (cols) [SETTINGS …] VALUES (…), (…)`
            false => (format!("INSERT INTO {t} ({cols}){} VALUES ", lt(self.dialect).insert_settings).len(), 2),
        }
    }

    /// The next statement for rows `start..` of `batch` (at most `max_rows`
    /// of them): the SQL and how many rows it inserts. `None` past the end.
    pub fn next(&self, batch: &RecordBatch, start: usize, max_rows: Option<usize>) -> Result<Option<(String, usize)>> {
        if start >= batch.num_rows() {
            return Ok(None);
        }
        let fopts = format_options();
        let fmts: Vec<ArrayFormatter<'_>> =
            batch.columns().iter().map(|a| ArrayFormatter::try_new(a.as_ref(), &fopts)).collect::<std::result::Result<_, _>>()?;
        let limit = max_rows.unwrap_or(usize::MAX).min(self.size.rows).max(1);
        let (fixed, per_row) = self.overhead();
        let mut rows: Vec<String> = Vec::new();
        let mut bytes = fixed;
        for r in start..batch.num_rows() {
            let vals: Vec<String> = batch
                .columns()
                .iter()
                .zip(&fmts)
                .zip(&self.kinds)
                .map(|((a, f), k)| literal((!a.is_null(r)).then(|| f.value(r).to_string()).as_deref(), *k, self.dialect))
                .collect();
            let row = format!("({})", vals.join(", "));
            // A single row always goes, even past the byte limit (it can't be split).
            if !rows.is_empty() && (rows.len() >= limit || bytes + per_row + row.len() > self.size.bytes) {
                break;
            }
            bytes += per_row + row.len();
            rows.push(row);
        }
        let n = rows.len();
        let (t, cols) = (&self.table, &self.cols);
        let sql = match lt(self.dialect).insert_all {
            true => format!("INSERT ALL {} SELECT 1 FROM DUAL", rows.iter().map(|r| format!("INTO {t} ({cols}) VALUES {r}")).collect::<Vec<_>>().join(" ")),
            false => format!("INSERT INTO {t} ({cols}){} VALUES {}", lt(self.dialect).insert_settings, rows.join(", ")),
        };
        Ok(Some((sql, n)))
    }
}

/// Rows as PostgreSQL `COPY … FROM STDIN` text format (a tab between
/// values, `\N` for NULL), each value the text its INSERT literal stands
/// for, so the table ends up the same: NaN/±inf as the INSERT path writes
/// them (the float's word, NULL in an exact number), booleans as
/// `true`/`false`, binary as `\x` hex. Which columns may take this path:
/// [`pg_copy_fits`].
pub struct PgCopy {
    kinds: Vec<Lit>,
    /// `COPY table (cols) FROM STDIN`.
    pub sql: String,
}

impl PgCopy {
    pub fn new(table: &str, schema: &SchemaRef) -> Self {
        let d = ConnectorKind::Postgres;
        Self {
            kinds: schema.fields().iter().map(|f| lit_kind(f)).collect(),
            sql: format!(
                "COPY {} ({}) FROM STDIN",
                quote_path(d, table),
                schema.fields().iter().map(|f| quote_ident(d, f.name())).collect::<Vec<_>>().join(", ")
            ),
        }
    }

    /// Data for rows `start..` of `batch`, up to about `max_bytes` (at least
    /// one row): the bytes and how many rows they hold. `None` past the end.
    pub fn next(&self, batch: &RecordBatch, start: usize, max_bytes: usize) -> Result<Option<(Vec<u8>, usize)>> {
        if start >= batch.num_rows() {
            return Ok(None);
        }
        let fopts = format_options();
        let fmts: Vec<ArrayFormatter<'_>> =
            batch.columns().iter().map(|a| ArrayFormatter::try_new(a.as_ref(), &fopts)).collect::<std::result::Result<_, _>>()?;
        let mut out = Vec::new();
        let mut n = 0;
        for r in start..batch.num_rows() {
            if n > 0 && out.len() >= max_bytes {
                break;
            }
            for (i, ((a, f), k)) in batch.columns().iter().zip(&fmts).zip(&self.kinds).enumerate() {
                if i > 0 {
                    out.push(b'\t');
                }
                let cell = (!a.is_null(r)).then(|| f.value(r).to_string());
                pg_copy_value(&mut out, cell.as_deref(), *k);
            }
            out.push(b'\n');
            n += 1;
        }
        Ok(Some((out, n)))
    }
}

/// A negative zero written as text (`-0`, `-0.000`, `-0e5`): every digit
/// of the number is 0. Read from the text, not as a float: `-1e-400`
/// rounds to 0 as an f64 but is a negative number for `numeric`.
fn is_negative_zero(s: &str) -> bool {
    let Some(rest) = s.trim().strip_prefix('-') else { return false };
    let mantissa = rest.split(['e', 'E']).next().unwrap_or_default();
    let mut digits = mantissa.chars().filter(|c| *c != '.').peekable();
    digits.peek().is_some() && mantissa.chars().all(|c| c.is_ascii_digit() || c == '.') && digits.all(|c| c == '0')
}

/// One value in COPY text format, as [`literal`] writes it for Postgres.
fn pg_copy_value(out: &mut Vec<u8>, cell: Option<&str>, kind: Lit) {
    const NULL: &[u8] = br"\N";
    let Some(s) = cell else {
        out.extend_from_slice(NULL);
        return;
    };
    let text: std::borrow::Cow<str> = match kind {
        Lit::Number | Lit::Float => match non_finite(s) {
            // NULL in an exact number, the float's own word otherwise.
            Some(v) if non_finite_literal(v, kind, ConnectorKind::Postgres) == "NULL" => {
                out.extend_from_slice(NULL);
                return;
            }
            Some(v) if v.is_nan() => "NaN".into(),
            Some(v) if v > 0.0 => "Infinity".into(),
            Some(_) => "-Infinity".into(),
            // A negative zero: the INSERT's bare `-0.0` is the numeric 0
            // negated, which stays 0, while float8in keeps -0.
            None if is_negative_zero(s) => s.trim_start().trim_start_matches('-').into(),
            None => s.into(),
        },
        Lit::Bool => s.to_ascii_lowercase().into(),
        Lit::Binary => format!(r"\x{s}").into(),
        _ => s.into(),
    };
    for b in text.bytes() {
        match b {
            b'\\' => out.extend_from_slice(br"\\"),
            b'\n' => out.extend_from_slice(br"\n"),
            b'\r' => out.extend_from_slice(br"\r"),
            b'\t' => out.extend_from_slice(br"\t"),
            _ => out.push(b),
        }
    }
}

/// Each column's values as text (`None`: NULL), for an [`ArrayInsert`].
pub type Columns = Vec<Vec<Option<String>>>;

/// One INSERT with a placeholder per column, run with arrays of values
/// (Oracle array DML), and the rows as those arrays: each value as the
/// text its INSERT literal stands for, turned into its type in the SQL
/// with explicit formats, so the table ends up as with the INSERTs
/// whatever the session's language settings (the session fixes the
/// decimal separator while it runs, see `Session::insert_array`).
pub struct ArrayInsert {
    dialect: ConnectorKind,
    kinds: Vec<Lit>,
    /// `INSERT INTO table (cols) VALUES (:1, TO_DATE(:2, …), …)`.
    pub sql: String,
}

impl ArrayInsert {
    /// For a dialect with array binding (Oracle, Snowflake); `None` otherwise.
    pub fn new(dialect: ConnectorKind, table: &str, schema: &SchemaRef) -> Option<Self> {
        let values: Vec<String> = match dialect {
            // Each value TEXT, cast to its column's type as its literal is
            // (dates are quoted text there too). Plain placeholders: an array
            // bind of an expression may be refused.
            ConnectorKind::Snowflake => vec!["?".to_string(); schema.fields().len()],
            ConnectorKind::Oracle => Self::oracle_values(schema),
            _ => return None,
        };
        Some(Self {
            dialect,
            kinds: schema.fields().iter().map(|f| lit_kind(f)).collect(),
            sql: format!(
                "INSERT INTO {} ({}) VALUES ({})",
                quote_path(dialect, table),
                schema.fields().iter().map(|f| quote_ident(dialect, f.name())).collect::<Vec<_>>().join(", "),
                values.join(", ")
            ),
        })
    }

    /// Rows per call: at most 10,000, and on Snowflake as many as stay
    /// under `values` bound values (the session's limit, else 60,000: past
    /// about 65,000 its drivers stage the binds in files).
    pub fn rows_per_call(&self, values: Option<usize>) -> usize {
        match self.dialect {
            ConnectorKind::Snowflake => (values.unwrap_or(60_000) / self.kinds.len().max(1)).clamp(1, 10_000),
            _ => 10_000,
        }
    }

    /// Each column's bind type, as the official Snowflake drivers name them:
    /// numbers `FIXED` and floats `REAL` (a TEXT bind into a number column
    /// may be cast as NUMBER(18,5) and lose digits), `BOOLEAN`, `BINARY` with
    /// the hex (text doesn't convert to binary), the rest `TEXT` as their
    /// literals are quoted text. Oracle binds text throughout (`TEXT`).
    pub fn bind_types(&self) -> Vec<&'static str> {
        self.kinds
            .iter()
            .map(|k| match (self.dialect, k) {
                (ConnectorKind::Snowflake, Lit::Number) => "FIXED",
                (ConnectorKind::Snowflake, Lit::Float) => "REAL",
                (ConnectorKind::Snowflake, Lit::Bool) => "BOOLEAN",
                (ConnectorKind::Snowflake, Lit::Binary) => "BINARY",
                _ => "TEXT",
            })
            .collect()
    }

    /// Bytes of values per call, counted as the request sends them (JSON):
    /// Oracle 32 MB (the thin helper reads requests up to 64 MB); Snowflake
    /// 1 MB, as its INSERTs had (it documents no limit for inline binds).
    pub fn bytes_per_call(&self) -> usize {
        match self.dialect {
            ConnectorKind::Snowflake => 1_000_000,
            _ => 32 * 1024 * 1024,
        }
    }

    fn oracle_values(schema: &SchemaRef) -> Vec<String> {
        schema
            .fields()
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let p = format!(":{}", i + 1);
                match (lit_kind(f), f.data_type()) {
                    (Lit::Date, DataType::Date32) => format!("TO_DATE({p}, 'YYYY-MM-DD')"),
                    // A Date64 shows its time: a timestamp, as its literal is.
                    (Lit::Date | Lit::Timestamp, _) => format!("TO_TIMESTAMP({p}, 'YYYY-MM-DD HH24:MI:SS.FF')"),
                    (Lit::TimestampTz, _) => format!("TO_TIMESTAMP_TZ({p}, 'YYYY-MM-DD HH24:MI:SS.FF TZH:TZM')"),
                    (Lit::Binary, _) => format!("HEXTORAW({p})"),
                    _ => p,
                }
            })
            .collect()
    }

    /// Rows `start..` of `batch` as one array per column, and how many rows
    /// they hold (at least one; `None` past the end). A call stops before a
    /// row that would pass `max_rows`, `max_bytes` of request (the values'
    /// JSON-escaped length: the thin Oracle helper and Snowflake get them as
    /// JSON) or [`ARRAY_SLOT_BYTES`] of bind buffers (rows × each column's
    /// longest value: a driver sizes every row's slot for the longest).
    /// `decimal`: the session's decimal separator, written in numbers so
    /// they convert as the INSERT's literals do (Oracle; `.` elsewhere).
    pub fn next(&self, batch: &RecordBatch, start: usize, max_rows: usize, max_bytes: usize, decimal: char) -> Result<Option<(Columns, usize)>> {
        if start >= batch.num_rows() {
            return Ok(None);
        }
        let fopts = format_options();
        let fmts: Vec<ArrayFormatter<'_>> =
            batch.columns().iter().map(|a| ArrayFormatter::try_new(a.as_ref(), &fopts)).collect::<std::result::Result<_, _>>()?;
        let ncols = batch.num_columns();
        let mut cols: Vec<Vec<Option<String>>> = (0..ncols).map(|_| Vec::with_capacity(max_rows.min(batch.num_rows() - start))).collect();
        let mut longest = vec![0usize; ncols];
        let mut row: Vec<Option<String>> = Vec::with_capacity(ncols);
        let sep = decimal.to_string();
        let (mut bytes, mut n) = (0usize, 0usize);
        for r in start..batch.num_rows() {
            if n >= max_rows {
                break;
            }
            row.clear();
            let (mut row_bytes, mut wider) = (0usize, 0usize);
            for (i, ((a, f), k)) in batch.columns().iter().zip(&fmts).zip(&self.kinds).enumerate() {
                let cell = (!a.is_null(r)).then(|| f.value(r).to_string());
                let mut v = match self.dialect {
                    ConnectorKind::Snowflake => snowflake_text(cell.as_deref(), *k),
                    _ => oracle_text(cell.as_deref(), *k),
                };
                if decimal != '.'
                    && matches!(k, Lit::Number | Lit::Float)
                    && let Some(t) = v.as_mut()
                {
                    *t = t.replace('.', &sep);
                }
                row_bytes += v.as_deref().map_or(5, json_len);
                wider += longest[i].max(v.as_ref().map_or(0, String::len));
                row.push(v);
            }
            if n > 0 && (bytes + row_bytes > max_bytes || (n + 1) * wider > ARRAY_SLOT_BYTES) {
                break;
            }
            for (i, (col, v)) in cols.iter_mut().zip(row.drain(..)).enumerate() {
                longest[i] = longest[i].max(v.as_ref().map_or(0, String::len));
                col.push(v);
            }
            bytes += row_bytes;
            n += 1;
        }
        Ok(Some((cols, n)))
    }
}

/// Bind buffer an array call may take: its rows times the sum of each
/// column's longest value (Instant Client allocates every row's slot for a
/// column's longest value, up to 4 bytes a character).
pub const ARRAY_SLOT_BYTES: usize = 16 * 1024 * 1024;

/// Length of `s` as a JSON string with its comma: quotes, `\"` `\\` and
/// short escapes 2 bytes, other control characters 6.
fn json_len(s: &str) -> usize {
    3 + s.len()
        + s.bytes()
            .filter(|b| *b < 0x20 || *b == b'"' || *b == b'\\')
            .map(|b| match b {
                b'"' | b'\\' | b'\n' | b'\r' | b'\t' | 0x08 | 0x0c => 1,
                _ => 5,
            })
            .sum::<usize>()
}

/// A value as its Snowflake bind (see [`ArrayInsert::bind_types`]) holds
/// it (`None`: NULL): NaN/±inf NULL (as the INSERT writes them), booleans
/// `true` / `false`, binary as hex, the rest the text of its literal.
fn snowflake_text(cell: Option<&str>, kind: Lit) -> Option<String> {
    let s = cell?;
    match kind {
        Lit::Number | Lit::Float if non_finite(s).is_some() => None,
        Lit::Bool => Some(s.to_ascii_lowercase()),
        _ => Some(s.to_string()),
    }
}

/// A value as the text its Oracle INSERT literal stands for (`None`: NULL):
/// NaN/±inf NULL (as the INSERT writes them), booleans `1`/`0`, binary as
/// hex, a time zone's offset after a space (as `TO_TIMESTAMP_TZ` reads it).
fn oracle_text(cell: Option<&str>, kind: Lit) -> Option<String> {
    let s = cell?;
    match kind {
        Lit::Number | Lit::Float if non_finite(s).is_some() => None,
        Lit::Bool => Some(if s.eq_ignore_ascii_case("true") { "1" } else { "0" }.to_string()),
        Lit::TimestampTz => Some(spaced_offset(s)),
        _ => Some(s.to_string()),
    }
}

/// Rows for a Databricks bulk load: Parquet files of text columns `c0`,
/// `c1`, … holding each value as the text its INSERT literal stands for
/// (NULL for NaN/±inf, which the INSERT writes as NULL, booleans as
/// `true`/`false`, binary as hex), and one `INSERT … SELECT` that casts each
/// column to the table's type, as the INSERT's literal is ([`dbx_fits`]
/// says when it is the same), from all the files at once: one commit.
pub struct DbxStage {
    kinds: Vec<Lit>,
    /// `INSERT INTO table (cols) SELECT … FROM read_files(BULK_FILES, …)`.
    pub sql: String,
}

impl DbxStage {
    /// `target`: each row column's type in the table, as `DESCRIBE TABLE`
    /// writes it (checked by [`dbx_fits`] first).
    pub fn new(table: &str, schema: &SchemaRef, target: &[String]) -> Self {
        let d = ConnectorKind::Databricks;
        let exprs: Vec<String> = schema.fields().iter().zip(target).enumerate().map(|(i, (f, t))| dbx_expr(&format!("c{i}"), f, t)).collect();
        let read_schema = (0..schema.fields().len()).map(|i| format!("c{i} STRING")).collect::<Vec<_>>().join(", ");
        Self {
            kinds: schema.fields().iter().map(|f| lit_kind(f)).collect(),
            sql: format!(
                "INSERT INTO {} ({}) SELECT {} FROM read_files({}, format => 'parquet', schema => '{read_schema}')",
                quote_path(d, table),
                schema.fields().iter().map(|f| quote_ident(d, f.name())).collect::<Vec<_>>().join(", "),
                exprs.join(", "),
                databrain_connector_core::BULK_FILES,
            ),
        }
    }

    /// One Parquet file of rows `start..` of `batch`, up to about
    /// `max_bytes` of text (at least one row): the file and how many rows it
    /// holds. `None` past the end.
    pub fn next(&self, batch: &RecordBatch, start: usize, max_bytes: usize) -> Result<Option<(Vec<u8>, usize)>> {
        use databrain_connector_core::arrow::array::StringBuilder;
        use databrain_connector_core::arrow::datatypes::Schema;
        if start >= batch.num_rows() {
            return Ok(None);
        }
        let fopts = format_options();
        let fmts: Vec<ArrayFormatter<'_>> =
            batch.columns().iter().map(|a| ArrayFormatter::try_new(a.as_ref(), &fopts)).collect::<std::result::Result<_, _>>()?;
        let mut cols: Vec<StringBuilder> = (0..batch.num_columns()).map(|_| StringBuilder::new()).collect();
        let (mut bytes, mut n) = (0usize, 0usize);
        for r in start..batch.num_rows() {
            if n > 0 && bytes >= max_bytes {
                break;
            }
            for ((col, (a, f)), k) in cols.iter_mut().zip(batch.columns().iter().zip(&fmts)).zip(&self.kinds) {
                let cell = (!a.is_null(r)).then(|| f.value(r).to_string());
                match dbx_text(cell.as_deref(), *k) {
                    Some(t) => {
                        bytes += t.len();
                        col.append_value(t);
                    }
                    None => col.append_null(),
                }
            }
            n += 1;
        }
        let schema = std::sync::Arc::new(Schema::new((0..cols.len()).map(|i| Field::new(format!("c{i}"), DataType::Utf8, true)).collect::<Vec<_>>()));
        let arrays: Vec<std::sync::Arc<dyn Array>> = cols.iter_mut().map(|c| std::sync::Arc::new(c.finish()) as std::sync::Arc<dyn Array>).collect();
        let rows = RecordBatch::try_new(schema.clone(), arrays)?;
        let props = parquet::file::properties::WriterProperties::builder().set_compression(parquet::basic::Compression::SNAPPY).build();
        let mut out = Vec::new();
        let mut w = parquet::arrow::ArrowWriter::try_new(&mut out, schema, Some(props))?;
        w.write(&rows)?;
        w.close()?;
        Ok(Some((out, n)))
    }
}

/// A value as the text its Databricks INSERT literal stands for (`None`: NULL).
fn dbx_text(cell: Option<&str>, kind: Lit) -> Option<String> {
    let s = cell?;
    match kind {
        // NaN/±inf: the INSERT writes NULL on Databricks.
        Lit::Number | Lit::Float if non_finite(s).is_some() => None,
        Lit::Bool => Some(s.to_ascii_lowercase()),
        _ => Some(s.to_string()),
    }
}

/// The table's base type, lowercase, without its parameters (`decimal(10,2)` → `decimal`).
fn dbx_base(target: &str) -> String {
    target.trim().to_ascii_lowercase().split(['(', '<', ' ']).next().unwrap_or_default().to_string()
}

const DBX_INTEGRAL: [&str; 7] = ["tinyint", "byte", "smallint", "short", "int", "integer", "bigint"];

/// The SELECT expression that turns text column `c` into column `f`'s
/// value in a `target` column as the INSERT's literal would be: a number
/// into an integer column through a decimal (`12.5` truncates to 12, as
/// the literal does), binary from its hex, text as a string (the table
/// checks a VARCHAR's length, as for an INSERT), the rest cast to the type.
fn dbx_expr(c: &str, f: &Field, target: &str) -> String {
    let base = dbx_base(target);
    match lit_kind(f) {
        Lit::Binary => format!("unhex({c})"),
        Lit::Text => format!("CAST({c} AS STRING)"),
        Lit::Number if DBX_INTEGRAL.contains(&base.as_str()) => format!("CAST(CAST({c} AS DECIMAL(38, 10)) AS {})", target.trim()),
        _ => format!("CAST({c} AS {})", target.trim()),
    }
}

/// Whether column `f` of the rows can go into a Databricks column of type
/// `target` (as `DESCRIBE TABLE` writes it) by a bulk load with the same
/// result as its INSERT literal; `Err` says why not (its rows then go by
/// INSERTs). Casting text is stricter or looser than assigning a literal
/// for other pairs (text into a number column, a fraction into a boolean),
/// so only these go: text into strings, numbers into numbers, booleans into
/// booleans, binary into binary, dates and times into dates and timestamps.
pub fn dbx_fits(f: &Field, target: &str) -> std::result::Result<(), String> {
    let t = target.trim();
    if t.is_empty() || !t.bytes().all(|b| b.is_ascii_alphanumeric() || b"_(), ".contains(&b)) {
        return Err(format!("{} has a column type DataBrain doesn't read ({t})", f.name()));
    }
    let base = dbx_base(t);
    let ok = match lit_kind(f) {
        Lit::Text => matches!(base.as_str(), "string" | "varchar" | "char"),
        Lit::Number => DBX_INTEGRAL.contains(&base.as_str()) || matches!(base.as_str(), "decimal" | "dec" | "numeric" | "double" | "float" | "real"),
        Lit::Float => matches!(base.as_str(), "decimal" | "dec" | "numeric" | "double" | "float" | "real"),
        Lit::Bool => base == "boolean",
        Lit::Binary => base == "binary",
        Lit::Date => matches!(base.as_str(), "date" | "timestamp" | "timestamp_ntz"),
        Lit::Timestamp => matches!(base.as_str(), "timestamp" | "timestamp_ntz"),
        Lit::TimestampTz => base == "timestamp",
    };
    if ok { Ok(()) } else { Err(format!("{} ({}) into a {t} column", f.name(), f.data_type())) }
}

/// Whether column `f` of the rows can go into a Postgres column of type
/// `target` (as `format_type` names it) by COPY with the same result as its
/// INSERT literal; `Err` says why not. An INSERT writes numbers and
/// booleans bare and the database casts them (`12.5` into an integer is
/// rounded, `TRUE` into text is `true`); COPY reads text with the column
/// type's input, which refuses or keeps other text. Quoted literals (text,
/// dates, timestamps) are read by that same input.
pub fn pg_copy_fits(f: &Field, target: &str) -> std::result::Result<(), String> {
    let t = target.trim().to_ascii_lowercase();
    let t = t.split('(').next().unwrap_or_default().trim();
    let integer = matches!(t, "smallint" | "integer" | "bigint");
    let exact = matches!(t, "numeric" | "real" | "double precision");
    let text = matches!(t, "text" | "character varying");
    let ok = match lit_kind(f) {
        Lit::Number if f.data_type().is_integer() => integer || exact,
        Lit::Number | Lit::Float => exact,
        Lit::Bool => t == "boolean" || text,
        // Into text, an INSERT's bytea is cast by the server's
        // `bytea_output` (hex or escape): COPY can't know which.
        Lit::Binary => t == "bytea",
        _ => true,
    };
    if ok { Ok(()) } else { Err(format!("{} ({}) into {target}", f.name(), f.data_type())) }
}

/// INSERT statements for every row, several rows per statement (the dialect's default batch size).
pub fn insert_statements(dialect: ConnectorKind, table: &str, schema: &SchemaRef, batches: &[RecordBatch]) -> Result<Vec<String>> {
    let b = InsertSql::new(dialect, table, schema, batch_limits(dialect).default_size());
    let mut out = Vec::new();
    for batch in batches {
        let mut at = 0;
        while let Some((sql, n)) = b.next(batch, at, None)? {
            out.push(sql);
            at += n;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    const MB: usize = 1024 * KB;
    use super::*;
    use databrain_connector_core::arrow::array::{BooleanArray, Int64Array, StringArray, TimestampMicrosecondArray};
    use databrain_connector_core::arrow::datatypes::{Schema, TimeUnit};
    use std::sync::Arc;

    fn rows(n: usize) -> (SchemaRef, Vec<RecordBatch>) {
        let schema: SchemaRef = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, true),
            Field::new("ok", DataType::Boolean, true),
            Field::new("at", DataType::Timestamp(TimeUnit::Microsecond, None), true),
        ]));
        let b = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from_iter_values(0..n as i64)),
                Arc::new(StringArray::from_iter((0..n).map(|i| if i == 1 { None } else { Some(format!("it's {i}")) }))),
                Arc::new(BooleanArray::from_iter((0..n).map(|i| Some(i % 2 == 0)))),
                Arc::new(TimestampMicrosecondArray::from_iter((0..n).map(|_| Some(1_767_225_600_000_000)))),
            ],
        )
        .unwrap();
        (schema, vec![b])
    }

    #[test]
    fn create_and_insert_per_dialect() {
        let (schema, batches) = rows(2);
        assert_eq!(
            create_table_sql(ConnectorKind::Postgres, "app.t", &schema),
            r#"CREATE TABLE "app"."t" ("id" BIGINT NOT NULL, "name" TEXT, "ok" BOOLEAN, "at" TIMESTAMP)"#
        );
        assert_eq!(
            create_table_sql(ConnectorKind::Mssql, "t", &schema),
            "CREATE TABLE [t] ([id] BIGINT NOT NULL, [name] NVARCHAR(MAX), [ok] BIT, [at] DATETIME2)"
        );
        assert_eq!(
            insert_statements(ConnectorKind::Postgres, "t", &schema, &batches).unwrap(),
            vec![r#"INSERT INTO "t" ("id", "name", "ok", "at") VALUES (0, 'it''s 0', TRUE, '2026-01-01 00:00:00'), (1, NULL, FALSE, '2026-01-01 00:00:00')"#]
        );
        let ora = insert_statements(ConnectorKind::Oracle, "t", &schema, &batches).unwrap();
        assert!(
            ora[0].starts_with(r#"INSERT ALL INTO "t" ("id", "name", "ok", "at") VALUES (0, 'it''s 0', 1, TIMESTAMP '2026-01-01 00:00:00') INTO"#),
            "{}",
            ora[0]
        );
        assert!(ora[0].ends_with("SELECT 1 FROM DUAL"));
        assert!(insert_statements(ConnectorKind::Mssql, "t", &schema, &batches).unwrap()[0].contains("N'it''s 0', 1,"));
        assert!(drop_table_sql(ConnectorKind::Oracle, "t").contains("-942"));
        assert!(create_table_if_missing_sql(ConnectorKind::Sqlite, "t", &schema).starts_with(r#"CREATE TABLE IF NOT EXISTS "t" ("id" INTEGER NOT NULL"#));
        assert!(
            create_table_if_missing_sql(ConnectorKind::Mssql, "dbo.t", &schema).starts_with("IF OBJECT_ID(N'[dbo].[t]', N'U') IS NULL CREATE TABLE [dbo].[t]")
        );
        assert!(create_table_if_missing_sql(ConnectorKind::Oracle, "t", &schema).contains("-955"));
    }

    /// ClickHouse: MergeTree tables with explicit NULL columns, TRUNCATE for
    /// "Delete rows, then insert", timestamps with offsets parsed best-effort,
    /// and no Update/Merge by key.
    #[test]
    fn clickhouse_load_sql() {
        use ConnectorKind as K;
        let (schema, batches) = rows(2);
        assert_eq!(
            create_table_sql(K::Clickhouse, "db.t", &schema),
            "CREATE TABLE `db`.`t` (`id` Int64 NOT NULL, `name` String NULL, `ok` Bool NULL, `at` DateTime64(6) NULL) ENGINE = MergeTree ORDER BY tuple()"
        );
        assert!(create_table_if_missing_sql(K::Clickhouse, "t", &schema).starts_with("CREATE TABLE IF NOT EXISTS `t` ("));
        assert_eq!(delete_all_sql(K::Clickhouse, "db.t"), "TRUNCATE TABLE `db`.`t`");
        assert_eq!(delete_all_sql(K::Postgres, "t"), r#"DELETE FROM "t""#);
        assert_eq!(
            insert_statements(K::Clickhouse, "t", &schema, &batches).unwrap(),
            vec![
                "INSERT INTO `t` (`id`, `name`, `ok`, `at`) SETTINGS date_time_input_format = 'best_effort' VALUES (0, 'it''s 0', TRUE, '2026-01-01 00:00:00'), (1, NULL, FALSE, '2026-01-01 00:00:00')"
            ]
        );
        assert_eq!(literal(Some("a\\b"), Lit::Text, K::Clickhouse), "'a\\\\b'", "backslashes are escaped");
        assert_eq!(literal(Some("0aff"), Lit::Binary, K::Clickhouse), "unhex('0aff')");
        // One text escaper with parameters: a trailing backslash can't close the quote.
        assert_eq!(literal(Some("a\\"), Lit::Text, K::Snowflake), "'a\\\\'");
        assert_eq!(literal(Some("it's"), Lit::Text, K::Bigquery), "'it\\'s'");
        // BigQuery: a bare 12.5 is FLOAT64 and '12.5' a STRING; NUMERIC columns take neither.
        assert_eq!(literal(Some("12.50"), Lit::Number, K::Bigquery), "NUMERIC '12.50'");
        assert_eq!(literal(Some("-7"), Lit::Number, K::Bigquery), "-7");
        assert_eq!(literal(Some("123456789012345678901234567890.5"), Lit::Number, K::Bigquery), "BIGNUMERIC '123456789012345678901234567890.5'");
        assert_eq!(literal(Some("1.0123456789"), Lit::Number, K::Bigquery), "BIGNUMERIC '1.0123456789'");
        assert_eq!(literal(Some("99999999999999999999"), Lit::Number, K::Bigquery), "NUMERIC '99999999999999999999'", "past INT64");
        assert_eq!(literal(Some("1.5"), Lit::Float, K::Bigquery), "1.5");
        assert_eq!(literal(Some("12.50"), Lit::Number, K::Postgres), "12.50");
        assert_eq!(literal(Some("it's"), Lit::Text, K::Mssql), "N'it''s'");
        assert!(is_load_target(K::Clickhouse));
        assert!(!keyed_loads(K::Clickhouse));
        assert!(keyed_loads(K::Postgres));
    }

    /// Sync's delete of the rows missing from the stage, per dialect.
    #[test]
    fn delete_missing_per_dialect() {
        use ConnectorKind as K;
        let keys = vec!["id".to_string(), "day".to_string()];
        assert_eq!(
            delete_missing_sql(K::Postgres, "app.t", "s", Keys::nullable(&keys)),
            r#"DELETE FROM "app"."t" WHERE NOT EXISTS (SELECT 1 FROM "s" src WHERE src."id" IS NOT DISTINCT FROM "t"."id" AND src."day" IS NOT DISTINCT FROM "t"."day")"#
        );
        assert_eq!(
            delete_missing_sql(K::Mssql, "dbo.t", "#s", Keys::nullable(&keys[..1])),
            "DELETE tg FROM [dbo].[t] AS tg WHERE NOT EXISTS (SELECT 1 FROM [#s] AS src WHERE (src.[id] = tg.[id] OR (src.[id] IS NULL AND tg.[id] IS NULL)))"
        );
        assert_eq!(
            delete_missing_sql(K::Bigquery, "ds.t", "ds.s", Keys::nullable(&keys[..1])),
            "DELETE `ds`.`t` tg WHERE NOT EXISTS (SELECT 1 FROM `ds`.`s` AS src WHERE src.`id` IS NOT DISTINCT FROM tg.`id`)"
        );
        assert!(
            delete_missing_sql(K::Mysql, "t", "s", Keys::nullable(&keys[..1]))
                .starts_with("DELETE FROM `t` WHERE NOT EXISTS (SELECT 1 FROM `s` src WHERE src.`id` <=> `t`.`id`)")
        );
    }

    /// Timestamps with a time zone load into Oracle as typed literals (text
    /// was rejected); other databases take the text.
    #[test]
    fn timestamptz_literals() {
        use ConnectorKind as K;
        assert_eq!(literal(Some("2026-01-02 03:04:05.678+02:00"), Lit::TimestampTz, K::Oracle), "TIMESTAMP '2026-01-02 03:04:05.678 +02:00'");
        assert_eq!(literal(Some("2026-01-02 03:04:05-08:00"), Lit::TimestampTz, K::Oracle), "TIMESTAMP '2026-01-02 03:04:05 -08:00'");
        // Other spellings of the offset: UTC as Z, no colon, hours only, already spaced.
        for (v, want) in [
            ("2026-01-02 03:04:05.678Z", "2026-01-02 03:04:05.678 +00:00"),
            ("2026-01-02T03:04:05z", "2026-01-02T03:04:05 +00:00"),
            ("2026-01-02 03:04:05+0530", "2026-01-02 03:04:05 +05:30"),
            ("2026-01-02 03:04:05-03", "2026-01-02 03:04:05 -03:00"),
            ("2026-01-02 03:04:05 +02:00", "2026-01-02 03:04:05 +02:00"),
            ("2026-01-02 03:04:05", "2026-01-02 03:04:05"),
        ] {
            assert_eq!(spaced_offset(v), want, "{v}");
        }
        assert_eq!(literal(Some("2026-01-02 03:04:05.678+02:00"), Lit::TimestampTz, K::Postgres), "'2026-01-02 03:04:05.678+02:00'");
        let f = Field::new("at", DataType::Timestamp(databrain_connector_core::arrow::datatypes::TimeUnit::Microsecond, Some("+02:00".into())), true);
        assert!(matches!(lit_kind(&f), Lit::TimestampTz));
        assert_eq!(column_type(K::Oracle, &f), "TIMESTAMP WITH TIME ZONE");
    }

    /// NaN and ±inf: never quoted text into a numeric column (SQL Server and
    /// MySQL refuse it, SQLite stores the text). Postgres, DuckDB and
    /// ClickHouse float columns keep them as their own float values;
    /// elsewhere NULL. Typed into a grid cell, they are refused where they
    /// would become NULL.
    #[test]
    fn non_finite_floats() {
        use ConnectorKind as K;
        use databrain_connector_core::arrow::array::Float64Array;
        let schema: SchemaRef = Arc::new(Schema::new(vec![Field::new("x", DataType::Float64, true)]));
        let b = RecordBatch::try_new(
            schema.clone(),
            vec![Arc::new(Float64Array::from(vec![Some(f64::NAN), Some(f64::INFINITY), Some(f64::NEG_INFINITY), Some(-1.5), None]))],
        )
        .unwrap();
        for k in [K::Mssql, K::Mysql, K::Sqlite, K::Oracle, K::Snowflake, K::Bigquery, K::Databricks] {
            let sql = insert_statements(k, "t", &schema, std::slice::from_ref(&b)).unwrap().join(" ");
            assert!(!sql.contains('\''), "{k:?}: no quoted text: {sql}");
            assert_eq!(sql.matches("NULL").count(), 4, "{k:?}: {sql}");
            assert!(sql.contains("-1.5"), "{k:?}: {sql}");
        }
        assert_eq!(
            insert_statements(K::Postgres, "t", &schema, std::slice::from_ref(&b)).unwrap(),
            vec![r#"INSERT INTO "t" ("x") VALUES ('NaN'::float8), ('Infinity'::float8), ('-Infinity'::float8), (-1.5), (NULL)"#]
        );
        assert!(insert_statements(K::Clickhouse, "t", &schema, &[b]).unwrap()[0].ends_with("VALUES (nan), (inf), (-inf), (-1.5), (NULL)"));
        assert_eq!(literal(Some("NaN"), Lit::Float, K::Duckdb), "'NaN'::DOUBLE");
        assert_eq!(literal(Some("-inf"), Lit::Float, K::Duckdb), "'-Infinity'::DOUBLE");
        // A decimal or integer column has no NaN of its own type: NULL.
        assert_eq!(literal(Some("NaN"), Lit::Number, K::Postgres), "NULL");
        assert_eq!(literal(Some("+Infinity"), Lit::Number, K::Mssql), "NULL");
        // Numbers stay numbers, also ones too large for an f64 (decimals).
        assert_eq!(literal(Some("1e400"), Lit::Number, K::Mssql), "1e400");
        assert_eq!(literal(Some("-0.0"), Lit::Float, K::Postgres), "-0.0");
        assert_eq!(literal(Some("12345678901234567890.123"), Lit::Number, K::Mysql), "12345678901234567890.123");
        let f = Field::new("x", DataType::Float64, true);
        assert!(edit_literal(&f, Some("NaN"), K::Mssql).unwrap_err().contains("can't be stored"));
        assert_eq!(edit_literal(&f, Some(" -inf "), K::Postgres).unwrap(), "'-Infinity'::float8");
        assert_eq!(edit_literal(&f, Some(" 2.5 "), K::Mssql).unwrap(), "2.5");
        assert!(edit_literal(&Field::new("n", DataType::Int64, true), Some("inf"), K::Postgres).is_err());
    }

    /// Oracle array INSERTs: each value as its INSERT literal's text, turned
    /// into its type with explicit formats in the SQL.
    #[test]
    fn oracle_array_insert_binds_the_literals_text() {
        use databrain_connector_core::arrow::array::{BinaryArray, Date32Array, Date64Array, Float64Array, TimestampMicrosecondArray};
        let schema: SchemaRef = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, true),
            Field::new("ok", DataType::Boolean, true),
            Field::new("f", DataType::Float64, true),
            Field::new("d", DataType::Date32, true),
            Field::new("d64", DataType::Date64, true),
            Field::new("ts", DataType::Timestamp(TimeUnit::Microsecond, None), true),
            Field::new("tz", DataType::Timestamp(TimeUnit::Microsecond, Some("+02:00".into())), true),
            Field::new("b", DataType::Binary, true),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from(vec![1, 2])),
                Arc::new(StringArray::from(vec![Some("it's"), None])),
                Arc::new(BooleanArray::from(vec![Some(true), Some(false)])),
                Arc::new(Float64Array::from(vec![Some(2.5), Some(f64::NAN)])),
                Arc::new(Date32Array::from(vec![Some(20_000), None])),
                Arc::new(Date64Array::from(vec![Some(1_700_000_000_000), None])),
                Arc::new(TimestampMicrosecondArray::from(vec![Some(1_700_000_000_123_456), None])),
                Arc::new(TimestampMicrosecondArray::from(vec![Some(1_700_000_000_000_000), None]).with_timezone("+02:00")),
                Arc::new(BinaryArray::from(vec![Some(&b"\x00\xff"[..]), None])),
            ],
        )
        .unwrap();
        assert!(ArrayInsert::new(ConnectorKind::Postgres, "t", &schema).is_none(), "Oracle only");
        let ai = ArrayInsert::new(ConnectorKind::Oracle, "app.t", &schema).unwrap();
        assert_eq!(
            ai.sql,
            "INSERT INTO \"app\".\"t\" (\"id\", \"name\", \"ok\", \"f\", \"d\", \"d64\", \"ts\", \"tz\", \"b\") VALUES (:1, :2, :3, :4, \
             TO_DATE(:5, 'YYYY-MM-DD'), TO_TIMESTAMP(:6, 'YYYY-MM-DD HH24:MI:SS.FF'), TO_TIMESTAMP(:7, 'YYYY-MM-DD HH24:MI:SS.FF'), \
             TO_TIMESTAMP_TZ(:8, 'YYYY-MM-DD HH24:MI:SS.FF TZH:TZM'), HEXTORAW(:9))"
        );
        let (cols, n) = ai.next(&batch, 0, 10, MB, '.').unwrap().unwrap();
        assert_eq!(n, 2);
        let s = |v: &str| Some(v.to_string());
        assert_eq!(cols[0], vec![s("1"), s("2")]);
        assert_eq!(cols[1], vec![s("it's"), None], "text as it is: no SQL quoting");
        assert_eq!(cols[2], vec![s("1"), s("0")], "booleans as Oracle's INSERT writes them");
        assert_eq!(cols[3], vec![s("2.5"), None], "NaN: NULL, as the INSERT writes it");
        assert_eq!(cols[4], vec![s("2024-10-04"), None]);
        assert!(cols[5][0].as_deref().is_some_and(|v| v.starts_with("2023-11-14 ")), "{:?}", cols[5]);
        assert_eq!(cols[6][0].as_deref(), Some("2023-11-14 22:13:20.123456"));
        assert!(cols[7][0].as_deref().is_some_and(|v| v.ends_with(" +02:00")), "the offset after a space: {:?}", cols[7]);
        assert_eq!(cols[8], vec![s("00ff"), None]);
        // At most `max_rows`, or about `max_bytes` (at least one row).
        assert_eq!(ai.next(&batch, 0, 1, MB, '.').unwrap().unwrap().1, 1);
        assert_eq!(ai.next(&batch, 0, 10, 1, '.').unwrap().unwrap().1, 1);
        assert!(ai.next(&batch, 2, 10, MB, '.').unwrap().is_none());
        // A session whose decimal separator is ',': numbers written with it.
        let (cols, _) = ai.next(&batch, 0, 10, MB, ',').unwrap().unwrap();
        assert_eq!(cols[3][0].as_deref(), Some("2,5"));
        assert_eq!(cols[1][0].as_deref(), Some("it's"), "text as it is");
    }

    /// An array call stays within its request size counted as JSON (quotes
    /// and backslashes doubled, control characters 6 bytes) and within its
    /// bind buffers (rows × each column's longest value).
    #[test]
    fn array_calls_count_escaped_text_and_bind_buffers() {
        assert_eq!(json_len("ab"), 5);
        assert_eq!(json_len(r#"a"b\c"#), 3 + 7);
        assert_eq!(json_len("\u{1}\n"), 3 + 6 + 2);
        let schema: SchemaRef = Arc::new(Schema::new(vec![Field::new("s", DataType::Utf8, true)]));
        let quoted = "\"\\".repeat(50); // 100 bytes of text, 200 as JSON
        let batch = RecordBatch::try_new(schema.clone(), vec![Arc::new(StringArray::from(vec![quoted.as_str(); 10]))]).unwrap();
        let ai = ArrayInsert::new(ConnectorKind::Oracle, "t", &schema).unwrap();
        // 1,000 bytes: 4 rows of 203 bytes as JSON (not 9 of 100 as text).
        assert_eq!(ai.next(&batch, 0, 100, 1000, '.').unwrap().unwrap().1, 4);
        // One 8 KB value among short ones: rows × 8 KB within 16 MB.
        let mut values = vec!["x".to_string(); 10_000];
        values[0] = "y".repeat(8 * 1024);
        let batch = RecordBatch::try_new(schema.clone(), vec![Arc::new(StringArray::from(values))]).unwrap();
        let (_, n) = ai.next(&batch, 0, 10_000, 32 * MB, '.').unwrap().unwrap();
        assert_eq!(n, ARRAY_SLOT_BYTES / (8 * 1024));
        assert!(n * 8 * 1024 <= ARRAY_SLOT_BYTES);
    }

    /// Snowflake array binds: plain placeholders, each value the text its
    /// literal holds (cast as the literal is), at most 60,000 values a call.
    #[test]
    fn snowflake_array_insert_binds_text() {
        use databrain_connector_core::arrow::array::Float64Array;
        let schema: SchemaRef = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, true),
            Field::new("ok", DataType::Boolean, true),
            Field::new("f", DataType::Float64, true),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from(vec![1, 2])),
                Arc::new(StringArray::from(vec![Some("it's \\ x"), None])),
                Arc::new(BooleanArray::from(vec![Some(true), Some(false)])),
                Arc::new(Float64Array::from(vec![Some(2.5), Some(f64::INFINITY)])),
            ],
        )
        .unwrap();
        let ai = ArrayInsert::new(ConnectorKind::Snowflake, "db.s.t", &schema).unwrap();
        assert_eq!(ai.sql, r#"INSERT INTO "db"."s"."t" ("id", "name", "ok", "f") VALUES (?, ?, ?, ?)"#);
        assert_eq!(ai.rows_per_call(None), 10_000, "4 columns: 15,000 rows would pass 60,000 values; 10,000 at most");
        assert_eq!(ai.rows_per_call(Some(4_000)), 1_000, "the session's limit");
        assert_eq!(ai.bind_types(), vec!["FIXED", "TEXT", "BOOLEAN", "REAL"]);
        let wide: SchemaRef = Arc::new(Schema::new((0..30).map(|i| Field::new(format!("c{i}"), DataType::Utf8, true)).collect::<Vec<_>>()));
        assert_eq!(ArrayInsert::new(ConnectorKind::Snowflake, "t", &wide).unwrap().rows_per_call(None), 2_000);
        let (cols, n) = ai.next(&batch, 0, ai.rows_per_call(None), MB, '.').unwrap().unwrap();
        assert_eq!(n, 2);
        let s = |v: &str| Some(v.to_string());
        assert_eq!(cols[1], vec![s("it's \\ x"), None], "text as it is: no SQL quoting or escapes");
        assert_eq!(cols[2], vec![s("true"), s("false")]);
        assert_eq!(cols[3], vec![s("2.5"), None], "inf: NULL, as Snowflake's INSERT writes it");
    }

    /// A Databricks bulk load: one INSERT … SELECT casting each text column
    /// as its INSERT literal is, and Parquet files of the literals' text.
    #[test]
    fn dbx_stage_writes_the_inserts_text_as_parquet() {
        use databrain_connector_core::arrow::array::{BinaryArray, Float64Array};
        use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
        let schema: SchemaRef = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, true),
            Field::new("ok", DataType::Boolean, true),
            Field::new("f", DataType::Float64, true),
            Field::new("b", DataType::Binary, true),
            Field::new("weird `col`", DataType::Utf8, true),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from(vec![1, 2, 3])),
                Arc::new(StringArray::from(vec![Some("a'b\\c\n"), None, Some("ünï")])),
                Arc::new(BooleanArray::from(vec![Some(true), Some(false), None])),
                Arc::new(Float64Array::from(vec![Some(f64::NAN), Some(2.5), Some(f64::INFINITY)])),
                Arc::new(BinaryArray::from(vec![Some(&b"\x00\xff"[..]), None, Some(&b"A"[..])])),
                Arc::new(StringArray::from(vec![Some("x"), Some("y"), Some("z")])),
            ],
        )
        .unwrap();
        let target = ["int", "varchar(10)", "boolean", "double", "binary", "string"].map(String::from);
        let st = DbxStage::new("main.s.t", &schema, &target);
        assert_eq!(
            st.sql,
            "INSERT INTO `main`.`s`.`t` (`id`, `name`, `ok`, `f`, `b`, `weird ``col```) SELECT CAST(CAST(c0 AS DECIMAL(38, 10)) AS int), CAST(c1 AS STRING), \
             CAST(c2 AS boolean), CAST(c3 AS double), unhex(c4), CAST(c5 AS STRING) FROM read_files({databrain_bulk_files}, format => 'parquet', \
             schema => 'c0 STRING, c1 STRING, c2 STRING, c3 STRING, c4 STRING, c5 STRING')"
        );
        let (file, n) = st.next(&batch, 0, MB).unwrap().unwrap();
        assert_eq!(n, 3);
        let mut tmp = tempfile::tempfile().unwrap();
        std::io::Write::write_all(&mut tmp, &file).unwrap();
        let read: Vec<RecordBatch> = ParquetRecordBatchReaderBuilder::try_new(tmp).unwrap().build().unwrap().map(|b| b.unwrap()).collect();
        let col = |i: usize| -> Vec<Option<String>> {
            read.iter()
                .flat_map(|b| b.column(i).as_any().downcast_ref::<StringArray>().unwrap().iter().map(|v| v.map(str::to_string)).collect::<Vec<_>>())
                .collect()
        };
        let s = |v: &str| Some(v.to_string());
        assert_eq!(col(0), vec![s("1"), s("2"), s("3")]);
        assert_eq!(col(1), vec![s("a'b\\c\n"), None, s("ünï")], "text as it is: no SQL quoting");
        assert_eq!(col(2), vec![s("true"), s("false"), None]);
        assert_eq!(col(3), vec![None, s("2.5"), None], "NaN and inf: NULL, as the INSERT writes them");
        assert_eq!(col(4), vec![s("00ff"), None, s("41")], "binary as hex, unhex()ed back");
        // A file ends past the byte size (at least one row); the next starts there.
        let (_, first) = st.next(&batch, 0, 1).unwrap().unwrap();
        assert_eq!(first, 1);
        assert_eq!(st.next(&batch, 2, 1).unwrap().unwrap().1, 1);
        assert!(st.next(&batch, 3, 1).unwrap().is_none());
    }

    /// Which columns a Databricks bulk load takes (the cast gives what the
    /// INSERT literal would), and which go by INSERTs.
    #[test]
    fn dbx_fits_per_type() {
        let f = |t: DataType| Field::new("c", t, true);
        for (field, target) in [
            (f(DataType::Utf8), "string"),
            (f(DataType::Utf8), "varchar(20)"),
            (f(DataType::Int64), "bigint"),
            (f(DataType::Int64), "decimal(10,2)"),
            (f(DataType::Float64), "double"),
            (f(DataType::Boolean), "boolean"),
            (f(DataType::Binary), "binary"),
            (f(DataType::Date32), "date"),
            (f(DataType::Date32), "timestamp"),
            (f(DataType::Timestamp(TimeUnit::Microsecond, None)), "timestamp_ntz"),
            (f(DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()))), "timestamp"),
        ] {
            assert!(dbx_fits(&field, target).is_ok(), "{field:?} into {target}");
        }
        for (field, target) in [
            (f(DataType::Utf8), "int"),
            (f(DataType::Float64), "bigint"),
            (f(DataType::Boolean), "int"),
            (f(DataType::Int64), "string"),
            (f(DataType::Timestamp(TimeUnit::Microsecond, None)), "date"),
            (f(DataType::Utf8), "array<string>"),
            (f(DataType::Utf8), "string'); drop table t; --"),
            (f(DataType::Utf8), ""),
        ] {
            assert!(dbx_fits(&field, target).is_err(), "{field:?} into {target}");
        }
    }

    /// COPY text: NULL as `\N`, the escapes COPY reads (backslash, tab,
    /// newline, carriage return), NaN/inf as the INSERT literal has them,
    /// booleans and binary as their literals' text.
    #[test]
    fn pg_copy_text() {
        use databrain_connector_core::arrow::array::{BinaryArray, Decimal128Array, Float64Array};
        let schema: SchemaRef = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, true),
            Field::new("ok", DataType::Boolean, true),
            Field::new("f", DataType::Float64, true),
            Field::new("d", DataType::Decimal128(10, 2), true),
            Field::new("b", DataType::Binary, true),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from(vec![1, 2, 3])),
                Arc::new(StringArray::from(vec![Some("a\tb\nc\\d\re"), None, Some("ünï 'q' \"x\"")])),
                Arc::new(BooleanArray::from(vec![Some(true), Some(false), None])),
                Arc::new(Float64Array::from(vec![Some(f64::NAN), Some(f64::NEG_INFINITY), Some(2.5)])),
                Arc::new(Decimal128Array::from(vec![Some(150), None, Some(-325)]).with_precision_and_scale(10, 2).unwrap()),
                Arc::new(BinaryArray::from(vec![Some(&b"\x00\xff"[..]), None, Some(&b""[..])])),
            ],
        )
        .unwrap();
        let c = PgCopy::new("s.t", &schema);
        assert_eq!(c.sql, r#"COPY "s"."t" ("id", "name", "ok", "f", "d", "b") FROM STDIN"#);
        let (data, n) = c.next(&batch, 0, usize::MAX).unwrap().unwrap();
        assert_eq!(n, 3);
        assert_eq!(
            String::from_utf8(data).unwrap(),
            "1\ta\\tb\\nc\\\\d\\re\ttrue\tNaN\t1.50\t\\\\x00ff\n\
             2\t\\N\tfalse\t-Infinity\t\\N\t\\N\n\
             3\tünï 'q' \"x\"\t\\N\t2.5\t-3.25\t\\\\x\n"
        );
        // The INSERT literals of the same rows (what COPY must match).
        let sql = insert_statements(ConnectorKind::Postgres, "s.t", &schema, std::slice::from_ref(&batch)).unwrap().join("");
        assert!(sql.contains("'NaN'::float8") && sql.contains("'-Infinity'::float8") && sql.contains(r"'\x00ff'::bytea") && sql.contains("TRUE"), "{sql}");
        // By size: at least one row a chunk, then as many as fit.
        let (_, n) = c.next(&batch, 0, 1).unwrap().unwrap();
        assert_eq!(n, 1);
        let (_, n) = c.next(&batch, 1, 1).unwrap().unwrap();
        assert_eq!(n, 1);
        assert!(c.next(&batch, 3, 1).unwrap().is_none());
        // NaN in an exact number is NULL, as its INSERT literal.
        let mut out = Vec::new();
        pg_copy_value(&mut out, Some("NaN"), Lit::Number);
        assert_eq!(out, br"\N");
        // A negative zero is written as 0, as the INSERT's `-0.0` stores it;
        // other negatives keep their sign.
        let tiny = format!("-0.{}1", "0".repeat(400));
        for (v, want) in [
            ("-0", "0"),
            ("-0.0", "0.0"),
            ("-0.000", "0.000"),
            ("-0e5", "0e5"),
            ("-0.5", "-0.5"),
            ("-0.0001", "-0.0001"),
            ("-1e-400", "-1e-400"),
            (tiny.as_str(), tiny.as_str()),
            ("0", "0"),
        ] {
            let mut out = Vec::new();
            pg_copy_value(&mut out, Some(v), Lit::Float);
            assert_eq!(String::from_utf8(out).unwrap(), want, "{v}");
        }
    }

    /// Which columns may go by COPY: where the INSERT's bare literal and
    /// COPY's text input give the same value.
    #[test]
    fn pg_copy_fits_per_type() {
        let f = |t: DataType| Field::new("c", t, true);
        // Integers: integer and exact/float columns, not text (`1e20` vs its cast) or json.
        assert!(pg_copy_fits(&f(DataType::Int64), "bigint").is_ok());
        assert!(pg_copy_fits(&f(DataType::Int32), "numeric(12,2)").is_ok());
        assert!(pg_copy_fits(&f(DataType::Int32), "double precision").is_ok());
        assert!(pg_copy_fits(&f(DataType::Int32), "text").is_err());
        assert!(pg_copy_fits(&f(DataType::Int32), "jsonb").is_err());
        // Fractions into an integer are rounded by the INSERT's cast, refused by COPY: INSERTs.
        assert!(pg_copy_fits(&f(DataType::Float64), "integer").unwrap_err().contains("integer"));
        assert!(pg_copy_fits(&f(DataType::Decimal128(10, 2)), "bigint").is_err());
        assert!(pg_copy_fits(&f(DataType::Float64), "numeric").is_ok());
        assert!(pg_copy_fits(&f(DataType::Float32), "real").is_ok());
        // Booleans: boolean or text (both `true`); not an integer column.
        assert!(pg_copy_fits(&f(DataType::Boolean), "boolean").is_ok());
        assert!(pg_copy_fits(&f(DataType::Boolean), "character varying(10)").is_ok());
        assert!(pg_copy_fits(&f(DataType::Boolean), "integer").is_err());
        // Binary: bytea only (into text, the INSERT's cast follows the server's bytea_output).
        assert!(pg_copy_fits(&f(DataType::Binary), "bytea").is_ok());
        assert!(pg_copy_fits(&f(DataType::Binary), "text").is_err());
        assert!(pg_copy_fits(&f(DataType::Binary), "uuid").is_err());
        // Quoted literals are read by the column's own input either way.
        for t in ["date", "timestamp with time zone", "uuid", "jsonb", "integer", "text[]"] {
            assert!(pg_copy_fits(&f(DataType::Utf8), t).is_ok(), "{t}");
        }
        assert!(pg_copy_fits(&f(DataType::Date32), "date").is_ok());
    }

    /// Dates: Oracle gets typed literals (DATE for a date, TIMESTAMP when the
    /// value has a time, as a Date64 does); other databases take the text.
    #[test]
    fn date_literals() {
        use ConnectorKind as K;
        assert_eq!(literal(Some("2026-01-02"), Lit::Date, K::Oracle), "DATE '2026-01-02'");
        assert_eq!(literal(Some("2026-01-02 00:00:00"), Lit::Date, K::Oracle), "TIMESTAMP '2026-01-02 00:00:00'");
        assert_eq!(literal(Some("2026-01-02"), Lit::Date, K::Postgres), "'2026-01-02'");
        assert_eq!(literal(Some("2026-01-02"), Lit::Date, K::Mssql), "N'2026-01-02'");
    }

    /// Update/Merge/Sync match NULL keys too (`NULL = NULL` is never true, so
    /// a Merge inserted those rows again on every run), per dialect.
    #[test]
    fn null_safe_key_match() {
        use ConnectorKind as K;
        let cols: Vec<String> = ["id", "name"].map(String::from).to_vec();
        let keys = vec!["id".to_string()];
        let pg = merge_from_stage_sql(K::Postgres, "t", "s", &cols, Keys::nullable(&keys));
        assert!(pg[0].ends_with(r#"WHERE tg."id" IS NOT DISTINCT FROM src."id""#), "{}", pg[0]);
        assert!(pg[1].ends_with(r#"(SELECT 1 FROM "t" AS tg WHERE tg."id" IS NOT DISTINCT FROM src."id")"#), "{}", pg[1]);
        let my = merge_from_stage_sql(K::Mysql, "t", "s", &cols, Keys::nullable(&keys));
        assert!(my[0].contains("ON tg.`id` <=> src.`id` SET"), "{}", my[0]);
        assert!(my[1].ends_with("WHERE tg.`id` <=> src.`id`)"), "{}", my[1]);
        let lite = merge_from_stage_sql(K::Sqlite, "t", "s", &cols, Keys::nullable(&keys));
        assert!(lite[1].ends_with(r#"WHERE tg."id" IS src."id")"#), "{}", lite[1]);
        let ms = &merge_from_stage_sql(K::Mssql, "t", "#s", &cols, Keys::nullable(&keys))[0];
        assert!(ms.contains("ON (tg.[id] = src.[id] OR (tg.[id] IS NULL AND src.[id] IS NULL)) WHEN MATCHED"), "{ms}");
        let ora = &update_from_stage_sql(K::Oracle, "T", "S", &cols, Keys::nullable(&keys))[0];
        assert!(ora.contains(r#"ON ((tg."id" = src."id" OR (tg."id" IS NULL AND src."id" IS NULL)))"#), "{ora}");
        for k in [K::Snowflake, K::Databricks, K::Bigquery, K::Duckdb] {
            let sql = merge_from_stage_sql(k, "a.t", "a.s", &cols, Keys::nullable(&keys)).join(" ");
            assert!(sql.contains(" IS NOT DISTINCT FROM src."), "{k:?}: {sql}");
        }
        // Two keys: each matched null-safely, joined by AND.
        let two = vec!["id".to_string(), "name".to_string()];
        assert!(
            merge_from_stage_sql(K::Postgres, "t", "s", &cols, Keys::nullable(&two))[0]
                .ends_with(r#"WHERE tg."id" IS NOT DISTINCT FROM src."id" AND tg."name" IS NOT DISTINCT FROM src."name")"#)
        );
        // NOT NULL keys (primary keys) keep `=`, which can use an index or a hash join.
        let id_not_null = Keys { all: &two, nullable: &two[1..] };
        assert!(
            merge_from_stage_sql(K::Postgres, "t", "s", &cols, id_not_null)[0]
                .ends_with(r#"WHERE tg."id" = src."id" AND tg."name" IS NOT DISTINCT FROM src."name")"#)
        );
        assert_eq!(
            delete_missing_sql(K::Postgres, "t", "s", Keys { all: &keys, nullable: &[] }),
            r#"DELETE FROM "t" WHERE NOT EXISTS (SELECT 1 FROM "s" src WHERE src."id" = "t"."id")"#
        );
        // Sync keeps a target row whose NULL key is among the staged rows.
        assert_eq!(
            delete_missing_sql(K::Postgres, "t", "s", Keys::nullable(&keys)),
            r#"DELETE FROM "t" WHERE NOT EXISTS (SELECT 1 FROM "s" src WHERE src."id" IS NOT DISTINCT FROM "t"."id")"#
        );
        assert!(
            delete_missing_sql(K::Mssql, "t", "#s", Keys::nullable(&keys)).ends_with("WHERE (src.[id] = tg.[id] OR (src.[id] IS NULL AND tg.[id] IS NULL)))")
        );
    }

    #[test]
    fn batches_rows() {
        // MySQL's default is 5,000 rows: 5,000 + 5,000 + 2,001.
        let (schema, batches) = rows(12_001);
        let s = insert_statements(ConnectorKind::Mysql, "t", &schema, &batches).unwrap();
        assert_eq!(s.len(), 3);
        assert_eq!(s[2].matches("), (").count() + 1, 2001);
        assert!(insert_statements(ConnectorKind::Mysql, "t", &schema, &[]).unwrap().is_empty());
        // One statement at a time, capped.
        let b = InsertSql::new(ConnectorKind::Postgres, "t", &schema, batch_limits(ConnectorKind::Postgres).default_size());
        let (sql, n) = b.next(&batches[0], 11_999, Some(10)).unwrap().unwrap();
        assert_eq!(n, 2);
        assert!(sql.starts_with(r#"INSERT INTO "t" ("id", "name", "ok", "at") VALUES (11999,"#), "{sql}");
        assert!(b.next(&batches[0], 12_001, None).unwrap().is_none());
    }

    #[test]
    fn batch_sizes_per_dialect() {
        use ConnectorKind as K;
        let ms = batch_limits(K::Mssql);
        assert_eq!(ms.resolve(None, None).0, BatchSize { rows: 500, bytes: 8 * MB });
        let (size, notes) = ms.resolve(Some(5000), Some(1_000_000));
        assert_eq!(size, BatchSize { rows: 1000, bytes: 64 * MB });
        assert_eq!(notes.len(), 2);
        assert!(notes[0].contains("1,000 rows"), "{notes:?}");
        assert_eq!(batch_limits(K::Oracle).resolve(None, None).0.rows, 100);
        let (bq, notes) = batch_limits(K::Bigquery).resolve(Some(2000), Some(4096));
        assert_eq!((bq.rows, bq.bytes), (2000, 1_000_000));
        assert!(notes[0].contains("1,024 KB"));
        assert!(batch_limits(K::Sqlite).default_bytes <= 1_000_000);
        assert_eq!(batch_limits(K::Postgres).resolve(Some(0), Some(0)).0, batch_limits(K::Postgres).default_size(), "0 = default");

        // Rows and bytes of whole statements stay within the size (Oracle's INTO per row included).
        let (schema, batches) = rows(300);
        for (k, size) in [
            (K::Postgres, BatchSize { rows: 100, bytes: MB }),
            (K::Postgres, BatchSize { rows: 10_000, bytes: 4 * KB }),
            (K::Oracle, BatchSize { rows: 1000, bytes: 4 * KB }),
        ] {
            let b = InsertSql::new(k, "t", &schema, size);
            let mut at = 0;
            let mut n = 0;
            while let Some((sql, rows)) = b.next(&batches[0], at, None).unwrap() {
                assert!(rows <= size.rows && sql.len() <= size.bytes, "{k:?} {size:?}: {rows} rows, {} bytes", sql.len());
                at += rows;
                n += 1;
            }
            assert_eq!(at, 300);
            if size.rows == 100 {
                assert_eq!(n, 3);
            }
        }
    }

    #[test]
    fn staging_update_and_merge_per_dialect() {
        use ConnectorKind as K;
        let cols: Vec<String> = ["id", "name", "qty"].map(String::from).to_vec();
        let keys = vec!["id".to_string()];
        assert_eq!(stage_table_name(K::Postgres, "app.t", "x1"), "databrain_stage_x1");
        assert_eq!(stage_table_name(K::Mssql, "dbo.t", "x1"), "#databrain_stage_x1");
        assert_eq!(stage_table_name(K::Databricks, "main.app.t", "x1"), "main.app.databrain_stage_x1");
        assert_eq!(create_stage_sql(K::Postgres, "s", "app.t", &cols), r#"CREATE TEMP TABLE "s" AS SELECT "id", "name", "qty" FROM "app"."t" WHERE 1 = 0"#);
        assert!(create_stage_sql(K::Mssql, "#s", "t", &cols).starts_with("SELECT [id], [name], [qty] INTO [#s] FROM [t] WHERE 1 = 0 UNION ALL"));
        assert_eq!(drop_stage_sql(K::Mysql, "s"), "DROP TEMPORARY TABLE IF EXISTS `s`");

        assert_eq!(
            update_from_stage_sql(K::Postgres, "app.t", "s", &cols, Keys::nullable(&keys)),
            vec![r#"UPDATE "app"."t" AS tg SET "name" = src."name", "qty" = src."qty" FROM "s" AS src WHERE tg."id" IS NOT DISTINCT FROM src."id""#]
        );
        assert_eq!(
            update_from_stage_sql(K::Mysql, "t", "s", &cols, Keys::nullable(&keys)),
            vec!["UPDATE `t` AS tg JOIN `s` AS src ON tg.`id` <=> src.`id` SET tg.`name` = src.`name`, tg.`qty` = src.`qty`"]
        );
        assert!(update_from_stage_sql(K::Postgres, "t", "s", &keys, Keys::nullable(&keys)).is_empty(), "only keys: nothing to update");

        let pg = merge_from_stage_sql(K::Postgres, "t", "s", &cols, Keys::nullable(&keys));
        assert_eq!(pg.len(), 2);
        assert_eq!(
            pg[1],
            r#"INSERT INTO "t" ("id", "name", "qty") SELECT src."id", src."name", src."qty" FROM "s" AS src WHERE NOT EXISTS (SELECT 1 FROM "t" AS tg WHERE tg."id" IS NOT DISTINCT FROM src."id")"#
        );
        assert_eq!(merge_from_stage_sql(K::Sqlite, "t", "s", &keys, Keys::nullable(&keys)).len(), 1, "only keys: insert the missing ones");

        let ms = merge_from_stage_sql(K::Mssql, "dbo.t", "#s", &cols, Keys::nullable(&keys));
        assert_eq!(
            ms,
            vec![
                "MERGE INTO [dbo].[t] AS tg USING [#s] AS src ON (tg.[id] = src.[id] OR (tg.[id] IS NULL AND src.[id] IS NULL)) WHEN MATCHED THEN UPDATE SET tg.[name] = src.[name], tg.[qty] = src.[qty] WHEN NOT MATCHED THEN INSERT ([id], [name], [qty]) VALUES (src.[id], src.[name], src.[qty]);"
            ]
        );
        let ora = &update_from_stage_sql(K::Oracle, "APP.T", "APP.S", &cols, Keys::nullable(&keys))[0];
        assert!(
            ora.starts_with(r#"MERGE INTO "APP"."T" tg USING "APP"."S" src ON ((tg."id" = src."id" OR (tg."id" IS NULL AND src."id" IS NULL))) WHEN MATCHED"#),
            "{ora}"
        );
        assert!(!ora.contains("NOT MATCHED"));
        let bq = &merge_from_stage_sql(K::Bigquery, "ds.t", "ds.s", &cols, Keys::nullable(&keys))[0];
        assert!(
            bq.starts_with(
                "MERGE `ds`.`t` AS tg USING `ds`.`s` AS src ON tg.`id` IS NOT DISTINCT FROM src.`id` WHEN MATCHED THEN UPDATE SET `name` = src.`name`"
            ),
            "{bq}"
        );
    }
}
