//! Row-oriented values from drivers and conversion into Arrow record batches.
//!
//! Drivers decode native values into [`Value`] and push rows into a
//! [`BatchBuilder`], which produces Arrow `RecordBatch`es with a fixed schema.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;

use arrow::array::{
    ArrayRef, BinaryBuilder, BooleanBuilder, Date32Builder, Float64Builder, Int64Builder, StringBuilder, Time64MicrosecondBuilder, TimestampMicrosecondBuilder,
    UInt64Builder,
};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use arrow::record_batch::{RecordBatch, RecordBatchOptions};

use crate::error::Result;

/// Field metadata key holding the database's native type name.
pub const META_DB_TYPE: &str = "databrain.db_type";

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
    /// Days since 1970-01-01.
    Date(i32),
    /// Microseconds since epoch, no time zone.
    Timestamp(i64),
    /// Microseconds since epoch, UTC.
    TimestampTz(i64),
    /// Microseconds since midnight.
    Time(i64),
}

/// Arrow-level column type chosen by a connector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColType {
    Bool,
    Int64,
    UInt64,
    Float64,
    Utf8,
    Binary,
    Date,
    Timestamp,
    TimestampTz,
    Time,
}

impl ColType {
    pub fn data_type(self) -> DataType {
        match self {
            ColType::Bool => DataType::Boolean,
            ColType::Int64 => DataType::Int64,
            ColType::UInt64 => DataType::UInt64,
            ColType::Float64 => DataType::Float64,
            ColType::Utf8 => DataType::Utf8,
            ColType::Binary => DataType::Binary,
            ColType::Date => DataType::Date32,
            ColType::Timestamp => DataType::Timestamp(TimeUnit::Microsecond, None),
            ColType::TimestampTz => DataType::Timestamp(TimeUnit::Microsecond, Some(Arc::from("UTC"))),
            ColType::Time => DataType::Time64(TimeUnit::Microsecond),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub col_type: ColType,
    /// Native database type name, e.g. `varchar(255)` or `int4`.
    pub db_type: String,
}

impl Column {
    pub fn new(name: impl Into<String>, col_type: ColType, db_type: impl Into<String>) -> Self {
        Self { name: name.into(), col_type, db_type: db_type.into() }
    }
}

/// Build an Arrow schema from columns. Duplicate column names are kept
/// (SQL allows them); Arrow permits duplicate field names.
pub fn schema_for(columns: &[Column]) -> SchemaRef {
    let fields: Vec<Field> = columns
        .iter()
        .map(|c| {
            let mut md = HashMap::new();
            md.insert(META_DB_TYPE.to_string(), c.db_type.clone());
            Field::new(&c.name, c.col_type.data_type(), true).with_metadata(md)
        })
        .collect();
    Arc::new(Schema::new(fields))
}

/// What [`ColBuilder::append`] did with a value.
#[derive(PartialEq)]
enum Appended {
    Ok,
    Failed,
    Rounded,
}

enum ColBuilder {
    Bool(BooleanBuilder),
    Int64(Int64Builder),
    UInt64(UInt64Builder),
    Float64(Float64Builder),
    Utf8(StringBuilder),
    Binary(BinaryBuilder),
    Date(Date32Builder),
    Timestamp(TimestampMicrosecondBuilder),
    TimestampTz(TimestampMicrosecondBuilder),
    Time(Time64MicrosecondBuilder),
}

impl ColBuilder {
    fn new(t: ColType, cap: usize) -> Self {
        match t {
            ColType::Bool => Self::Bool(BooleanBuilder::with_capacity(cap)),
            ColType::Int64 => Self::Int64(Int64Builder::with_capacity(cap)),
            ColType::UInt64 => Self::UInt64(UInt64Builder::with_capacity(cap)),
            ColType::Float64 => Self::Float64(Float64Builder::with_capacity(cap)),
            ColType::Utf8 => Self::Utf8(StringBuilder::with_capacity(cap, cap * 16)),
            ColType::Binary => Self::Binary(BinaryBuilder::with_capacity(cap, cap * 16)),
            ColType::Date => Self::Date(Date32Builder::with_capacity(cap)),
            ColType::Timestamp => Self::Timestamp(TimestampMicrosecondBuilder::with_capacity(cap)),
            ColType::TimestampTz => Self::TimestampTz(TimestampMicrosecondBuilder::with_capacity(cap).with_timezone("UTC")),
            ColType::Time => Self::Time(Time64MicrosecondBuilder::with_capacity(cap)),
        }
    }

    /// Append a value, coercing where it keeps its meaning. `Failed`: it
    /// couldn't be represented (a null is appended instead); `Rounded`: an
    /// integer past ±2^53 went into a float column and lost digits.
    /// `Timestamp` and `TimestampTz` columns take both kinds of value: both
    /// are microseconds of the same instant, nothing is lost.
    fn append(&mut self, v: Value) -> Appended {
        use Value as V;
        if matches!(v, V::Null) {
            self.append_null();
            return Appended::Ok;
        }
        const EXACT: u64 = 1 << 53;
        match self {
            Self::Bool(b) => match v {
                V::Bool(x) => b.append_value(x),
                V::Int(x) => b.append_value(x != 0),
                V::UInt(x) => b.append_value(x != 0),
                _ => return self.fail(),
            },
            Self::Int64(b) => match v {
                V::Int(x) => b.append_value(x),
                V::UInt(x) if i64::try_from(x).is_ok() => b.append_value(x as i64),
                V::Bool(x) => b.append_value(x as i64),
                V::Float(x) if x.fract() == 0.0 && x.abs() < 9.0e15 => b.append_value(x as i64),
                V::Text(s) => match s.trim().parse::<i64>() {
                    Ok(x) => b.append_value(x),
                    Err(_) => return self.fail(),
                },
                _ => return self.fail(),
            },
            Self::UInt64(b) => match v {
                V::UInt(x) => b.append_value(x),
                V::Int(x) if x >= 0 => b.append_value(x as u64),
                V::Text(s) => match s.trim().parse::<u64>() {
                    Ok(x) => b.append_value(x),
                    Err(_) => return self.fail(),
                },
                _ => return self.fail(),
            },
            Self::Float64(b) => match v {
                V::Float(x) => b.append_value(x),
                V::Int(x) => {
                    b.append_value(x as f64);
                    if x.unsigned_abs() > EXACT {
                        return Appended::Rounded;
                    }
                }
                V::UInt(x) => {
                    b.append_value(x as f64);
                    if x > EXACT {
                        return Appended::Rounded;
                    }
                }
                V::Text(s) => match s.trim().parse::<f64>() {
                    Ok(x) => b.append_value(x),
                    Err(_) => return self.fail(),
                },
                _ => return self.fail(),
            },
            Self::Utf8(b) => b.append_value(value_to_string(&v)),
            Self::Binary(b) => match v {
                V::Bytes(x) => b.append_value(x),
                V::Text(s) => b.append_value(s.as_bytes()),
                _ => return self.fail(),
            },
            Self::Date(b) => match v {
                V::Date(d) => b.append_value(d),
                V::Text(s) => match parse_date(&s) {
                    Some(d) => b.append_value(d),
                    None => return self.fail(),
                },
                _ => return self.fail(),
            },
            Self::Timestamp(b) | Self::TimestampTz(b) => match v {
                V::Timestamp(t) | V::TimestampTz(t) => b.append_value(t),
                V::Date(d) => match (d as i64).checked_mul(MICROS_PER_DAY) {
                    Some(t) => b.append_value(t),
                    None => return self.fail(),
                },
                V::Text(s) => match parse_datetime(&s) {
                    Some(t) => b.append_value(t),
                    None => return self.fail(),
                },
                _ => return self.fail(),
            },
            Self::Time(b) => match v {
                V::Time(t) => b.append_value(t),
                V::Text(s) => match parse_time(&s) {
                    Some(t) => b.append_value(t),
                    None => return self.fail(),
                },
                _ => return self.fail(),
            },
        }
        Appended::Ok
    }

    fn fail(&mut self) -> Appended {
        self.append_null();
        Appended::Failed
    }

    fn append_null(&mut self) {
        match self {
            Self::Bool(b) => b.append_null(),
            Self::Int64(b) => b.append_null(),
            Self::UInt64(b) => b.append_null(),
            Self::Float64(b) => b.append_null(),
            Self::Utf8(b) => b.append_null(),
            Self::Binary(b) => b.append_null(),
            Self::Date(b) => b.append_null(),
            Self::Timestamp(b) | Self::TimestampTz(b) => b.append_null(),
            Self::Time(b) => b.append_null(),
        }
    }

    fn finish(&mut self) -> ArrayRef {
        match self {
            Self::Bool(b) => Arc::new(b.finish()),
            Self::Int64(b) => Arc::new(b.finish()),
            Self::UInt64(b) => Arc::new(b.finish()),
            Self::Float64(b) => Arc::new(b.finish()),
            Self::Utf8(b) => Arc::new(b.finish()),
            Self::Binary(b) => Arc::new(b.finish()),
            Self::Date(b) => Arc::new(b.finish()),
            Self::Timestamp(b) | Self::TimestampTz(b) => Arc::new(b.finish()),
            Self::Time(b) => Arc::new(b.finish()),
        }
    }
}

/// Accumulates rows and emits Arrow record batches with a fixed schema.
pub struct BatchBuilder {
    schema: SchemaRef,
    types: Vec<ColType>,
    builders: Vec<ColBuilder>,
    rows: usize,
    capacity: usize,
    coercion_failures: u64,
    rounded: u64,
}

impl BatchBuilder {
    pub fn new(columns: &[Column], capacity: usize) -> Self {
        let types: Vec<ColType> = columns.iter().map(|c| c.col_type).collect();
        Self {
            schema: schema_for(columns),
            builders: types.iter().map(|t| ColBuilder::new(*t, capacity)).collect(),
            types,
            rows: 0,
            capacity,
            coercion_failures: 0,
            rounded: 0,
        }
    }

    pub fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    pub fn len(&self) -> usize {
        self.rows
    }

    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    pub fn is_full(&self) -> bool {
        self.rows >= self.capacity
    }

    /// Number of values that could not be converted to the column type and
    /// were replaced by NULL.
    pub fn coercion_failures(&self) -> u64 {
        self.coercion_failures
    }

    /// What to tell the user about the values so far: how many became NULL
    /// (`why`, e.g. "infinity dates", names the usual cause for the engine)
    /// and how many integers lost digits in a float column. Send each as a
    /// `StreamEvent::Notice` after the last batch.
    pub fn notices(&self, why: Option<&str>) -> Vec<String> {
        let mut out = Vec::new();
        if self.coercion_failures > 0 {
            let why = why.map(|w| format!(" (e.g. {w})")).unwrap_or_default();
            out.push(format!("{} value(s) did not fit their column's type{why} and are shown as NULL", self.coercion_failures));
        }
        if self.rounded > 0 {
            out.push(format!("{} integer(s) larger than 2^53 in a floating-point column were rounded", self.rounded));
        }
        out
    }

    /// Append one row. Missing trailing values become NULL; extras are ignored.
    pub fn push_row(&mut self, row: impl IntoIterator<Item = Value>) {
        let mut it = row.into_iter();
        for b in &mut self.builders {
            match b.append(it.next().unwrap_or(Value::Null)) {
                Appended::Ok => {}
                Appended::Failed => self.coercion_failures += 1,
                Appended::Rounded => self.rounded += 1,
            }
        }
        self.rows += 1;
    }

    /// Finish the current batch and reset the builder for the next one.
    pub fn finish(&mut self) -> Result<RecordBatch> {
        let arrays: Vec<ArrayRef> = self.builders.iter_mut().map(|b| b.finish()).collect();
        let rows = self.rows;
        self.rows = 0;
        self.builders = self.types.iter().map(|t| ColBuilder::new(*t, self.capacity)).collect();
        let opts = RecordBatchOptions::new().with_row_count(Some(rows));
        Ok(RecordBatch::try_new_with_options(self.schema.clone(), arrays, &opts)?)
    }
}

// ---------------------------------------------------------------------------
// Formatting and temporal helpers (no external date crate needed)
// ---------------------------------------------------------------------------

pub const MICROS_PER_DAY: i64 = 86_400_000_000;

/// Days since 1970-01-01 for a proleptic Gregorian date.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Inverse of [`days_from_civil`].
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn format_date(days: i64) -> String {
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

pub fn format_time(micros: i64) -> String {
    let secs = micros.div_euclid(1_000_000);
    let frac = micros.rem_euclid(1_000_000);
    let (h, mi, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    if frac == 0 {
        format!("{h:02}:{mi:02}:{s:02}")
    } else {
        let f = format!("{frac:06}");
        format!("{h:02}:{mi:02}:{s:02}.{}", f.trim_end_matches('0'))
    }
}

pub fn format_timestamp(micros: i64) -> String {
    let days = micros.div_euclid(MICROS_PER_DAY);
    let rem = micros.rem_euclid(MICROS_PER_DAY);
    format!("{} {}", format_date(days), format_time(rem))
}

/// Parse `YYYY-MM-DD` into days since epoch. A trailing ` BC` (Postgres)
/// is the year before 1: `0044-03-15 BC` is astronomical year -43.
pub fn parse_date(s: &str) -> Option<i32> {
    let s = s.trim();
    let (s, bc) = match s.strip_suffix(" BC") {
        Some(r) if r.len() == 10 => (r, true),
        _ => (s, false),
    };
    let mut parts = s.get(..10)?.splitn(3, '-');
    let y: i64 = parts.next()?.parse().ok()?;
    let y = if bc { 1 - y } else { y };
    let m: u32 = parts.next()?.parse().ok()?;
    let d: u32 = parts.next()?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    i32::try_from(days_from_civil(y, m, d)).ok()
}

/// Parse `HH:MM:SS[.fffffffff]` into microseconds since midnight (digits
/// past the sixth are dropped). Anything else after it (an offset, a
/// word) makes it `None`: the value isn't silently read as something else.
pub fn parse_time(s: &str) -> Option<i64> {
    let s = s.trim();
    let (hms, frac) = match s.split_once('.') {
        Some((a, b)) => (a, b),
        None => (s, ""),
    };
    let mut it = hms.splitn(3, ':');
    let h: i64 = it.next()?.parse().ok()?;
    let m: i64 = it.next()?.parse().ok()?;
    let sec: i64 = it.next().unwrap_or("0").parse().ok()?;
    let mut micros = 0i64;
    if s.contains('.') {
        if frac.is_empty() || !frac.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let digits = &frac[..frac.len().min(6)];
        micros = digits.parse::<i64>().ok()? * 10i64.pow(6 - digits.len() as u32);
    }
    Some(((h * 60 + m) * 60 + sec) * 1_000_000 + micros)
}

/// Parse `YYYY-MM-DD[ T]HH:MM:SS[.ffffff]` into micros, with an optional
/// offset (`Z`, `+02`, `-05:30`, `+0530`, `+01:00:00`): then the result is
/// UTC (`13:04:05.25+02:00` is 11:04:05.25).
pub fn parse_datetime(s: &str) -> Option<i64> {
    let s = s.trim();
    // Postgres puts ` BC` after the time (and its offset).
    let (s, bc) = match s.strip_suffix(" BC") {
        Some(r) => (r.trim_end(), true),
        None => (s, false),
    };
    let (s, offset) = split_utc_offset(s)?;
    let days = if bc { parse_date(&format!("{} BC", s.get(..10)?))? } else { parse_date(s)? } as i64;
    let rest = s.get(10..).unwrap_or("").trim_start_matches(['T', ' ']);
    let t = if rest.is_empty() { 0 } else { parse_time(rest)? };
    Some(days * MICROS_PER_DAY + t - offset)
}

/// A trailing UTC offset of a date-time: the text before it and the offset
/// in microseconds (0 without one). `None` for an offset that doesn't read.
fn split_utc_offset(s: &str) -> Option<(&str, i64)> {
    if let Some(r) = s.strip_suffix(['Z', 'z']) {
        return Some((r, 0));
    }
    // A sign after the time (past the date's own `-`s).
    let Some(at) = s.get(11..).and_then(|t| t.rfind(['+', '-'])).map(|i| i + 11) else { return Some((s, 0)) };
    let (dt, off) = (s[..at].trim_end(), &s[at + 1..]);
    if !off.bytes().all(|c| c.is_ascii_digit() || c == b':') || off.is_empty() {
        return None;
    }
    let parts: Vec<&str> = if off.contains(':') {
        off.split(':').collect()
    } else if off.len() == 4 {
        vec![&off[..2], &off[2..]]
    } else {
        vec![off]
    };
    if parts.len() > 3 || parts.iter().any(|p| p.is_empty() || p.len() > 2) {
        return None;
    }
    let n = |i: usize| parts.get(i).map_or(Some(0), |p| p.parse::<i64>().ok());
    let secs = (n(0)? * 60 + n(1)?) * 60 + n(2)?;
    let sign = if s.as_bytes()[at] == b'-' { -1 } else { 1 };
    Some((dt, sign * secs * 1_000_000))
}

pub fn hex_bytes(b: &[u8]) -> String {
    let mut s = String::with_capacity(2 + b.len() * 2);
    s.push_str("\\x");
    for x in b {
        let _ = write!(s, "{x:02x}");
    }
    s
}

pub fn value_to_string(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Text(s) => s.clone(),
        Value::Bytes(b) => match std::str::from_utf8(b) {
            Ok(s) => s.to_string(),
            Err(_) => hex_bytes(b),
        },
        Value::Date(d) => format_date(*d as i64),
        Value::Timestamp(t) => format_timestamp(*t),
        Value::TimestampTz(t) => format!("{}Z", format_timestamp(*t)),
        Value::Time(t) => format_time(*t),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Array, AsArray};
    use arrow::datatypes::{Float64Type, Int64Type};

    #[test]
    fn civil_roundtrip() {
        for days in [-719_468i64, -1, 0, 1, 10_957, 19_723, 2_932_896] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        assert_eq!(format_date(0), "1970-01-01");
        assert_eq!(parse_date("2024-02-29"), Some(19_782));
    }

    #[test]
    fn datetime_parse_format() {
        let t = parse_datetime("2024-03-05 13:04:05.25").unwrap();
        assert_eq!(format_timestamp(t), "2024-03-05 13:04:05.25");
        assert_eq!(parse_time("01:02:03"), Some(3_723_000_000));
    }

    #[test]
    fn date_times_with_offsets_are_utc() {
        let utc = |s: &str| parse_datetime(s);
        let at = utc("2024-01-02 11:04:05.25").unwrap();
        assert_eq!(utc("2024-01-02 13:04:05.25+02:00"), Some(at), "an offset after a fraction");
        assert_eq!(utc("2024-01-02T13:04:05.25+02"), Some(at));
        assert_eq!(utc("2024-01-02 13:04:05.25+0200"), Some(at));
        assert_eq!(utc("2024-01-02 06:34:05.25-04:30"), Some(at));
        assert_eq!(utc("2024-01-02 11:04:05.25Z"), Some(at));
        assert_eq!(utc("2024-01-02 13:04:05+02:00"), utc("2024-01-02 11:04:05"), "and without one");
        assert_eq!(utc("2024-01-02 13:04:05+01:00:00"), utc("2024-01-02 12:04:05"));
        assert_eq!(utc("2024-01-02"), Some(days_from_civil(2024, 1, 2) * MICROS_PER_DAY));
        // Not read as something else.
        for bad in ["2024-01-02 13:04:05.25xyz", "2024-01-02 13:04:05+2:x", "2024-01-02 13:04:05.", "2024-01-02 13:04:05+123:00"] {
            assert_eq!(utc(bad), None, "{bad}");
        }
        assert_eq!(parse_time("13:04:05.123456789"), parse_time("13:04:05.123456"), "nanoseconds are cut to micros");
        assert_eq!(parse_time("13:04:05+02"), None);
    }

    #[test]
    fn builder_coerces_and_counts_failures() {
        let cols = vec![Column::new("id", ColType::Int64, "int"), Column::new("name", ColType::Utf8, "text")];
        let mut b = BatchBuilder::new(&cols, 4);
        b.push_row([Value::Int(1), Value::Text("a".into())]);
        b.push_row([Value::Text("2".into()), Value::Int(7)]);
        b.push_row([Value::Text("x".into()), Value::Null]);
        assert_eq!(b.coercion_failures(), 1);
        let batch = b.finish().unwrap();
        assert_eq!(batch.num_rows(), 3);
        let ids = batch.column(0).as_primitive::<Int64Type>();
        assert_eq!(ids.value(1), 2);
        assert!(ids.is_null(2));
        assert_eq!(batch.column(1).as_string::<i32>().value(1), "7");
        assert!(b.is_empty());
    }

    #[test]
    fn builder_tells_what_it_lost() {
        let cols = vec![Column::new("f", ColType::Float64, "double"), Column::new("t", ColType::Timestamp, "timestamp")];
        let mut b = BatchBuilder::new(&cols, 8);
        // Exact below 2^53, rounded above; a far date that overflows micros.
        b.push_row([Value::Int(1 << 53), Value::Date(19_000)]);
        b.push_row([Value::Int((1 << 53) + 1), Value::Date(i32::MAX)]);
        b.push_row([Value::UInt(u64::MAX), Value::Text("infinity".into())]);
        assert_eq!(b.coercion_failures(), 2, "the overflowing date and infinity");
        assert_eq!(
            b.notices(Some("infinity dates")),
            vec![
                "2 value(s) did not fit their column's type (e.g. infinity dates) and are shown as NULL".to_string(),
                "2 integer(s) larger than 2^53 in a floating-point column were rounded".to_string(),
            ]
        );
        let batch = b.finish().unwrap();
        assert_eq!(batch.column(0).as_primitive::<Float64Type>().value(1), ((1i64 << 53) + 1) as f64, "kept, rounded");
        assert!(batch.column(1).is_null(1));
        assert!(BatchBuilder::new(&cols, 1).notices(None).is_empty());
    }

    #[test]
    fn dates_before_year_one() {
        assert_eq!(parse_date("0044-03-15 BC"), i32::try_from(days_from_civil(-43, 3, 15)).ok());
        assert_eq!(parse_date("0001-01-01 BC"), i32::try_from(days_from_civil(0, 1, 1)).ok());
        assert_eq!(parse_datetime("0044-03-15 12:00:00 BC"), Some(days_from_civil(-43, 3, 15) * MICROS_PER_DAY + 12 * 3_600_000_000));
        assert_eq!(parse_datetime("0044-03-15 12:00:00+02 BC"), Some(days_from_civil(-43, 3, 15) * MICROS_PER_DAY + 10 * 3_600_000_000));
    }
}
