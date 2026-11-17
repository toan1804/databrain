//! Bulk copy (`INSERT BULK`, as `bcp` and `SqlBulkCopy` send it) for loads.
//!
//! tiberius sends each value as its column's exact type and converts
//! nothing, so the rows are converted here, and only where that gives what
//! the load's INSERT literal gives ([`plan`]): a column pair that would
//! convert differently (a double into `real` or `decimal`, text into
//! `varchar`, a timestamp with more digits than its column, …) keeps the
//! load on INSERTs. The copy fires triggers and checks constraints, and
//! keeps NULLs, as INSERTs do.

use databrain_connector_core::ConnectorError;
use databrain_connector_core::Result;
use databrain_connector_core::arrow::array::Array;
use databrain_connector_core::arrow::array::AsArray;
use databrain_connector_core::arrow::array::RecordBatch;
use databrain_connector_core::arrow::datatypes::DataType as D;
use databrain_connector_core::arrow::datatypes::Field;
use databrain_connector_core::arrow::datatypes::TimeUnit;
use databrain_connector_core::arrow::datatypes::*;
use std::borrow::Cow;
use tiberius::ColumnData;
use tiberius::FixedLenType;
use tiberius::MetaDataColumn;
use tiberius::TokenRow;
use tiberius::TypeInfo;
use tiberius::VarLenType;
use tiberius::numeric::Numeric;
use tiberius::time::Date;
use tiberius::time::DateTime2;
use tiberius::time::DateTimeOffset;
use tiberius::time::Time;

/// Days from 0001-01-01 (SQL Server's day 0) to 1970-01-01.
const EPOCH_DAYS: i64 = 719_162;
/// 9999-12-31, the last day `date` and `datetime2` hold.
const LAST_DAY: i64 = 3_652_058;

/// How one column's values go into their target column.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Conv {
    Bit,
    /// An integer column of this many bytes (range checked).
    Int(u8),
    /// An integer into `decimal`/`numeric` (scale 0, the server's scale).
    IntDecimal,
    Float,
    /// A decimal of this scale into `decimal`/`numeric`, rounded to the
    /// column's scale half away from zero, as SQL Server rounds a literal.
    Decimal(i8),
    Text,
    Binary,
    Date,
    /// A date into `datetime2` of this scale (midnight).
    DateAsDateTime2(u8),
    /// A timestamp of this unit into `datetime2` of this scale.
    DateTime2(TimeUnit, u8),
    /// A timestamp of this unit into `datetimeoffset` of this scale, at this
    /// offset in minutes (the time zone the literal writes).
    DateTimeOffset(TimeUnit, u8, i16),
}

/// A column the copy writes: how its values convert, and what the server
/// would refuse in the middle of the copy, checked before it starts.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Col {
    pub conv: Conv,
    pub nullable: bool,
    /// Bytes the column holds (`nvarchar(n)`: 2n; `None`: `max`).
    pub max_bytes: Option<usize>,
}

/// The target column kinds bulk copy writes here (others: INSERTs).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Target {
    Bit,
    Int(u8),
    Float,
    Decimal,
    NText,
    Binary,
    Date,
    DateTime2(u8),
    DateTimeOffset(u8),
    Other,
}

fn target(ty: &TypeInfo) -> Target {
    match ty {
        TypeInfo::FixedLen(f) => match f {
            FixedLenType::Bit => Target::Bit,
            FixedLenType::Int1 => Target::Int(1),
            FixedLenType::Int2 => Target::Int(2),
            FixedLenType::Int4 => Target::Int(4),
            FixedLenType::Int8 => Target::Int(8),
            FixedLenType::Float8 => Target::Float,
            _ => Target::Other,
        },
        TypeInfo::VarLenSized(v) => match v.r#type() {
            VarLenType::Bitn => Target::Bit,
            VarLenType::Intn => Target::Int(v.len() as u8),
            VarLenType::Floatn if v.len() == 8 => Target::Float,
            VarLenType::NVarchar | VarLenType::NChar => Target::NText,
            VarLenType::BigVarBin | VarLenType::BigBinary => Target::Binary,
            VarLenType::Daten => Target::Date,
            VarLenType::Datetime2 => Target::DateTime2(v.len() as u8),
            VarLenType::DatetimeOffsetn => Target::DateTimeOffset(v.len() as u8),
            _ => Target::Other,
        },
        TypeInfo::VarLenSizedPrecision { ty: VarLenType::Decimaln | VarLenType::Numericn, .. } => Target::Decimal,
        _ => Target::Other,
    }
}

fn digits(u: TimeUnit) -> u8 {
    match u {
        TimeUnit::Second => 0,
        TimeUnit::Millisecond => 3,
        TimeUnit::Microsecond => 6,
        TimeUnit::Nanosecond => 9,
    }
}

/// A time zone's fixed offset in minutes (`UTC`, `Z`, `+07:00`, `-0530`);
/// `None` for a named zone, whose offset changes over the year.
fn fixed_offset(tz: &str) -> Option<i16> {
    let tz = tz.trim();
    if tz.eq_ignore_ascii_case("utc") || tz == "Z" || tz.eq_ignore_ascii_case("etc/utc") {
        return Some(0);
    }
    let (sign, rest) = match tz.as_bytes().first()? {
        b'+' => (1, &tz[1..]),
        b'-' => (-1, &tz[1..]),
        _ => return None,
    };
    let digits: String = rest.chars().filter(|c| *c != ':').collect();
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (h, m): (i16, i16) = (digits[..2].parse().ok()?, digits[2..].parse().ok()?);
    (h <= 14 && m < 60).then_some(sign * (h * 60 + m))
}

/// How the rows' column `f` goes into `col` by bulk copy, when that writes
/// what the INSERT literal writes; `Err` says why not.
pub(crate) fn plan(f: &Field, col: &MetaDataColumn<'_>) -> std::result::Result<Col, String> {
    let name = col.col_name();
    if col.base.is_identity() {
        return Err(format!("{name} is an identity column"));
    }
    if !col.base.is_updateable() {
        return Err(format!("{name} is computed or read-only"));
    }
    let t = target(col.base.ty());
    let conv = match (f.data_type(), t) {
        (D::Boolean, Target::Bit) => Some(Conv::Bit),
        (D::Int8 | D::Int16 | D::Int32 | D::Int64 | D::UInt8 | D::UInt16 | D::UInt32 | D::UInt64, Target::Int(n)) if matches!(n, 1 | 2 | 4 | 8) => {
            Some(Conv::Int(n))
        }
        (D::Int8 | D::Int16 | D::Int32 | D::Int64 | D::UInt8 | D::UInt16 | D::UInt32 | D::UInt64, Target::Decimal) => Some(Conv::IntDecimal),
        (D::Float64, Target::Float) => Some(Conv::Float),
        (D::Decimal128(_, s), Target::Decimal) if *s >= 0 => Some(Conv::Decimal(*s)),
        (D::Utf8 | D::LargeUtf8 | D::Utf8View, Target::NText) => Some(Conv::Text),
        (D::Binary | D::LargeBinary | D::BinaryView, Target::Binary) => Some(Conv::Binary),
        (D::Date32, Target::Date) => Some(Conv::Date),
        (D::Date32, Target::DateTime2(s)) => Some(Conv::DateAsDateTime2(s)),
        (D::Timestamp(u, None), Target::DateTime2(s)) if digits(*u) <= s => Some(Conv::DateTime2(*u, s)),
        // Only `datetimeoffset(7)`: the server refuses tiberius's copy into
        // one of a smaller scale ("Invalid column attribute from bcp client").
        (D::Timestamp(_, Some(_)), Target::DateTimeOffset(s)) if s != 7 => {
            return Err(format!("{} into {name} (datetimeoffset({s}): bulk copy only takes datetimeoffset(7))", f.name()));
        }
        (D::Timestamp(u, Some(tz)), Target::DateTimeOffset(s)) if digits(*u) <= s => fixed_offset(tz).map(|o| Conv::DateTimeOffset(*u, s, o)),
        _ => None,
    };
    let conv = conv.ok_or_else(|| format!("{} ({}) into {name} ({})", f.name(), f.data_type(), type_name(col.base.ty())))?;
    let max_bytes = match col.base.ty() {
        TypeInfo::VarLenSized(v) if matches!(v.r#type(), VarLenType::NVarchar | VarLenType::NChar | VarLenType::BigVarBin | VarLenType::BigBinary) => {
            (v.len() < 0xffff).then_some(v.len())
        }
        _ => None,
    };
    Ok(Col { conv, nullable: col.base.is_nullable(), max_bytes })
}

/// A column type as SQL Server names it, for messages.
fn type_name(ty: &TypeInfo) -> String {
    match ty {
        TypeInfo::FixedLen(f) => format!("{f:?}").to_lowercase(),
        TypeInfo::VarLenSized(v) => match v.r#type() {
            VarLenType::BigVarChar => "varchar".into(),
            VarLenType::BigChar => "char".into(),
            VarLenType::Floatn if v.len() == 4 => "real".into(),
            VarLenType::Datetimen => "datetime".into(),
            VarLenType::Money => "money".into(),
            t => format!("{t:?}").to_lowercase(),
        },
        TypeInfo::VarLenSizedPrecision { precision, scale, .. } => format!("decimal({precision}, {scale})"),
        TypeInfo::Xml { .. } => "xml".into(),
        TypeInfo::Udt(u) => u.type_name.clone(),
    }
}

fn int_value(a: &dyn Array, r: usize) -> i128 {
    match a.data_type() {
        D::Int8 => a.as_primitive::<Int8Type>().value(r) as i128,
        D::Int16 => a.as_primitive::<Int16Type>().value(r) as i128,
        D::Int32 => a.as_primitive::<Int32Type>().value(r) as i128,
        D::Int64 => a.as_primitive::<Int64Type>().value(r) as i128,
        D::UInt8 => a.as_primitive::<UInt8Type>().value(r) as i128,
        D::UInt16 => a.as_primitive::<UInt16Type>().value(r) as i128,
        D::UInt32 => a.as_primitive::<UInt32Type>().value(r) as i128,
        _ => a.as_primitive::<UInt64Type>().value(r) as i128,
    }
}

fn timestamp_value(a: &dyn Array, r: usize) -> i64 {
    match a.data_type() {
        D::Timestamp(TimeUnit::Second, _) => a.as_primitive::<TimestampSecondType>().value(r),
        D::Timestamp(TimeUnit::Millisecond, _) => a.as_primitive::<TimestampMillisecondType>().value(r),
        D::Timestamp(TimeUnit::Microsecond, _) => a.as_primitive::<TimestampMicrosecondType>().value(r),
        _ => a.as_primitive::<TimestampNanosecondType>().value(r),
    }
}

fn out_of_range(col: &str, v: impl std::fmt::Display) -> ConnectorError {
    ConnectorError::query(format!("{v} is out of range for column {col}"))
}

fn date(days_since_epoch: i64, col: &str) -> Result<Date> {
    let d = days_since_epoch + EPOCH_DAYS;
    if !(0..=LAST_DAY).contains(&d) {
        return Err(out_of_range(col, format!("day {days_since_epoch} after 1970-01-01")));
    }
    Ok(Date::new(d as u32))
}

/// A timestamp `t` in `unit` as a `datetime2` of `scale` (exact: the unit
/// has no more digits than the scale).
fn datetime2(t: i64, unit: TimeUnit, scale: u8, col: &str) -> Result<DateTime2> {
    let per_second = 10i64.pow(digits(unit) as u32);
    let per_day = 86_400 * per_second;
    let (days, rem) = (t.div_euclid(per_day), t.rem_euclid(per_day));
    let increments = rem as u64 * 10u64.pow((scale - digits(unit)) as u32);
    Ok(DateTime2::new(date(days, col)?, Time::new(increments, scale)))
}

/// Row `r` of column `a` as `conv` makes it.
fn value(a: &dyn Array, r: usize, conv: Conv, col: &str) -> Result<ColumnData<'static>> {
    let null = a.is_null(r);
    Ok(match conv {
        Conv::Bit => ColumnData::Bit((!null).then(|| a.as_boolean().value(r))),
        Conv::Int(n) => {
            let v = (!null).then(|| int_value(a, r));
            let fits = |lo: i128, hi: i128| v.is_none_or(|v| (lo..=hi).contains(&v));
            match n {
                1 if fits(0, 255) => ColumnData::U8(v.map(|v| v as u8)),
                2 if fits(i16::MIN as i128, i16::MAX as i128) => ColumnData::I16(v.map(|v| v as i16)),
                4 if fits(i32::MIN as i128, i32::MAX as i128) => ColumnData::I32(v.map(|v| v as i32)),
                8 if fits(i64::MIN as i128, i64::MAX as i128) => ColumnData::I64(v.map(|v| v as i64)),
                _ => return Err(out_of_range(col, v.unwrap_or_default())),
            }
        }
        Conv::IntDecimal => ColumnData::Numeric((!null).then(|| Numeric::new_with_scale(int_value(a, r), 0))),
        // NaN and ±inf: NULL, as the INSERT writes them.
        Conv::Float => ColumnData::F64((!null).then(|| a.as_primitive::<Float64Type>().value(r)).filter(|v| v.is_finite())),
        Conv::Decimal(s) => ColumnData::Numeric((!null).then(|| Numeric::new_with_scale(a.as_primitive::<Decimal128Type>().value(r), s as u8))),
        Conv::Text => ColumnData::String((!null).then(|| {
            Cow::Owned(match a.data_type() {
                D::Utf8 => a.as_string::<i32>().value(r).to_string(),
                D::LargeUtf8 => a.as_string::<i64>().value(r).to_string(),
                _ => a.as_string_view().value(r).to_string(),
            })
        })),
        Conv::Binary => ColumnData::Binary((!null).then(|| {
            Cow::Owned(match a.data_type() {
                D::Binary => a.as_binary::<i32>().value(r).to_vec(),
                D::LargeBinary => a.as_binary::<i64>().value(r).to_vec(),
                _ => a.as_binary_view().value(r).to_vec(),
            })
        })),
        Conv::Date => ColumnData::Date(if null { None } else { Some(date(a.as_primitive::<Date32Type>().value(r) as i64, col)?) }),
        Conv::DateAsDateTime2(s) => {
            ColumnData::DateTime2(if null { None } else { Some(DateTime2::new(date(a.as_primitive::<Date32Type>().value(r) as i64, col)?, Time::new(0, s))) })
        }
        Conv::DateTime2(u, s) => ColumnData::DateTime2(if null { None } else { Some(datetime2(timestamp_value(a, r), u, s, col)?) }),
        // The value on the wire is the instant in UTC, with the offset.
        Conv::DateTimeOffset(u, s, o) => {
            ColumnData::DateTimeOffset(if null { None } else { Some(DateTimeOffset::new(datetime2(timestamp_value(a, r), u, s, col)?, o)) })
        }
    })
}

/// Row `r` of `batch` for the copy.
pub(crate) fn row(batch: &RecordBatch, r: usize, cols: &[Col], names: &[String]) -> Result<TokenRow<'static>> {
    let mut row = TokenRow::with_capacity(cols.len());
    for ((a, c), n) in batch.columns().iter().zip(cols).zip(names) {
        row.push(value(a.as_ref(), r, c.conv, n)?);
    }
    Ok(row)
}

/// The rows' values the server would refuse, found before the copy starts
/// (a copy can't stop part way without dropping the connection, and with
/// it the session's transaction and #temp tables): one that doesn't
/// convert, a NULL into a NOT NULL column, text or bytes longer than the
/// column. The error names the row among `batch`'s, from 1.
pub(crate) fn check(batch: &RecordBatch, cols: &[Col], names: &[String]) -> Result<()> {
    for ((a, c), n) in batch.columns().iter().zip(cols).zip(names) {
        for r in 0..batch.num_rows() {
            let at = |e: ConnectorError| ConnectorError::query(format!("{} (row {} of the call)", e.message, r + 1));
            let v = value(a.as_ref(), r, c.conv, n).map_err(at)?;
            let (null, bytes) = match &v {
                ColumnData::String(s) => (s.is_none(), s.as_ref().map(|s| s.encode_utf16().count() * 2)),
                ColumnData::Binary(b) => (b.is_none(), b.as_ref().map(|b| b.len())),
                _ => (a.is_null(r) || matches!(v, ColumnData::F64(None)), None),
            };
            if null && !c.nullable {
                return Err(at(ConnectorError::query(format!("Cannot insert the value NULL into column {n}, which does not allow nulls"))));
            }
            if let (Some(b), Some(max)) = (bytes, c.max_bytes)
                && b > max
            {
                return Err(at(ConnectorError::query(format!("String or binary data would be truncated in column {n} ({b} bytes, it holds {max})"))));
            }
        }
    }
    Ok(())
}

/// The rows of `batch` checked and converted for a copy into a table the
/// load created (its columns as `CREATE TABLE` makes them), as a call does:
/// for `crates/bench`. Values converted.
#[doc(hidden)]
pub fn bench_rows(batch: &RecordBatch) -> Result<usize> {
    let cols: Vec<Col> = batch
        .schema()
        .fields()
        .iter()
        .map(|f| {
            let conv = match f.data_type() {
                D::Boolean => Conv::Bit,
                D::Float64 => Conv::Float,
                D::Decimal128(_, s) => Conv::Decimal(*s),
                D::Utf8 | D::LargeUtf8 | D::Utf8View => Conv::Text,
                D::Binary | D::LargeBinary | D::BinaryView => Conv::Binary,
                D::Date32 => Conv::Date,
                D::Timestamp(u, None) => Conv::DateTime2(*u, 7),
                D::Timestamp(u, Some(_)) => Conv::DateTimeOffset(*u, 7, 0),
                _ => Conv::Int(8),
            };
            Col { conv, nullable: true, max_bytes: None }
        })
        .collect();
    let names: Vec<String> = batch.schema().fields().iter().map(|f| f.name().clone()).collect();
    check(batch, &cols, &names)?;
    let mut n = 0;
    for r in 0..batch.num_rows() {
        n += row(batch, r, &cols, &names)?.len();
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A column as the server describes it (`flags`: tiberius's `ColumnFlag`s).
    macro_rules! col {
        ($ty:expr, $flags:expr) => {
            MetaDataColumn { base: tiberius::BaseMetaDataColumn { flags: $flags, ty: $ty, table_name: None }, col_name: "c".into() }
        };
    }

    fn var(t: VarLenType, len: usize) -> TypeInfo {
        TypeInfo::VarLenSized(tiberius::VarLenContext::new(t, len, None))
    }

    /// The pairs that convert as the INSERT literal does go by bulk copy;
    /// others, identity and computed columns get INSERTs, saying why.
    #[test]
    fn plan_per_pair() {
        use tiberius::ColumnFlag as F;
        let rw = F::Nullable | F::Updateable;
        let f = |t: D| Field::new("x", t, true);
        let utc = D::Timestamp(TimeUnit::Microsecond, Some("UTC".into()));
        let dec = TypeInfo::VarLenSizedPrecision { ty: VarLenType::Decimaln, size: 9, precision: 18, scale: 2 };
        for (field, ty, conv) in [
            (f(D::Boolean), var(VarLenType::Bitn, 1), Conv::Bit),
            (f(D::Int64), var(VarLenType::Intn, 4), Conv::Int(4)),
            (f(D::Int32), TypeInfo::FixedLen(FixedLenType::Int8), Conv::Int(8)),
            (f(D::Int64), dec.clone(), Conv::IntDecimal),
            (f(D::Float64), var(VarLenType::Floatn, 8), Conv::Float),
            (f(D::Decimal128(12, 3)), dec.clone(), Conv::Decimal(3)),
            (f(D::Utf8), var(VarLenType::NVarchar, 0xffff), Conv::Text),
            (f(D::Binary), var(VarLenType::BigVarBin, 100), Conv::Binary),
            (f(D::Date32), var(VarLenType::Daten, 3), Conv::Date),
            (f(D::Date32), var(VarLenType::Datetime2, 3), Conv::DateAsDateTime2(3)),
            (f(D::Timestamp(TimeUnit::Microsecond, None)), var(VarLenType::Datetime2, 6), Conv::DateTime2(TimeUnit::Microsecond, 6)),
            (f(utc.clone()), var(VarLenType::DatetimeOffsetn, 7), Conv::DateTimeOffset(TimeUnit::Microsecond, 7, 0)),
        ] {
            assert_eq!(plan(&field, &col!(ty.clone(), rw)).map(|c| c.conv), Ok(conv), "{field:?} into {ty:?}");
        }
        for (field, ty) in [
            // A double's literal into real or decimal rounds from its text.
            (f(D::Float64), var(VarLenType::Floatn, 4)),
            (f(D::Float64), dec.clone()),
            (f(D::Float32), var(VarLenType::Floatn, 8)),
            // varchar: the literal is N'…', which the server maps to the code page.
            (f(D::Utf8), var(VarLenType::BigVarChar, 100)),
            (f(D::Int64), var(VarLenType::NVarchar, 100)),
            // More digits than the column: the literal rounds.
            (f(D::Timestamp(TimeUnit::Microsecond, None)), var(VarLenType::Datetime2, 3)),
            (f(D::Timestamp(TimeUnit::Nanosecond, None)), var(VarLenType::Datetime2, 7)),
            (f(utc.clone()), var(VarLenType::DatetimeOffsetn, 6)),
            (f(D::Timestamp(TimeUnit::Second, Some("Asia/Bangkok".into()))), var(VarLenType::DatetimeOffsetn, 7)),
            (f(D::Timestamp(TimeUnit::Second, None)), var(VarLenType::Datetimen, 8)),
        ] {
            assert!(plan(&field, &col!(ty.clone(), rw)).is_err(), "{field:?} into {ty:?}");
        }
        let int = var(VarLenType::Intn, 8);
        assert!(plan(&f(D::Int64), &col!(int.clone(), F::Identity.into())).unwrap_err().contains("identity"));
        assert!(plan(&f(D::Int64), &col!(int, F::Nullable.into())).unwrap_err().contains("computed"));
    }

    /// Values the server would refuse part way are found before the copy.
    #[test]
    fn checked_before_the_copy() {
        use databrain_connector_core::arrow::array::{Int64Array, StringArray};
        use std::sync::Arc;
        let schema = Arc::new(databrain_connector_core::arrow::datatypes::Schema::new(vec![Field::new("n", D::Int64, true), Field::new("s", D::Utf8, true)]));
        let batch =
            RecordBatch::try_new(schema, vec![Arc::new(Int64Array::from(vec![Some(1), None])), Arc::new(StringArray::from(vec![Some("abcd"), None]))]).unwrap();
        let names = ["n".to_string(), "s".to_string()];
        let c = |conv, nullable, max_bytes| Col { conv, nullable, max_bytes };
        assert!(check(&batch, &[c(Conv::Int(4), true, None), c(Conv::Text, true, Some(8))], &names).is_ok());
        let e = check(&batch, &[c(Conv::Int(4), false, None), c(Conv::Text, true, None)], &names).unwrap_err();
        assert!(e.message.contains("NULL into column n") && e.message.contains("row 2"), "{}", e.message);
        let e = check(&batch, &[c(Conv::Int(4), true, None), c(Conv::Text, true, Some(6))], &names).unwrap_err();
        assert!(e.message.contains("truncated in column s") && e.message.contains("row 1"), "{}", e.message);
        let big = RecordBatch::try_new(
            Arc::new(databrain_connector_core::arrow::datatypes::Schema::new(vec![Field::new("n", D::Int64, true)])),
            vec![Arc::new(Int64Array::from(vec![300]))],
        )
        .unwrap();
        assert!(check(&big, &[c(Conv::Int(1), true, None)], &names[..1]).unwrap_err().message.contains("out of range"));
    }

    #[test]
    fn fixed_offsets() {
        assert_eq!(fixed_offset("UTC"), Some(0));
        assert_eq!(fixed_offset("+00:00"), Some(0));
        assert_eq!(fixed_offset("+07:00"), Some(420));
        assert_eq!(fixed_offset("-0530"), Some(-330));
        assert_eq!(fixed_offset("Asia/Bangkok"), None);
        assert_eq!(fixed_offset("+7"), None);
    }

    #[test]
    fn datetime2_is_exact() {
        // 2026-01-01 10:00:00.123456 (microseconds) into datetime2(7).
        let t = 1_767_261_600_123_456i64;
        let dt = datetime2(t, TimeUnit::Microsecond, 7, "c").unwrap();
        assert_eq!(dt.date().days() as i64, EPOCH_DAYS + 20_454);
        assert_eq!(dt.time().increments(), 360_001_234_560);
        // Before 1970.
        let dt = datetime2(-1, TimeUnit::Second, 0, "c").unwrap();
        assert_eq!((dt.date().days() as i64, dt.time().increments()), (EPOCH_DAYS - 1, 86_399));
        assert!(date(-EPOCH_DAYS - 1, "c").is_err());
    }
}
