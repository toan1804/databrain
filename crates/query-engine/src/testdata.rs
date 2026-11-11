//! Test data for a table: rows made up from its column types, ready for the
//! load path ("Load into…", or straight into the table).
//!
//! - Each column's database type (`varchar(40)`, `NUMBER(10,2)`,
//!   `timestamptz`, ClickHouse `Nullable(Int32)`…) picks a generator; the
//!   column name refines text (`email`, `city`, `first_name`…). Lengths,
//!   precision and scale are kept.
//! - Columns the database fills itself (identity, auto-increment, sequence
//!   and UUID defaults, computed columns) are left out unless asked for.
//!   Types DataBrain can't make up (arrays, geometry, user types) are left
//!   out and reported.
//! - The first primary key column counts up from the table's current
//!   maximum (numbers, dates, date-times, times) or row count (text,
//!   bytes, IP addresses), so the rows don't collide with what is there.
//!   When the column can't hold that many new keys, nothing is made.
//! - Foreign key columns take values read from the parent table (up to
//!   1,000 distinct keys), so the rows satisfy the constraint. The user is
//!   told when a table has foreign keys, and when a parent has no rows.
//! - A seed makes the rows repeatable.

use std::collections::HashSet;

use databrain_connector_core::arrow::array::RecordBatch;
use databrain_connector_core::traits::RowLimit;
use databrain_connector_core::value::{MICROS_PER_DAY, days_from_civil, parse_date, parse_datetime, parse_time};
use databrain_connector_core::{
    BatchBuilder, ColType, Column, ColumnInfo, ConnectorKind, ForeignKey, ObjectKind, Value, query_text, quote_ident, quote_object,
};
use databrain_export::load as sqlgen;
use serde::{Deserialize, Serialize};

use crate::{EngineError, LoadRows, QueryEngine, Result};

/// Most rows one request makes.
pub const MAX_TEST_ROWS: u32 = 100_000;
/// Largest seed: the UI shows and sends seeds as JavaScript numbers, exact
/// up to 2^53 - 1 (`Number.MAX_SAFE_INTEGER`).
pub const MAX_SEED: u64 = (1 << 53) - 1;
/// Distinct parent keys read per foreign key.
const PARENT_KEYS: usize = 1000;
const BATCH: usize = 8192;

/// One column of the table and what goes into it.
#[derive(Debug, Clone, Serialize, PartialEq, ts_rs::TS)]
#[ts(export)]
pub struct TestDataColumn {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub primary_key: bool,
    /// DataBrain can make up values of this type.
    pub supported: bool,
    /// The database fills it (identity, auto-increment, sequence or UUID
    /// default, computed column): left out unless asked for.
    pub database_fills: bool,
    /// What goes in, in words ("emails", "numbers counting up").
    pub generator: String,
    /// The parent table its values come from (`schema.table.column`).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub parent: Option<String>,
    /// Why it is left out, or what to know.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, ts_rs::TS)]
#[ts(export)]
pub struct TestDataForeignKey {
    pub columns: Vec<String>,
    /// The parent table, quoted as the database needs it.
    pub parent: String,
    pub ref_columns: Vec<String>,
}

/// What generating rows for a table would do, before any row is made.
#[derive(Debug, Clone, Serialize, PartialEq, ts_rs::TS)]
#[ts(export)]
pub struct TestDataPlan {
    /// The table, quoted as the database needs it.
    pub table: String,
    pub columns: Vec<TestDataColumn>,
    pub foreign_keys: Vec<TestDataForeignKey>,
    pub warnings: Vec<String>,
}

fn yes() -> bool {
    true
}

/// How many rows, and which columns.
#[derive(Debug, Clone, Deserialize, ts_rs::TS)]
#[ts(export)]
#[ts(optional_fields = nullable)]
pub struct TestDataSpec {
    pub rows: u32,
    /// Share of NULLs in nullable columns (0–100). Keys are never NULL.
    #[serde(default)]
    pub null_percent: u8,
    /// Same seed, same rows (0 to 2^53 - 1). `None`: a new seed each time.
    #[serde(default)]
    #[ts(as = "Option<f64>")]
    pub seed: Option<u64>,
    /// Columns to leave to the database (its default or NULL).
    #[serde(default)]
    pub skip: Vec<String>,
    /// Columns the database fills that should be generated anyway.
    #[serde(default)]
    pub fill: Vec<String>,
    /// Foreign key columns take keys read from the parent tables (else
    /// made-up values that may break the constraint).
    #[serde(default = "yes")]
    pub parent_keys: bool,
}

/// Generated rows plus what the user should know about them.
pub struct TestData {
    pub rows: LoadRows,
    pub seed: u64,
    pub notes: Vec<String>,
    /// The table, quoted.
    pub table: String,
}

impl QueryEngine {
    /// Columns of a table and what each would get.
    pub async fn test_data_plan(&self, connection_id: &str, schema: &str, name: &str) -> Result<TestDataPlan> {
        let kind = self.test_data_kind(connection_id)?;
        let detail = self.describe(connection_id, schema, name).await?;
        if !matches!(detail.object.kind, ObjectKind::Table | ObjectKind::Other) {
            return Err(EngineError::new("invalid", format!("{name} is a {}: test data goes into tables", detail.object.kind.as_str().replace('_', " "))));
        }
        let ddl = self.object_ddl(connection_id, schema, name, ObjectKind::Table).await.ok().flatten();
        Ok(plan_for(kind, schema, name, &detail.columns, &detail.foreign_keys, ddl.as_deref()).0)
    }

    /// Make up `spec.rows` rows for a table. Reads the table's largest key
    /// and, for foreign keys, existing keys of the parent tables; writes
    /// nothing.
    pub async fn generate_test_data(&self, connection_id: &str, schema: &str, name: &str, spec: &TestDataSpec) -> Result<TestData> {
        if spec.rows == 0 || spec.rows > MAX_TEST_ROWS {
            return Err(EngineError::new("invalid", format!("Choose between 1 and {MAX_TEST_ROWS} rows")));
        }
        if spec.seed.is_some_and(|s| s > MAX_SEED) {
            return Err(EngineError::new("invalid", format!("Choose a seed between 0 and {MAX_SEED}")));
        }
        let kind = self.test_data_kind(connection_id)?;
        let detail = self.describe(connection_id, schema, name).await?;
        let ddl = self.object_ddl(connection_id, schema, name, ObjectKind::Table).await.ok().flatten();
        let (plan, gens) = plan_for(kind, schema, name, &detail.columns, &detail.foreign_keys, ddl.as_deref());
        let skip: HashSet<String> = spec.skip.iter().map(|s| s.to_lowercase()).collect();
        let fill: HashSet<String> = spec.fill.iter().map(|s| s.to_lowercase()).collect();
        let chosen: Vec<usize> = plan
            .columns
            .iter()
            .enumerate()
            .filter(|(_, c)| c.supported && !skip.contains(&c.name.to_lowercase()) && (!c.database_fills || fill.contains(&c.name.to_lowercase())))
            .map(|(i, _)| i)
            .collect();
        if chosen.is_empty() {
            return Err(EngineError::new("invalid", "No column to fill: every column is left to the database or has a type DataBrain can't make up"));
        }
        let mut notes = plan.warnings.clone();
        let seed = spec.seed.unwrap_or_else(new_seed);
        let mut rng = Rng(seed);

        // Sources per chosen column: random, a counting key, or parent keys.
        let mut sources: Vec<Source> = chosen.iter().map(|&i| Source::Random(gens[i].clone().expect("supported"))).collect();
        let pos = |col: &str| chosen.iter().position(|&i| plan.columns[i].name.eq_ignore_ascii_case(col));

        // The first primary key column counts up past the current maximum
        // (or row count): unique within the rows and against the table.
        if let Some(k) = chosen.iter().position(|&i| plan.columns[i].primary_key) {
            let col = &plan.columns[chosen[k]];
            let maker = gens[chosen[k]].clone().expect("supported");
            let q = quote_ident(kind, &col.name);
            let read = match key_read(&maker) {
                KeyRead::Max => Some((format!("SELECT MAX({q}) FROM {}", plan.table), "largest")),
                KeyRead::Count => Some((format!("SELECT COUNT(*) FROM {}", plan.table), "row count of")),
                KeyRead::Nothing => None,
            };
            let found = match read {
                Some((sql, what)) => match self.meta_rows(connection_id, &sql).await {
                    Ok(r) => r.first().and_then(|row| row.first().cloned().flatten()),
                    Err(e) => {
                        notes.push(format!("Couldn't read the {what} {} ({}): its new keys may collide with existing rows.", col.name, e.message));
                        None
                    }
                },
                None => None,
            };
            let start = key_start(&maker, found.as_deref(), spec.rows, &col.name, &col.data_type).map_err(|m| EngineError::new("invalid", m))?;
            let tag: String = (0..4).map(|_| (b'a' + rng.below(26) as u8) as char).collect();
            sources[k] = Source::Key { maker, start, tag };
        }

        // Foreign keys: values of the parent's existing keys.
        let mut parents: Vec<Vec<Vec<Option<String>>>> = Vec::new();
        for fk in &plan.foreign_keys {
            let positions: Vec<(usize, usize)> = fk.columns.iter().enumerate().filter_map(|(j, c)| pos(c).map(|p| (p, j))).collect();
            if positions.is_empty() {
                continue;
            }
            let label = fk.columns.join(", ");
            if !spec.parent_keys {
                notes.push(format!("{label}: made-up values (reading {} was turned off); they may break the foreign key.", fk.parent));
                continue;
            }
            let cols: Vec<&str> = positions.iter().map(|&(_, j)| fk.ref_columns[j].as_str()).collect();
            let sql = parent_keys_sql(kind, &fk.parent, &cols, PARENT_KEYS);
            match self.meta_rows(connection_id, &sql).await {
                Ok(keys) if keys.is_empty() => {
                    let all_nullable = positions.iter().all(|&(p, _)| plan.columns[chosen[p]].nullable);
                    if all_nullable {
                        notes.push(format!("{} has no rows: {label} is left NULL.", fk.parent));
                        for &(p, _) in &positions {
                            sources[p] = Source::Null;
                        }
                    } else {
                        notes.push(format!(
                            "{} has no rows: {label} gets made-up values, which break the foreign key. Add rows to {} first.",
                            fk.parent, fk.parent
                        ));
                    }
                }
                Ok(keys) => {
                    let n = keys.len();
                    let fk_index = parents.len();
                    parents.push(keys);
                    let unique = positions.iter().any(|&(p, _)| matches!(sources[p], Source::Key { .. }));
                    if unique && n < spec.rows as usize {
                        notes.push(format!("{label} is also the key, and {} has only {n} keys: rows past {n} repeat a key.", fk.parent));
                    }
                    for (slot, &(p, _)) in positions.iter().enumerate() {
                        sources[p] = Source::Parent { fk: fk_index, slot, unique };
                    }
                    notes.push(format!("{label}: {} existing key{} of {}.", n, if n == 1 { "" } else { "s" }, fk.parent));
                }
                Err(e) => notes
                    .push(format!("Couldn't read the keys of {} ({}): {label} gets made-up values, which may break the foreign key.", fk.parent, e.message)),
            }
        }

        let columns: Vec<Column> = chosen
            .iter()
            .zip(&sources)
            .map(|(&i, s)| {
                let c = &plan.columns[i];
                let ty = match s {
                    Source::Random(g) | Source::Key { maker: g, .. } => g.col_type(),
                    _ => gens[i].as_ref().map(Gen::col_type).unwrap_or(ColType::Utf8),
                };
                Column::new(&c.name, ty, &c.data_type)
            })
            .collect();
        let nullable: Vec<bool> = chosen.iter().map(|&i| plan.columns[i].nullable && !plan.columns[i].primary_key).collect();
        let batches = generate(&columns, &sources, &nullable, &parents, spec.rows as usize, spec.null_percent.min(100), &mut rng)?;
        let schema = databrain_connector_core::value::schema_for(&columns);
        Ok(TestData { rows: LoadRows { schema, batches }, seed, notes, table: plan.table })
    }

    fn test_data_kind(&self, connection_id: &str) -> Result<ConnectorKind> {
        let p = self.workspace().get_connection(connection_id)?;
        let kind = p.config.kind;
        if !sqlgen::is_load_target(kind) {
            return Err(EngineError::new(
                "unsupported",
                format!("{} tables can't be loaded by DataBrain, so it can't make test data for them", kind.traits().name),
            ));
        }
        Ok(kind)
    }

    async fn meta_rows(&self, connection_id: &str, sql: &str) -> Result<Vec<Vec<Option<String>>>> {
        let sql = sql.to_string();
        self.with_meta(connection_id, |s| {
            let sql = sql.clone();
            async move { query_text(&*s, &sql).await }
        })
        .await
    }
}

/// `SELECT DISTINCT` of a parent's key columns, non-NULL, capped.
fn parent_keys_sql(kind: ConnectorKind, parent: &str, cols: &[&str], n: usize) -> String {
    let list = cols.iter().map(|c| quote_ident(kind, c)).collect::<Vec<_>>().join(", ");
    let filter = cols.iter().map(|c| format!("{} IS NOT NULL", quote_ident(kind, c))).collect::<Vec<_>>().join(" AND ");
    match kind.traits().row_limit {
        RowLimit::Top => format!("SELECT DISTINCT TOP {n} {list} FROM {parent} WHERE {filter}"),
        RowLimit::FetchFirst => format!("SELECT DISTINCT {list} FROM {parent} WHERE {filter} FETCH FIRST {n} ROWS ONLY"),
        RowLimit::Limit => format!("SELECT DISTINCT {list} FROM {parent} WHERE {filter} LIMIT {n}"),
    }
}

/// A new seed from the clock, at most [`MAX_SEED`] so the UI shows it
/// exactly (nanoseconds since 1970 are past 2^60).
fn new_seed() -> u64 {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(1);
    // Mix the high bits in so seeds made close together still differ widely.
    let mut r = Rng(nanos as u64 ^ (nanos >> 64) as u64);
    r.next() & MAX_SEED
}

// ------------------------------------------------------------- keys

/// What the table is asked before its first key column counts up.
#[derive(Debug, PartialEq)]
enum KeyRead {
    /// `MAX(column)`: keys continue after it.
    Max,
    /// `COUNT(*)`: keys continue after the number of rows (text, bytes and
    /// addresses have no "largest" that orders like the counter).
    Count,
    Nothing,
}

fn key_read(g: &Gen) -> KeyRead {
    match g {
        Gen::Int { .. } | Gen::Decimal { .. } | Gen::Float | Gen::Date | Gen::Timestamp | Gen::TimestampTz | Gen::Time => KeyRead::Max,
        Gen::Text { .. } | Gen::Binary { .. } | Gen::Inet => KeyRead::Count,
        Gen::Bool | Gen::Choice(_) | Gen::Uuid | Gen::Json => KeyRead::Nothing,
    }
}

/// Floats are exact integers up to 2^53.
const FLOAT_EXACT: i64 = 1 << 53;
const SECONDS_PER_DAY: i64 = 86_400;

/// The integer after `text` (the largest key, any size): exact for
/// integers and decimals of any length, `None` when it isn't a number.
/// `Err` when the next one is past a 64-bit integer.
fn after_number(text: &str) -> std::result::Result<Option<i64>, ()> {
    let t = text.trim();
    let neg = t.starts_with('-');
    let digits = t.trim_start_matches(['+', '-']);
    if !digits.contains(['e', 'E']) {
        let (ip, fp) = digits.split_once('.').unwrap_or((digits, ""));
        if (!ip.is_empty() || !fp.is_empty()) && ip.bytes().all(|b| b.is_ascii_digit()) && fp.bytes().all(|b| b.is_ascii_digit()) {
            let ip = ip.trim_start_matches('0');
            if ip.len() > 30 {
                return Err(());
            }
            let whole: i128 = if ip.is_empty() { 0 } else { ip.parse().map_err(|_| ())? };
            let fraction = fp.bytes().any(|b| b != b'0');
            let floor = if neg { -whole - i128::from(fraction) } else { whole };
            return i64::try_from(floor + 1).map(Some).map_err(|_| ());
        }
    }
    match t.parse::<f64>() {
        Ok(f) if f.is_finite() && f.abs() < 9.0e18 => Ok(Some(f.floor() as i64 + 1)),
        Ok(f) if f.is_finite() => Err(()),
        _ => Ok(None),
    }
}

/// Microseconds since 1970 of a date-time as databases print it.
fn parse_ts(s: &str) -> Option<i64> {
    let s = s.trim().trim_end_matches('Z');
    let s = s.strip_suffix("+00:00").or_else(|| s.strip_suffix(" UTC")).unwrap_or(s);
    parse_datetime(s)
}

fn base36(mut n: u64) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    loop {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
        if n == 0 {
            break;
        }
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// Text key `n`: `tag-n`, or `n` in base 36 (lowercase, so case-insensitive
/// collations keep them apart) when that doesn't fit the column. The two
/// forms never meet: only the first has a `-`.
fn text_key(tag: &str, n: i64, max: Option<usize>) -> String {
    let s = format!("{tag}-{n}");
    match max {
        Some(m) if s.chars().count() > m => base36(n.max(0) as u64),
        _ => s,
    }
}

/// Bytes of a binary key: big-endian, all the same length so that
/// fixed-length columns (padded with zeros) keep them apart.
fn binary_key_len(max: Option<usize>) -> usize {
    max.unwrap_or(8).clamp(1, 8)
}

/// Where the counting key starts (see [`key_value`] for what the number
/// means per type), from what [`key_read`] read: `None` when the table is
/// empty or it couldn't be read. `Err`: the column can't hold `rows` new
/// unique keys.
fn key_start(g: &Gen, found: Option<&str>, rows: u32, col: &str, data_type: &str) -> std::result::Result<i64, String> {
    let rows = i64::from(rows.max(1));
    let full = |what: &str| format!("{col} ({data_type}) has no room for {rows} more unique keys: {what}. Choose fewer rows, or leave {col} to the database.");
    let day0 = days_from_civil(2020, 1, 1);
    let last_day = days_from_civil(9999, 12, 31);
    let after_count = || found.and_then(|c| c.trim().parse::<i64>().ok()).map_or(1, |c| c.saturating_add(1));
    let start = match g {
        Gen::Int { .. } | Gen::Decimal { .. } | Gen::Float => match found.map(after_number) {
            Some(Err(())) => return Err(full(&format!("its largest value is {}", found.unwrap_or_default().trim()))),
            Some(Ok(Some(n))) => n,
            _ => 1,
        },
        Gen::Date => {
            found.and_then(|s| parse_date(s.trim()).map(i64::from).or_else(|| parse_ts(s).map(|m| m.div_euclid(MICROS_PER_DAY)))).map_or(day0, |d| d + 1)
        }
        Gen::Timestamp | Gen::TimestampTz => found.and_then(parse_ts).map_or(day0 * SECONDS_PER_DAY, |m| m.div_euclid(1_000_000) + 1),
        Gen::Time => found.and_then(|s| parse_time(s.trim())).map_or(0, |m| m.div_euclid(1_000_000) + 1),
        Gen::Text { .. } | Gen::Binary { .. } | Gen::Inet => after_count(),
        Gen::Bool | Gen::Choice(_) => 0,
        Gen::Uuid | Gen::Json => 1,
    };
    let last = start.checked_add(rows - 1).ok_or_else(|| full("the next key is past the largest 64-bit integer"))?;
    match g {
        Gen::Float if last > FLOAT_EXACT => Err(full("floats stop being exact past 2^53")),
        Gen::Date if last > last_day => Err(full("the dates would pass 9999-12-31")),
        Gen::Timestamp | Gen::TimestampTz if last >= (last_day + 1) * SECONDS_PER_DAY => Err(full("the date-times would pass 9999-12-31")),
        Gen::Time if last >= SECONDS_PER_DAY => Err(full("a day has 86,400 seconds")),
        Gen::Text { max: Some(m), .. } if text_key("abcd", last, Some(*m)).chars().count() > *m => {
            Err(full(&format!("{m} characters hold {} keys", 36u128.checked_pow(*m as u32).map_or("many".into(), |n| (n - 1).to_string()))))
        }
        Gen::Binary { max } if binary_key_len(*max) < 8 && last >= 1i64 << (8 * binary_key_len(*max)) => {
            Err(full(&format!("{} bytes hold {} keys", binary_key_len(*max), (1i64 << (8 * binary_key_len(*max))) - 1)))
        }
        Gen::Inet if last >= 1 << 24 => Err(full("the addresses 10.0.0.0/8 hold 16,777,216")),
        Gen::Bool if rows > 2 => Err(full("it has two values")),
        Gen::Choice(v) if rows > v.len() as i64 => Err(full(&format!("it has {} values", v.len()))),
        _ => Ok(start),
    }
}

// ------------------------------------------------------------------ plan

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flavor {
    Email,
    FirstName,
    LastName,
    FullName,
    City,
    Country,
    Phone,
    Url,
    Status,
    Code,
    Sentence,
    Word,
}

#[derive(Debug, Clone, PartialEq)]
enum Gen {
    Bool,
    Int {
        min: i64,
        max: i64,
    },
    Float,
    /// Exact decimal as text: digits before and after the point.
    Decimal {
        int_digits: u32,
        scale: u32,
    },
    Text {
        max: Option<usize>,
        flavor: Flavor,
    },
    Uuid,
    Json,
    Choice(Vec<String>),
    Inet,
    Date,
    Timestamp,
    TimestampTz,
    Time,
    Binary {
        max: Option<usize>,
    },
}

impl Gen {
    fn col_type(&self) -> ColType {
        match self {
            Gen::Bool => ColType::Bool,
            Gen::Int { .. } => ColType::Int64,
            Gen::Float => ColType::Float64,
            Gen::Date => ColType::Date,
            Gen::Timestamp => ColType::Timestamp,
            Gen::TimestampTz => ColType::TimestampTz,
            Gen::Time => ColType::Time,
            Gen::Binary { .. } => ColType::Binary,
            Gen::Decimal { .. } | Gen::Text { .. } | Gen::Uuid | Gen::Json | Gen::Choice(_) | Gen::Inet => ColType::Utf8,
        }
    }

    fn describe(&self) -> String {
        match self {
            Gen::Bool => "true or false".into(),
            Gen::Int { min, max } => format!("whole numbers {min}–{max}"),
            Gen::Float => "decimal numbers".into(),
            Gen::Decimal { int_digits, scale } => format!("numbers with {int_digits} digits and {scale} decimals"),
            Gen::Text { max, flavor } => {
                let what = match flavor {
                    Flavor::Email => "emails",
                    Flavor::FirstName => "first names",
                    Flavor::LastName => "last names",
                    Flavor::FullName => "names",
                    Flavor::City => "cities",
                    Flavor::Country => "countries",
                    Flavor::Phone => "phone numbers",
                    Flavor::Url => "web addresses",
                    Flavor::Status => "statuses",
                    Flavor::Code => "codes",
                    Flavor::Sentence => "sentences",
                    Flavor::Word => "words",
                };
                match max {
                    Some(m) => format!("{what} (up to {m} characters)"),
                    None => what.to_string(),
                }
            }
            Gen::Uuid => "UUIDs".into(),
            Gen::Json => "small JSON objects".into(),
            Gen::Choice(v) => format!("one of {}", v.join(", ")),
            Gen::Inet => "IP addresses".into(),
            Gen::Date => "dates 2020–2025".into(),
            Gen::Timestamp | Gen::TimestampTz => "date-times 2020–2025".into(),
            Gen::Time => "times of day".into(),
            Gen::Binary { .. } => "random bytes".into(),
        }
    }

    /// What a counting primary key gets.
    fn key_describe(&self) -> String {
        match self {
            Gen::Int { .. } | Gen::Decimal { .. } | Gen::Float => "numbers counting up from the largest".into(),
            Gen::Text { .. } => "unique codes, counting on from the row count".into(),
            Gen::Date => "dates counting up a day at a time from the latest".into(),
            Gen::Timestamp | Gen::TimestampTz => "date-times counting up a second at a time from the latest".into(),
            Gen::Time => "times counting up a second at a time from the latest".into(),
            Gen::Binary { .. } => "unique bytes, counting on from the row count".into(),
            Gen::Inet => "unique IP addresses, counting on from the row count".into(),
            Gen::Bool | Gen::Choice(_) => format!("{}, each once", self.describe()),
            Gen::Uuid | Gen::Json => self.describe(),
        }
    }
}

/// Columns, their generators (`None`: not supported) and warnings.
fn plan_for(kind: ConnectorKind, schema: &str, name: &str, cols: &[ColumnInfo], fks: &[ForeignKey], ddl: Option<&str>) -> (TestDataPlan, Vec<Option<Gen>>) {
    let table = quote_object(kind, schema, name);
    let mut warnings = Vec::new();
    let mut gens = Vec::new();
    let mut columns = Vec::new();
    let parent_of = |c: &str| {
        fks.iter().find_map(|fk| {
            let j = fk.columns.iter().position(|x| x.eq_ignore_ascii_case(c))?;
            Some(format!("{}.{}", parent_name(kind, schema, fk), fk.ref_columns.get(j)?))
        })
    };
    for c in cols {
        let maker = parse_type(kind, &c.data_type, &c.name);
        let fills = database_fills(kind, c, ddl);
        let parent = parent_of(&c.name);
        let note = match (&maker, fills) {
            (None, _) => Some(if !c.nullable && c.default.is_none() && !fills {
                format!("DataBrain can't make up {} values; the column is NOT NULL without a default, so inserts will fail", c.data_type)
            } else {
                format!("DataBrain can't make up {} values: left to the database", c.data_type)
            }),
            (Some(_), true) => Some("The database fills it (identity, sequence or computed): left out unless you turn it on".into()),
            _ => None,
        };
        if maker.is_none() && !c.nullable && c.default.is_none() && !fills {
            warnings.push(format!("{} ({}) is NOT NULL and DataBrain can't make up its values: inserts will fail.", c.name, c.data_type));
        }
        columns.push(TestDataColumn {
            name: c.name.clone(),
            data_type: c.data_type.clone(),
            nullable: c.nullable,
            primary_key: c.is_primary_key,
            supported: maker.is_some(),
            database_fills: fills,
            generator: match (&parent, &maker) {
                (Some(p), _) => format!("existing keys of {p}"),
                (None, Some(g)) if c.is_primary_key => g.key_describe(),
                (None, Some(g)) => g.describe(),
                (None, None) => "left to the database".into(),
            },
            parent,
            note,
        });
        gens.push(maker);
    }
    let foreign_keys: Vec<TestDataForeignKey> = fks
        .iter()
        .map(|fk| TestDataForeignKey { columns: fk.columns.clone(), parent: parent_name(kind, schema, fk), ref_columns: fk.ref_columns.clone() })
        .collect();
    if !foreign_keys.is_empty() {
        warnings.insert(
            0,
            format!(
                "This table has {} foreign key{}: their columns take existing keys read from {}.",
                foreign_keys.len(),
                if foreign_keys.len() == 1 { "" } else { "s" },
                foreign_keys.iter().map(|f| f.parent.as_str()).collect::<Vec<_>>().join(", ")
            ),
        );
    }
    (TestDataPlan { table, columns, foreign_keys, warnings }, gens)
}

fn parent_name(kind: ConnectorKind, schema: &str, fk: &ForeignKey) -> String {
    let s = if fk.ref_schema.trim().is_empty() { schema } else { &fk.ref_schema };
    quote_object(kind, s, &fk.ref_table)
}

/// Defaults and DDL words that mean the database makes the value.
fn database_fills(kind: ConnectorKind, c: &ColumnInfo, ddl: Option<&str>) -> bool {
    const DEFAULTS: &[&str] =
        &["nextval", "identity", "autoincrement", "auto_increment", "generated", "newid(", "newsequentialid(", "gen_random_uuid", "uuid_generate", "uuid()"];
    const TYPES: &[&str] = &["serial", "identity", "autoincrement"];
    const DDL: &[&str] = &["identity", "auto_increment", "autoincrement", "generated always", "materialized", " alias ", " as ("];
    let d = c.default.as_deref().unwrap_or("").to_ascii_lowercase();
    if DEFAULTS.iter().any(|w| d.contains(w)) {
        return true;
    }
    let t = c.data_type.to_ascii_lowercase();
    if TYPES.iter().any(|w| t.contains(w)) {
        return true;
    }
    // SQL Server `timestamp` is `rowversion`: the database writes it, and an
    // INSERT naming it fails.
    if kind == ConnectorKind::Mssql && matches!(t.trim(), "timestamp" | "rowversion") {
        return true;
    }
    let Some(ddl) = ddl else { return false };
    let name = c.name.to_ascii_lowercase();
    ddl.lines().any(|line| {
        let l = line.trim().to_ascii_lowercase();
        let rest = ["\"", "`", "["]
            .iter()
            .find_map(|q| {
                let close = if *q == "[" { "]" } else { q };
                l.strip_prefix(q).and_then(|r| r.strip_prefix(name.as_str())).and_then(|r| r.strip_prefix(close))
            })
            .or_else(|| l.strip_prefix(name.as_str()).filter(|r| r.starts_with(char::is_whitespace)));
        rest.is_some_and(|r| {
            let r = format!(" {r} ");
            DDL.iter().any(|w| r.contains(w))
        })
    })
}

/// The type name without its `(…)` parts, and the numbers inside the first one.
fn split_type(t: &str) -> (String, Vec<Option<u32>>) {
    let mut plain = String::new();
    let mut args = String::new();
    let mut depth = 0;
    let mut first_done = false;
    for ch in t.chars() {
        match ch {
            '(' => {
                depth += 1;
                if depth == 1 {
                    plain.push(' ');
                }
            }
            ')' => {
                depth -= 1;
                if depth == 0 {
                    first_done = true;
                }
            }
            _ if depth == 0 => plain.push(ch),
            _ if !first_done && depth == 1 => args.push(ch),
            _ => {}
        }
    }
    let nums = if args.trim().is_empty() {
        vec![]
    } else {
        args.split(',').map(|a| a.trim().chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok()).collect()
    };
    (plain.split_whitespace().collect::<Vec<_>>().join(" "), nums)
}

fn quoted_values(t: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur: Option<String> = None;
    let mut chars = t.chars().peekable();
    while let Some(c) = chars.next() {
        match (&mut cur, c) {
            (None, '\'') => cur = Some(String::new()),
            (Some(s), '\'') if chars.peek() == Some(&'\'') => {
                chars.next();
                s.push('\'');
            }
            (Some(_), '\'') => out.push(cur.take().unwrap_or_default()),
            (Some(s), c) => s.push(c),
            _ => {}
        }
    }
    out
}

fn flavor_of(column: &str) -> Flavor {
    let n = column.to_ascii_lowercase();
    let has = |w: &str| n.contains(w);
    if has("email") || has("e_mail") {
        Flavor::Email
    } else if has("first_name") || has("firstname") || has("given_name") {
        Flavor::FirstName
    } else if has("last_name") || has("lastname") || has("surname") || has("family_name") {
        Flavor::LastName
    } else if n == "name" || n.ends_with("_name") || n.ends_with("name") && n.len() > 4 && !has("file") {
        Flavor::FullName
    } else if has("city") || has("town") {
        Flavor::City
    } else if has("country") || has("nation") {
        Flavor::Country
    } else if has("phone") || has("mobile") || has("fax") {
        Flavor::Phone
    } else if has("url") || has("website") || has("link") || has("homepage") {
        Flavor::Url
    } else if has("status") || has("state") {
        Flavor::Status
    } else if has("code") || has("sku") || has("ref") {
        Flavor::Code
    } else if has("description") || has("comment") || has("note") || has("body") || has("message") || has("text") || has("summary") {
        Flavor::Sentence
    } else {
        Flavor::Word
    }
}

/// The generator for a database type, `None` when DataBrain can't make it up.
fn parse_type(kind: ConnectorKind, raw: &str, column: &str) -> Option<Gen> {
    let mut t = raw.trim().to_ascii_lowercase();
    // ClickHouse wrappers.
    loop {
        let before = t.clone();
        for w in ["nullable(", "lowcardinality("] {
            if t.starts_with(w) && t.ends_with(')') {
                t = t[w.len()..t.len() - 1].trim().to_string();
            }
        }
        if t == before {
            break;
        }
    }
    if t.ends_with("[]") || t.starts_with('_') || ["array", "map", "struct", "tuple", "row", "list", "union", "nested"].iter().any(|w| t.starts_with(w)) {
        return None;
    }
    if t.starts_with("enum") || t.starts_with("set(") {
        // The values as written (case kept).
        let v = quoted_values(&raw[raw.find('(')?..]);
        return (!v.is_empty()).then_some(Gen::Choice(v));
    }
    let (plain, args) = split_type(&t);
    let plain = plain.replace(" unsigned", "").replace(" zerofill", "");
    let a0 = args.first().copied().flatten();
    let a1 = args.get(1).copied().flatten();
    let text = |max: Option<u32>| Gen::Text { max: max.map(|m| m as usize), flavor: flavor_of(column) };
    let int = |min: i64, max: i64| Gen::Int { min, max };
    let ch = kind == ConnectorKind::Clickhouse;
    let first = plain.split(' ').next().unwrap_or("");
    let maker = match plain.as_str() {
        // SQL Server rowversion: the database writes it, nothing else can.
        "timestamp" | "rowversion" if kind == ConnectorKind::Mssql => return None,
        "bool" | "boolean" => Gen::Bool,
        // Postgres and DuckDB BIT is a bit string: TRUE doesn't go in, '1' does.
        "bit" if a0.is_none_or(|n| n == 1) && matches!(kind, ConnectorKind::Postgres | ConnectorKind::Duckdb) => Gen::Choice(vec!["0".into(), "1".into()]),
        "bit" if a0.is_none_or(|n| n == 1) => Gen::Bool,
        "int8" if ch => int(0, 100),
        "int16" if ch => int(1, 30_000),
        "int32" if ch => int(1, 1_000_000),
        "int64" | "int128" | "int256" if ch => int(1, 1_000_000_000),
        "uint8" => int(0, 200),
        "uint16" | "usmallint" => int(1, 60_000),
        "uint32" | "uinteger" => int(1, 1_000_000),
        "uint64" | "uint128" | "uint256" | "ubigint" | "uhugeint" => int(1, 1_000_000_000),
        "tinyint" | "int1" | "utinyint" => int(0, 100),
        "smallint" | "int2" | "smallserial" | "serial2" | "short" => int(1, 30_000),
        "mediumint" | "int" | "integer" | "int4" | "serial" | "serial4" | "int32" => int(1, 1_000_000),
        "bigint" | "int8" | "int64" | "bigserial" | "serial8" | "hugeint" | "int128" => int(1, 1_000_000_000),
        "year" => int(1990, 2030),
        "float" | "float4" | "float8" | "real" | "double" | "double precision" | "float32" | "float64" | "binary_float" | "binary_double" | "smallfloat" => {
            Gen::Float
        }
        "numeric" | "decimal" | "dec" | "number" | "bignumeric" | "bigdecimal" | "newdecimal" | "decimal32" | "decimal64" | "decimal128" | "decimal256" => {
            let (p, s) = match (a0, a1) {
                (Some(p), s) => (p, s.unwrap_or(0)),
                // Oracle NUMBER, Postgres numeric: any size.
                (None, _) if matches!(plain.as_str(), "number" | "numeric") => (8, 2),
                (None, _) if plain == "bignumeric" || plain == "bigdecimal" => (12, 2),
                (None, _) => (10, 0),
            };
            let int_digits = p.saturating_sub(s).min(6);
            if s == 0 { int(1, 10i64.pow(int_digits.clamp(1, 9)) - 1) } else { Gen::Decimal { int_digits, scale: s.min(6) } }
        }
        "money" | "smallmoney" => Gen::Decimal { int_digits: 4, scale: 2 },
        "date" | "date32" => Gen::Date,
        "time" | "timetz" | "time with time zone" | "time without time zone" => Gen::Time,
        "timestamptz" | "timestamp with time zone" | "timestamp with local time zone" | "timestamp_tz" | "timestamp_ltz" | "datetimeoffset" => Gen::TimestampTz,
        "timestamp" if matches!(kind, ConnectorKind::Bigquery | ConnectorKind::Databricks) => Gen::TimestampTz,
        "timestamp" | "timestamp without time zone" | "timestamp_ntz" | "datetime" | "datetime2" | "smalldatetime" | "datetime64" => Gen::Timestamp,
        "uuid" | "uniqueidentifier" => Gen::Uuid,
        // Text literals don't go into Snowflake VARIANT or BigQuery JSON columns.
        "json" | "jsonb" | "variant" | "object" | "super" if !matches!(kind, ConnectorKind::Snowflake | ConnectorKind::Bigquery) => Gen::Json,
        "inet" | "cidr" => Gen::Inet,
        "char"
        | "character"
        | "nchar"
        | "varchar"
        | "character varying"
        | "nvarchar"
        | "varchar2"
        | "nvarchar2"
        | "bpchar"
        | "fixedstring"
        | "string"
        | "national character varying"
        | "national char" => text(a0),
        "text" | "tinytext" | "mediumtext" | "longtext" | "ntext" | "clob" | "nclob" | "citext" | "name" | "sysname" | "long" => text(None),
        "bytea" | "blob" | "tinyblob" | "mediumblob" | "longblob" | "binary" | "varbinary" | "bytes" | "raw" | "image" | "long raw" => {
            Gen::Binary { max: a0.map(|m| m as usize) }
        }
        _ if first == "varchar" || first == "char" || first == "nvarchar" => text(a0),
        // SQLite type affinity: any name works.
        _ if kind == ConnectorKind::Sqlite => {
            if plain.contains("int") {
                int(1, 1_000_000)
            } else if plain.contains("char") || plain.contains("clob") || plain.contains("text") || plain.is_empty() {
                text(a0)
            } else if plain.contains("real") || plain.contains("floa") || plain.contains("doub") {
                Gen::Float
            } else if plain.contains("blob") {
                Gen::Binary { max: None }
            } else {
                int(1, 1_000_000)
            }
        }
        _ => return None,
    };
    Some(maker)
}

// -------------------------------------------------------------- generate

enum Source {
    Random(Gen),
    /// Unique per row: counts up from `start` (see [`key_value`]).
    Key {
        maker: Gen,
        start: i64,
        tag: String,
    },
    /// Column `slot` of a parent key tuple of foreign key `fk`.
    Parent {
        fk: usize,
        slot: usize,
        unique: bool,
    },
    Null,
}

struct Rng(u64);

impl Rng {
    /// splitmix64: small, fast, the same rows for the same seed everywhere.
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }
    fn range(&mut self, lo: i64, hi: i64) -> i64 {
        let span = (hi as i128 - lo as i128 + 1).max(1) as u128;
        (lo as i128 + (self.next() as u128 % span) as i128) as i64
    }
    fn pick<'a>(&mut self, xs: &'a [&'a str]) -> &'a str {
        xs[self.below(xs.len() as u64) as usize]
    }
}

const FIRST: &[&str] = &[
    "Ana", "Ben", "Chloe", "David", "Elena", "Farid", "Grace", "Hugo", "Iris", "Jonas", "Kenji", "Lena", "Mateo", "Nora", "Omar", "Priya", "Quinn", "Rosa",
    "Sven", "Tara",
];
const LAST: &[&str] =
    &["Silva", "Novak", "Tanaka", "Okafor", "Müller", "Garcia", "Kim", "Rossi", "Nguyen", "Larsen", "Cohen", "Patel", "Dubois", "Kowalski", "Haddad", "Moreau"];
const CITIES: &[&str] = &["Lisbon", "Osaka", "Lagos", "Hamburg", "Montreal", "Seoul", "Lima", "Oslo", "Austin", "Hanoi", "Porto", "Nairobi", "Lyon", "Perth"];
const COUNTRIES: &[&str] = &["Portugal", "Japan", "Nigeria", "Germany", "Canada", "Korea", "Peru", "Norway", "United States", "Vietnam", "Kenya", "France"];
const STATUSES: &[&str] = &["active", "pending", "closed", "on_hold"];
const WORDS: &[&str] =
    &["alpha", "brisk", "cedar", "delta", "ember", "fjord", "glade", "harbor", "indigo", "juniper", "kestrel", "lumen", "maple", "nimbus", "orbit", "pebble"];

fn truncate(s: String, max: Option<usize>) -> String {
    match max {
        Some(m) if s.chars().count() > m => s.chars().take(m).collect(),
        _ => s,
    }
}

fn uuid(rng: &mut Rng) -> String {
    let (a, b) = (rng.next(), rng.next());
    let b = (b & 0x3FFF_FFFF_FFFF_FFFF) | 0x8000_0000_0000_0000;
    let a = (a & 0xFFFF_FFFF_FFFF_0FFF) | 0x0000_0000_0000_4000;
    format!("{:08x}-{:04x}-{:04x}-{:04x}-{:012x}", a >> 32, (a >> 16) & 0xFFFF, a & 0xFFFF, b >> 48, b & 0xFFFF_FFFF_FFFF)
}

fn text_value(flavor: Flavor, max: Option<usize>, n: u64, rng: &mut Rng) -> String {
    let s = match flavor {
        Flavor::Email => format!("{}.{}{n}@example.com", rng.pick(FIRST).to_lowercase(), rng.pick(LAST).to_lowercase().replace('ü', "u")),
        Flavor::FirstName => rng.pick(FIRST).to_string(),
        Flavor::LastName => rng.pick(LAST).to_string(),
        Flavor::FullName => format!("{} {}", rng.pick(FIRST), rng.pick(LAST)),
        Flavor::City => rng.pick(CITIES).to_string(),
        Flavor::Country => rng.pick(COUNTRIES).to_string(),
        Flavor::Phone => format!("+1-555-{:04}", rng.below(10_000)),
        Flavor::Url => format!("https://example.com/{}/{n}", rng.pick(WORDS)),
        Flavor::Status => rng.pick(STATUSES).to_string(),
        Flavor::Code => (0..6).map(|i| if i % 2 == 0 { (b'A' + rng.below(26) as u8) as char } else { (b'0' + rng.below(10) as u8) as char }).collect(),
        Flavor::Sentence => {
            let k = 3 + rng.below(6) as usize;
            let mut w: Vec<&str> = (0..k).map(|_| rng.pick(WORDS)).collect();
            let first = w[0].to_string();
            let cap = first[..1].to_uppercase() + &first[1..];
            w[0] = &cap;
            format!("{}.", w.join(" "))
        }
        Flavor::Word => format!("{} {n}", rng.pick(WORDS)),
    };
    truncate(s, max)
}

fn random_value(g: &Gen, n: u64, rng: &mut Rng) -> Value {
    let day0 = days_from_civil(2020, 1, 1);
    let days = days_from_civil(2026, 1, 1) - day0;
    match g {
        Gen::Bool => Value::Bool(rng.below(2) == 1),
        Gen::Int { min, max } => Value::Int(rng.range(*min, *max)),
        Gen::Float => Value::Float((rng.below(10_000_000) as f64) / 100.0),
        Gen::Decimal { int_digits, scale } => {
            let ip = if *int_digits == 0 { 0 } else { rng.below(10u64.pow(*int_digits)) };
            let fp = rng.below(10u64.pow(*scale));
            Value::Text(format!("{ip}.{fp:0w$}", w = *scale as usize))
        }
        Gen::Text { max, flavor } => Value::Text(text_value(*flavor, *max, n, rng)),
        Gen::Uuid => Value::Text(uuid(rng)),
        Gen::Json => Value::Text(format!("{{\"n\": {n}, \"tag\": \"{}\"}}", rng.pick(WORDS))),
        Gen::Choice(v) => Value::Text(v[rng.below(v.len() as u64) as usize].clone()),
        Gen::Inet => Value::Text(format!("10.{}.{}.{}", rng.below(256), rng.below(256), 1 + rng.below(254))),
        Gen::Date => Value::Date((day0 + rng.below(days as u64) as i64) as i32),
        Gen::Timestamp | Gen::TimestampTz => {
            let micros = (day0 + rng.below(days as u64) as i64) * MICROS_PER_DAY + rng.below(86_400) as i64 * 1_000_000;
            if *g == Gen::Timestamp { Value::Timestamp(micros) } else { Value::TimestampTz(micros) }
        }
        Gen::Time => Value::Time(rng.below(86_400) as i64 * 1_000_000),
        Gen::Binary { max } => {
            let len = max.unwrap_or(16).clamp(1, 16);
            Value::Bytes((0..len).map(|_| rng.below(256) as u8).collect())
        }
    }
}

/// Key number `i` (from [`key_start`], counting up) as a value: numbers
/// as they are, dates as days since 1970, date-times and times as
/// seconds, text, bytes and addresses encoded from it.
fn key_value(g: &Gen, i: i64, tag: &str, rng: &mut Rng) -> Value {
    match g {
        Gen::Int { .. } => Value::Int(i),
        Gen::Decimal { .. } => Value::Text(i.to_string()),
        Gen::Float => Value::Float(i as f64),
        Gen::Text { max, .. } => Value::Text(text_key(tag, i, *max)),
        Gen::Date => Value::Date(i as i32),
        Gen::Timestamp => Value::Timestamp(i * 1_000_000),
        Gen::TimestampTz => Value::TimestampTz(i * 1_000_000),
        Gen::Time => Value::Time(i * 1_000_000),
        Gen::Binary { max } => Value::Bytes(i.to_be_bytes()[8 - binary_key_len(*max)..].to_vec()),
        Gen::Inet => Value::Text(format!("10.{}.{}.{}", (i >> 16) & 255, (i >> 8) & 255, i & 255)),
        Gen::Bool => Value::Bool(i % 2 == 1),
        Gen::Choice(v) => Value::Text(v[i.rem_euclid(v.len() as i64) as usize].clone()),
        Gen::Uuid => Value::Text(uuid(rng)),
        Gen::Json => random_value(g, i as u64, rng),
    }
}

/// A parent's key as text → a value of the column's type.
fn cell_value(t: ColType, s: &str) -> Value {
    let s = s.trim();
    let tz = parse_ts;
    let v = match t {
        ColType::Bool => match s.to_ascii_lowercase().as_str() {
            "true" | "1" | "t" | "yes" => Some(Value::Bool(true)),
            "false" | "0" | "f" | "no" => Some(Value::Bool(false)),
            _ => None,
        },
        ColType::Int64 => s.parse().ok().or_else(|| s.parse::<f64>().ok().map(|f| f as i64)).map(Value::Int),
        ColType::UInt64 => s.parse().ok().map(Value::UInt),
        ColType::Float64 => s.parse().ok().map(Value::Float),
        ColType::Date => parse_date(s).map(Value::Date),
        ColType::Timestamp => tz(s).map(Value::Timestamp),
        ColType::TimestampTz => tz(s).map(Value::TimestampTz),
        ColType::Time => parse_time(s).map(Value::Time),
        ColType::Binary => None,
        ColType::Utf8 => Some(Value::Text(s.to_string())),
    };
    v.unwrap_or(Value::Null)
}

fn generate(
    columns: &[Column],
    sources: &[Source],
    nullable: &[bool],
    parents: &[Vec<Vec<Option<String>>>],
    rows: usize,
    null_percent: u8,
    rng: &mut Rng,
) -> Result<Vec<RecordBatch>> {
    let mut b = BatchBuilder::new(columns, BATCH.min(rows));
    let mut out = Vec::new();
    for i in 0..rows {
        // One parent tuple per foreign key per row, shared by its columns.
        let picks: Vec<usize> = parents
            .iter()
            .enumerate()
            .map(|(f, keys)| {
                let unique = sources.iter().any(|s| matches!(s, Source::Parent { fk, unique: true, .. } if *fk == f));
                if unique { i % keys.len() } else { rng.below(keys.len() as u64) as usize }
            })
            .collect();
        let row: Vec<Value> = sources
            .iter()
            .enumerate()
            .map(|(c, s)| match s {
                Source::Null => Value::Null,
                Source::Key { maker, start, tag } => key_value(maker, start + i as i64, tag, rng),
                Source::Parent { fk, slot, .. } => match parents[*fk][picks[*fk]].get(*slot).cloned().flatten() {
                    Some(text) => cell_value(columns[c].col_type, &text),
                    None => Value::Null,
                },
                Source::Random(g) => {
                    if nullable[c] && null_percent > 0 && rng.below(100) < null_percent as u64 {
                        Value::Null
                    } else {
                        random_value(g, i as u64 + 1, rng)
                    }
                }
            })
            .collect();
        b.push_row(row);
        if b.is_full() {
            out.push(b.finish()?);
        }
    }
    if !b.is_empty() {
        out.push(b.finish()?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use databrain_connector_core::arrow::array::{Array, Date32Array, Float64Array, Int64Array, StringArray};

    fn col(name: &str, t: &str, nullable: bool, pk: bool) -> ColumnInfo {
        ColumnInfo { name: name.into(), data_type: t.into(), nullable, is_primary_key: pk, default: None, comment: None }
    }

    #[test]
    fn types_pick_generators() {
        use ConnectorKind::*;
        let g = |k, t| parse_type(k, t, "x");
        assert_eq!(g(Postgres, "integer"), Some(Gen::Int { min: 1, max: 1_000_000 }));
        assert_eq!(g(Postgres, "int8"), Some(Gen::Int { min: 1, max: 1_000_000_000 }));
        assert_eq!(g(Clickhouse, "Nullable(Int8)"), Some(Gen::Int { min: 0, max: 100 }));
        assert_eq!(g(Postgres, "numeric(12,2)"), Some(Gen::Decimal { int_digits: 6, scale: 2 }));
        assert_eq!(g(Oracle, "NUMBER(5,0)"), Some(Gen::Int { min: 1, max: 99_999 }));
        assert_eq!(g(Postgres, "character varying(40)"), Some(Gen::Text { max: Some(40), flavor: Flavor::Word }));
        assert_eq!(g(Oracle, "VARCHAR2(50 BYTE)"), Some(Gen::Text { max: Some(50), flavor: Flavor::Word }));
        assert_eq!(g(Mssql, "nvarchar(max)"), Some(Gen::Text { max: None, flavor: Flavor::Word }));
        assert_eq!(g(Postgres, "timestamp(6) with time zone"), Some(Gen::TimestampTz));
        assert_eq!(g(Mysql, "timestamp"), Some(Gen::Timestamp));
        assert_eq!(g(Bigquery, "TIMESTAMP"), Some(Gen::TimestampTz));
        assert_eq!(g(Mssql, "bit"), Some(Gen::Bool));
        assert_eq!(g(Postgres, "bit(1)"), Some(Gen::Choice(vec!["0".into(), "1".into()])), "TRUE doesn't go into a bit string");
        assert_eq!(g(Mssql, "timestamp"), None, "rowversion");
        assert_eq!(g(Mysql, "enum('a','b''c')"), Some(Gen::Choice(vec!["a".into(), "b'c".into()])));
        assert_eq!(g(Postgres, "jsonb"), Some(Gen::Json));
        assert_eq!(g(Snowflake, "VARIANT"), None, "text literals don't go into VARIANT");
        assert_eq!(g(Postgres, "integer[]"), None);
        assert_eq!(g(Postgres, "geometry"), None);
        assert_eq!(g(Sqlite, "WHATEVER INT"), Some(Gen::Int { min: 1, max: 1_000_000 }));
        assert_eq!(parse_type(Postgres, "text", "customer_email"), Some(Gen::Text { max: None, flavor: Flavor::Email }));
    }

    #[test]
    fn database_filled_columns_are_found() {
        use ConnectorKind::*;
        let mut c = col("id", "integer", false, true);
        c.default = Some("nextval('t_id_seq'::regclass)".into());
        assert!(database_fills(Postgres, &c, None));
        let c = col("id", "int", false, true);
        assert!(database_fills(Mysql, &c, Some("CREATE TABLE t (\n  `id` int NOT NULL AUTO_INCREMENT,\n  `name` text\n)")));
        assert!(database_fills(Postgres, &c, Some("CREATE TABLE t (\n    id bigint GENERATED ALWAYS AS IDENTITY NOT NULL\n)")));
        assert!(!database_fills(Mysql, &col("name", "text", true, false), Some("CREATE TABLE t (\n  `id` int NOT NULL AUTO_INCREMENT,\n  `name` text\n)")));
        assert!(!database_fills(Mssql, &col("idx", "int", true, false), Some("  id int IDENTITY(1,1)")));
    }

    #[test]
    fn sql_server_identity_computed_and_rowversion_columns_are_left_out() {
        // The DDL as the SQL Server connector writes it.
        let ddl = "CREATE TABLE [dbo].[t] (\n    [id] int IDENTITY(1,1) NOT NULL,\n    [total] AS ([qty]*[price]) PERSISTED,\n    [qty] int NOT NULL,\n    [rv] timestamp NOT NULL\n);";
        let cols = vec![
            col("id", "int", false, true),
            col("total", "decimal(12,2)", true, false),
            col("qty", "int", false, false),
            col("rv", "timestamp", false, false),
        ];
        let (p, _) = plan_for(ConnectorKind::Mssql, "dbo", "t", &cols, &[], Some(ddl));
        let fills: Vec<bool> = p.columns.iter().map(|c| c.database_fills).collect();
        assert_eq!(fills, [true, true, false, true]);
        assert!(!p.columns[3].supported, "rowversion can't be written");
        assert!(p.warnings.is_empty(), "the database fills rowversion: no 'inserts will fail' {:?}", p.warnings);
        // Elsewhere TIMESTAMP is a date-time.
        assert!(!database_fills(ConnectorKind::Mysql, &col("rv", "timestamp", false, false), None));
    }

    #[test]
    fn default_seeds_survive_javascript_numbers() {
        for _ in 0..100 {
            assert!(new_seed() <= MAX_SEED);
        }
        assert_eq!(MAX_SEED, 9_007_199_254_740_991, "Number.MAX_SAFE_INTEGER");
    }

    #[test]
    fn the_next_key_is_read_exactly() {
        assert_eq!(after_number("9007199254740993"), Ok(Some(9_007_199_254_740_994)), "past 2^53, exact");
        assert_eq!(after_number(" 41 "), Ok(Some(42)));
        assert_eq!(after_number("12.50"), Ok(Some(13)));
        assert_eq!(after_number("12.000"), Ok(Some(13)));
        assert_eq!(after_number("-3.5"), Ok(Some(-3)));
        assert_eq!(after_number("1e3"), Ok(Some(1001)));
        assert_eq!(after_number("9223372036854775806"), Ok(Some(i64::MAX)));
        assert_eq!(after_number("9223372036854775807"), Err(()));
        assert_eq!(after_number("123456789012345678901234567890123456789"), Err(()));
        assert_eq!(after_number("x"), Ok(None));
        let int = Gen::Int { min: 1, max: 9 };
        assert_eq!(key_start(&int, Some("9007199254740993"), 10, "id", "bigint"), Ok(9_007_199_254_740_994));
        assert_eq!(key_start(&int, None, 10, "id", "bigint"), Ok(1));
        let e = key_start(&int, Some("9223372036854775800"), 100, "id", "bigint").unwrap_err();
        assert!(e.contains("no room for 100 more"), "{e}");
        assert!(key_start(&int, Some("9223372036854775807"), 1, "id", "bigint").is_err());
        assert!(key_start(&Gen::Float, Some("9007199254740990"), 10, "id", "float").is_err());
    }

    #[test]
    fn short_text_keys_stay_unique_and_continue_after_existing_rows() {
        let g = Gen::Text { max: Some(2), flavor: Flavor::Word };
        assert_eq!(key_start(&g, Some("0"), 1000, "code", "varchar(2)"), Ok(1));
        assert_eq!(key_start(&g, Some("100"), 10, "code", "varchar(2)"), Ok(101), "past the rows already there");
        let e = key_start(&g, Some("0"), 1296, "code", "varchar(2)").unwrap_err();
        assert!(e.contains("2 characters hold 1295 keys"), "{e}");
        let mut rng = Rng(1);
        let keys: Vec<Value> = (1..=1295).map(|i| key_value(&g, i, "abcd", &mut rng)).collect();
        assert!(keys.iter().all(|k| matches!(k, Value::Text(t) if t.chars().count() <= 2)));
        assert_eq!(keys.iter().map(|k| format!("{k:?}")).collect::<HashSet<_>>().len(), 1295);
        // Room for the tag: `tag-n` until it doesn't fit, then base 36 (no `-`, no clash).
        let g = Gen::Text { max: Some(6), flavor: Flavor::Word };
        assert_eq!(key_value(&g, 9, "abcd", &mut rng), Value::Text("abcd-9".into()));
        assert_eq!(key_value(&g, 10, "abcd", &mut rng), Value::Text("a".into()));
    }

    #[test]
    fn date_float_time_and_byte_keys_are_unique() {
        let mut rng = Rng(3);
        let day = days_from_civil(2024, 2, 3);
        assert_eq!(key_start(&Gen::Date, Some("2024-02-03"), 10, "d", "date"), Ok(day + 1));
        assert_eq!(key_start(&Gen::Date, Some("2024-02-03 00:00:00"), 10, "d", "date"), Ok(day + 1), "Oracle DATE");
        assert_eq!(key_start(&Gen::Date, None, 10, "d", "date"), Ok(days_from_civil(2020, 1, 1)));
        let ts = key_start(&Gen::Timestamp, Some("2024-02-03 04:05:06.5"), 10, "t", "timestamp").unwrap();
        assert_eq!(key_value(&Gen::Timestamp, ts, "", &mut rng), Value::Timestamp(parse_datetime("2024-02-03 04:05:07").unwrap()));
        assert!(key_start(&Gen::Time, Some("23:59:00"), 60, "t", "time").is_err());
        assert_eq!(key_start(&Gen::Time, Some("23:59:00"), 59, "t", "time"), Ok(86_341));
        assert!(key_start(&Gen::Binary { max: Some(1) }, Some("200"), 56, "b", "binary(1)").is_err());
        assert!(key_start(&Gen::Bool, None, 3, "b", "boolean").is_err());

        let columns = vec![Column::new("d", ColType::Date, "date"), Column::new("f", ColType::Float64, "real")];
        let sources = vec![Source::Key { maker: Gen::Date, start: day, tag: String::new() }, Source::Key { maker: Gen::Float, start: 7, tag: String::new() }];
        let b = generate(&columns, &sources, &[false, false], &[], 5000, 0, &mut rng).unwrap();
        let dates: HashSet<i32> = b.iter().flat_map(|x| x.column(0).as_any().downcast_ref::<Date32Array>().unwrap().values().to_vec()).collect();
        assert_eq!(dates.len(), 5000);
        assert!(dates.contains(&(day as i32)));
        let floats: HashSet<u64> = b
            .iter()
            .flat_map(|x| x.column(1).as_any().downcast_ref::<Float64Array>().unwrap().values().iter().map(|f| f.to_bits()).collect::<Vec<_>>())
            .collect();
        assert_eq!(floats.len(), 5000);
        let bytes: HashSet<Vec<u8>> = (1..=255)
            .map(|i| match key_value(&Gen::Binary { max: Some(1) }, i, "", &mut rng) {
                Value::Bytes(b) => b,
                v => panic!("{v:?}"),
            })
            .collect();
        assert_eq!(bytes.len(), 255);
    }

    #[test]
    fn plan_reports_foreign_keys_and_unsupported_columns() {
        let cols = vec![col("id", "integer", false, true), col("customer_id", "integer", false, false), col("shape", "geometry", false, false)];
        let fks =
            vec![ForeignKey { columns: vec!["customer_id".into()], ref_schema: String::new(), ref_table: "customers".into(), ref_columns: vec!["id".into()] }];
        let (p, gens) = plan_for(ConnectorKind::Postgres, "public", "orders", &cols, &fks, None);
        assert_eq!(p.table, "\"public\".\"orders\"");
        assert_eq!(p.foreign_keys[0].parent, "\"public\".\"customers\"");
        assert_eq!(p.columns[1].parent.as_deref(), Some("\"public\".\"customers\".id"));
        assert!(p.warnings[0].contains("1 foreign key"), "{:?}", p.warnings);
        assert!(p.warnings.iter().any(|w| w.contains("shape") && w.contains("inserts will fail")), "{:?}", p.warnings);
        assert!(!p.columns[2].supported && gens[2].is_none());
    }

    #[test]
    fn parent_key_queries_follow_the_row_limit_style() {
        assert_eq!(parent_keys_sql(ConnectorKind::Postgres, "\"p\"", &["id"], 10), "SELECT DISTINCT \"id\" FROM \"p\" WHERE \"id\" IS NOT NULL LIMIT 10");
        assert!(
            parent_keys_sql(ConnectorKind::Mssql, "[p]", &["a", "b"], 10)
                .starts_with("SELECT DISTINCT TOP 10 [a], [b] FROM [p] WHERE [a] IS NOT NULL AND [b] IS NOT NULL")
        );
        assert!(parent_keys_sql(ConnectorKind::Oracle, "\"P\"", &["ID"], 10).ends_with("FETCH FIRST 10 ROWS ONLY"));
    }

    #[test]
    fn rows_are_repeatable_keys_unique_and_parents_used() {
        let columns = vec![
            Column::new("id", ColType::Int64, "integer"),
            Column::new("name", ColType::Utf8, "varchar(5)"),
            Column::new("customer_id", ColType::Int64, "integer"),
            Column::new("note", ColType::Utf8, "text"),
        ];
        let mk = || {
            vec![
                Source::Key { maker: Gen::Int { min: 1, max: 9 }, start: 41, tag: "abcd".into() },
                Source::Random(Gen::Text { max: Some(5), flavor: Flavor::FullName }),
                Source::Parent { fk: 0, slot: 0, unique: false },
                Source::Random(Gen::Text { max: None, flavor: Flavor::Sentence }),
            ]
        };
        let parents = vec![vec![vec![Some("7".to_string())], vec![Some("9".to_string())]]];
        let nullable = [false, false, false, true];
        let a = generate(&columns, &mk(), &nullable, &parents, 10_000, 50, &mut Rng(7)).unwrap();
        let b = generate(&columns, &mk(), &nullable, &parents, 10_000, 50, &mut Rng(7)).unwrap();
        assert_eq!(a.len(), 2, "8192 rows per batch");
        assert_eq!(a, b, "same seed, same rows");
        let ids: Vec<i64> = a.iter().flat_map(|x| x.column(0).as_any().downcast_ref::<Int64Array>().unwrap().values().to_vec()).collect();
        assert_eq!(ids.first(), Some(&41));
        assert_eq!(ids.iter().collect::<HashSet<_>>().len(), 10_000, "keys unique");
        let fk = a[0].column(2).as_any().downcast_ref::<Int64Array>().unwrap();
        assert!(fk.values().iter().all(|v| *v == 7 || *v == 9));
        let names = a[0].column(1).as_any().downcast_ref::<StringArray>().unwrap();
        assert!((0..names.len()).all(|i| names.value(i).chars().count() <= 5));
        assert_eq!(names.null_count(), 0, "NOT NULL columns never get NULL");
        let notes = a[0].column(3);
        assert!(notes.null_count() > 3000 && notes.null_count() < 5200, "{}", notes.null_count());
    }

    #[test]
    fn parent_keys_convert_to_the_column_type() {
        assert_eq!(cell_value(ColType::Int64, "12"), Value::Int(12));
        assert_eq!(cell_value(ColType::Date, "2024-02-03"), Value::Date(days_from_civil(2024, 2, 3) as i32));
        assert_eq!(cell_value(ColType::TimestampTz, "2024-02-03T04:05:06Z"), Value::TimestampTz(parse_datetime("2024-02-03 04:05:06").unwrap()));
        assert_eq!(cell_value(ColType::Utf8, "a-1"), Value::Text("a-1".into()));
        assert_eq!(cell_value(ColType::Int64, "x"), Value::Null);
    }
}
