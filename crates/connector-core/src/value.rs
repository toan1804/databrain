//! Row-oriented values from drivers and conversion into Arrow record batches.
//!
//! Drivers decode native values into [`Value`] and push rows into a
//! [`BatchBuilder`], which produces Arrow `RecordBatch`es with a fixed schema.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;

use arrow::array::{
    ArrayRef, BinaryBuilder, BooleanBuilder, Date32Builder, Float64Builder, Int64Builder,
    StringBuilder, Time64MicrosecondBuilder, TimestampMicrosecondBuilder, UInt64Builder,
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
            ColType::TimestampTz => {
                DataType::Timestamp(TimeUnit::Microsecond, Some(Arc::from("UTC")))
            }
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
        Self {
            name: name.into(),
            col_type,
            db_type: db_type.into(),
        }
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
            ColType::TimestampTz => Self::TimestampTz(
                TimestampMicrosecondBuilder::with_capacity(cap).with_timezone("UTC"),
            ),
            ColType::Time => Self::Time(Time64MicrosecondBuilder::with_capacity(cap)),
        }
    }

    /// Append a value, coercing where lossless. Returns `false` when the value
    /// could not be represented (a null is appended instead).
    fn append(&mut self, v: Value) -> bool {
        use Value as V;
        if matches!(v, V::Null) {
            self.append_null();
            return true;
        }
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
                V::Int(x) => b.append_value(x as f64),
                V::UInt(x) => b.append_value(x as f64),
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
                V::Date(d) => b.append_value(d as i64 * MICROS_PER_DAY),
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
        true
    }

    fn fail(&mut self) -> bool {
        self.append_null();
        false
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
}

impl BatchBuilder {
    pub fn new(columns: &[Column], capacity: usize) -> Self {
        let types: Vec<ColType> = columns.iter().map(|c| c.col_type).collect();
        Self {
            schema: schema_for(columns),
            builders: types
                .iter()
                .map(|t| ColBuilder::new(*t, capacity))
                .collect(),
            types,
            rows: 0,
            capacity,
            coercion_failures: 0,
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

    /// Append one row. Missing trailing values become NULL; extras are ignored.
    pub fn push_row(&mut self, row: impl IntoIterator<Item = Value>) {
        let mut it = row.into_iter();
        for b in &mut self.builders {
            let ok = b.append(it.next().unwrap_or(Value::Null));
            if !ok {
                self.coercion_failures += 1;
            }
        }
        self.rows += 1;
    }

    /// Finish the current batch and reset the builder for the next one.
    pub fn finish(&mut self) -> Result<RecordBatch> {
        let arrays: Vec<ArrayRef> = self.builders.iter_mut().map(|b| b.finish()).collect();
        let rows = self.rows;
        self.rows = 0;
        self.builders = self
            .types
            .iter()
            .map(|t| ColBuilder::new(*t, self.capacity))
            .collect();
        let opts = RecordBatchOptions::new().with_row_count(Some(rows));
        Ok(RecordBatch::try_new_with_options(
            self.schema.clone(),
            arrays,
            &opts,
        )?)
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

/// Parse `YYYY-MM-DD` into days since epoch.
pub fn parse_date(s: &str) -> Option<i32> {
    let s = s.trim();
    let mut parts = s.get(..10)?.splitn(3, '-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    let d: u32 = parts.next()?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    i32::try_from(days_from_civil(y, m, d)).ok()
}

/// Parse `HH:MM:SS[.ffffff]` into microseconds since midnight.
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
    if !frac.is_empty() {
        let digits: String = frac
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .take(6)
            .collect();
        let n = digits.len() as u32;
        micros = digits.parse::<i64>().ok()? * 10i64.pow(6 - n);
    }
    Some(((h * 60 + m) * 60 + sec) * 1_000_000 + micros)
}

/// Parse `YYYY-MM-DD[ T]HH:MM:SS[.ffffff]` (optional trailing `Z`) into micros.
pub fn parse_datetime(s: &str) -> Option<i64> {
    let s = s.trim().trim_end_matches('Z');
    let days = parse_date(s)? as i64;
    let rest = s.get(10..).unwrap_or("").trim_start_matches(['T', ' ']);
    let t = if rest.is_empty() {
        0
    } else {
        parse_time(rest)?
    };
    Some(days * MICROS_PER_DAY + t)
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
    use arrow::datatypes::Int64Type;

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
    fn builder_coerces_and_counts_failures() {
        let cols = vec![
            Column::new("id", ColType::Int64, "int"),
            Column::new("name", ColType::Utf8, "text"),
        ];
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
}
