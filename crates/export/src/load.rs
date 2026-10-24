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
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct BatchSize {
    pub rows: usize,
    pub bytes: usize,
}

/// Batch sizes a dialect allows, and what DataBrain uses by default.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
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
    Bool,
    Binary,
    Date,
    Timestamp,
    Text,
}

fn lit_kind(f: &Field) -> Lit {
    match f.data_type() {
        DataType::Boolean => Lit::Bool,
        t if t.is_numeric() => Lit::Number,
        DataType::Binary | DataType::LargeBinary | DataType::FixedSizeBinary(_) => Lit::Binary,
        DataType::Date32 | DataType::Date64 => Lit::Date,
        DataType::Timestamp(_, None) => Lit::Timestamp,
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
        DataType::Int8 | DataType::Int16 | DataType::Int32 | DataType::Int64 | DataType::UInt8 | DataType::UInt16 | DataType::UInt32 | DataType::UInt64 => t.integer,
        DataType::Float16 | DataType::Float32 | DataType::Float64 => t.float,
        DataType::Decimal128(..) | DataType::Decimal256(..) => t.decimal,
        DataType::Date32 | DataType::Date64 => t.date,
        DataType::Timestamp(_, tz) => if tz.is_some() { t.timestamp_tz } else { t.timestamp },
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
        .map(|f| format!("{} {}{}", quote_ident(dialect, f.name()), column_type(dialect, f), if f.is_nullable() { "" } else { " NOT NULL" }))
        .collect();
    format!("CREATE TABLE {} ({})", quote_path(dialect, table), cols.join(", "))
}

/// [`create_table_sql`] that does nothing when the table already exists
/// (it may have been created by SQL run just before).
pub fn create_table_if_missing_sql(dialect: ConnectorKind, table: &str, schema: &SchemaRef) -> String {
    let create = create_table_sql(dialect, table, schema);
    match lt(dialect).create_if_missing {
        CreateIfMissing::PlsqlBlock => format!("BEGIN EXECUTE IMMEDIATE '{}'; EXCEPTION WHEN OTHERS THEN IF SQLCODE != -955 THEN RAISE; END IF; END;", create.replace('\'', "''")),
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

/// `DELETE FROM table` (Truncate mode: transactional everywhere, unlike `TRUNCATE`).
pub fn delete_all_sql(dialect: ConnectorKind, table: &str) -> String {
    format!("DELETE FROM {}", quote_path(dialect, table))
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

/// Engines that update/merge with a native `MERGE`; the others use
/// `UPDATE … FROM` / `UPDATE … JOIN` and `INSERT … WHERE NOT EXISTS`.
fn native_merge(dialect: ConnectorKind) -> bool {
    !matches!(lt(dialect).merge, MergeStyle::UpdateFrom | MergeStyle::UpdateJoin)
}

fn merge_sql(dialect: ConnectorKind, target: &str, stage: &str, cols: &[String], keys: &[String], insert: bool) -> String {
    let style = lt(dialect).merge;
    let q = |c: &str| quote_ident(dialect, c);
    let (t, s) = (quote_path(dialect, target), quote_path(dialect, stage));
    let on = keys.iter().map(|k| format!("tg.{} = src.{}", q(k), q(k))).collect::<Vec<_>>().join(" AND ");
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

fn update_join_sql(dialect: ConnectorKind, target: &str, stage: &str, cols: &[String], keys: &[String]) -> Option<String> {
    let q = |c: &str| quote_ident(dialect, c);
    let (t, s) = (quote_path(dialect, target), quote_path(dialect, stage));
    let on = keys.iter().map(|k| format!("tg.{} = src.{}", q(k), q(k))).collect::<Vec<_>>().join(" AND ");
    let set: Vec<&String> = cols.iter().filter(|c| !keys.contains(c)).collect();
    if set.is_empty() {
        return None;
    }
    Some(match lt(dialect).merge {
        MergeStyle::UpdateJoin => format!("UPDATE {t} AS tg JOIN {s} AS src ON {on} SET {}", set.iter().map(|c| format!("tg.{} = src.{}", q(c), q(c))).collect::<Vec<_>>().join(", ")),
        _ => format!("UPDATE {t} AS tg SET {} FROM {s} AS src WHERE {on}", set.iter().map(|c| format!("{} = src.{}", q(c), q(c))).collect::<Vec<_>>().join(", ")),
    })
}

/// Update rows of `target` from `stage` where `keys` match (`cols` = every
/// staged column, keys included). Empty when only key columns are given.
pub fn update_from_stage_sql(dialect: ConnectorKind, target: &str, stage: &str, cols: &[String], keys: &[String]) -> Vec<String> {
    if !cols.iter().any(|c| !keys.contains(c)) {
        return vec![];
    }
    if native_merge(dialect) {
        return vec![merge_sql(dialect, target, stage, cols, keys, false)];
    }
    update_join_sql(dialect, target, stage, cols, keys).into_iter().collect()
}

/// Upsert `stage` into `target`: update rows whose `keys` match, insert the others.
pub fn merge_from_stage_sql(dialect: ConnectorKind, target: &str, stage: &str, cols: &[String], keys: &[String]) -> Vec<String> {
    if native_merge(dialect) {
        return vec![merge_sql(dialect, target, stage, cols, keys, true)];
    }
    let q = |c: &str| quote_ident(dialect, c);
    let (t, s) = (quote_path(dialect, target), quote_path(dialect, stage));
    let on = keys.iter().map(|k| format!("tg.{} = src.{}", q(k), q(k))).collect::<Vec<_>>().join(" AND ");
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
        true => format!("BEGIN EXECUTE IMMEDIATE 'DROP TABLE {}'; EXCEPTION WHEN OTHERS THEN IF SQLCODE != -942 THEN RAISE; END IF; END;", t.replace('\'', "''")),
        false => format!("DROP TABLE IF EXISTS {t}"),
    }
}

pub(crate) fn literal(cell: Option<&str>, kind: Lit, dialect: ConnectorKind) -> String {
    let l = lt(dialect);
    let Some(s) = cell else { return "NULL".into() };
    match kind {
        Lit::Number if s.parse::<f64>().is_ok_and(|f| f.is_finite()) => s.to_string(),
        Lit::Bool if l.bool_words => s.to_uppercase(),
        Lit::Bool => if s == "true" { "1".into() } else { "0".into() },
        Lit::Binary => l.binary_literal.replace("{}", s),
        // Oracle doesn't read text as a date without NLS settings: typed literals.
        Lit::Date if l.typed_datetime_literals => format!("DATE {}", quote_literal(s)),
        Lit::Timestamp if l.typed_datetime_literals => format!("TIMESTAMP {}", quote_literal(s)),
        _ => match l.text_literal {
            TextLiteral::NPrefix => format!("N{}", quote_literal(s)),
            TextLiteral::BackslashEscaped => format!("'{}'", s.replace('\\', "\\\\").replace('\'', "''")),
            TextLiteral::Plain => quote_literal(s),
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
        Lit::Number if v.trim().parse::<f64>().is_err() => Err(format!("{}: “{v}” is not a number", f.name())),
        k => Ok(literal(Some(v.trim_end_matches('\n')).map(|s| if matches!(k, Lit::Number) { s.trim() } else { s }), k, dialect)),
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
            // `INSERT INTO t (cols) VALUES (…), (…)`
            false => (format!("INSERT INTO {t} ({cols}) VALUES ").len(), 2),
        }
    }

    /// The next statement for rows `start..` of `batch` (at most `max_rows`
    /// of them): the SQL and how many rows it inserts. `None` past the end.
    pub fn next(&self, batch: &RecordBatch, start: usize, max_rows: Option<usize>) -> Result<Option<(String, usize)>> {
        if start >= batch.num_rows() {
            return Ok(None);
        }
        let fopts = format_options();
        let fmts: Vec<ArrayFormatter<'_>> = batch.columns().iter().map(|a| ArrayFormatter::try_new(a.as_ref(), &fopts)).collect::<std::result::Result<_, _>>()?;
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
            false => format!("INSERT INTO {t} ({cols}) VALUES {}", rows.join(", ")),
        };
        Ok(Some((sql, n)))
    }
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
        assert_eq!(create_table_sql(ConnectorKind::Postgres, "app.t", &schema), r#"CREATE TABLE "app"."t" ("id" BIGINT NOT NULL, "name" TEXT, "ok" BOOLEAN, "at" TIMESTAMP)"#);
        assert_eq!(create_table_sql(ConnectorKind::Mssql, "t", &schema), "CREATE TABLE [t] ([id] BIGINT NOT NULL, [name] NVARCHAR(MAX), [ok] BIT, [at] DATETIME2)");
        assert_eq!(
            insert_statements(ConnectorKind::Postgres, "t", &schema, &batches).unwrap(),
            vec![r#"INSERT INTO "t" ("id", "name", "ok", "at") VALUES (0, 'it''s 0', TRUE, '2026-01-01 00:00:00'), (1, NULL, FALSE, '2026-01-01 00:00:00')"#]
        );
        let ora = insert_statements(ConnectorKind::Oracle, "t", &schema, &batches).unwrap();
        assert!(ora[0].starts_with(r#"INSERT ALL INTO "t" ("id", "name", "ok", "at") VALUES (0, 'it''s 0', 1, TIMESTAMP '2026-01-01 00:00:00') INTO"#), "{}", ora[0]);
        assert!(ora[0].ends_with("SELECT 1 FROM DUAL"));
        assert!(insert_statements(ConnectorKind::Mssql, "t", &schema, &batches).unwrap()[0].contains("N'it''s 0', 1,"));
        assert!(drop_table_sql(ConnectorKind::Oracle, "t").contains("-942"));
        assert!(create_table_if_missing_sql(ConnectorKind::Sqlite, "t", &schema).starts_with(r#"CREATE TABLE IF NOT EXISTS "t" ("id" INTEGER NOT NULL"#));
        assert!(create_table_if_missing_sql(ConnectorKind::Mssql, "dbo.t", &schema).starts_with("IF OBJECT_ID(N'[dbo].[t]', N'U') IS NULL CREATE TABLE [dbo].[t]"));
        assert!(create_table_if_missing_sql(ConnectorKind::Oracle, "t", &schema).contains("-955"));
    }

    #[test]
    fn batches_rows() {
        let (schema, batches) = rows(1201);
        let s = insert_statements(ConnectorKind::Mysql, "t", &schema, &batches).unwrap();
        assert_eq!(s.len(), 3);
        assert_eq!(s[2].matches("), (").count() + 1, 201);
        assert!(insert_statements(ConnectorKind::Mysql, "t", &schema, &[]).unwrap().is_empty());
        // One statement at a time, capped.
        let b = InsertSql::new(ConnectorKind::Postgres, "t", &schema, batch_limits(ConnectorKind::Postgres).default_size());
        let (sql, n) = b.next(&batches[0], 1199, Some(10)).unwrap().unwrap();
        assert_eq!(n, 2);
        assert!(sql.starts_with(r#"INSERT INTO "t" ("id", "name", "ok", "at") VALUES (1199,"#), "{sql}");
        assert!(b.next(&batches[0], 1201, None).unwrap().is_none());
    }

    #[test]
    fn batch_sizes_per_dialect() {
        use ConnectorKind as K;
        let ms = batch_limits(K::Mssql);
        assert_eq!(ms.resolve(None, None).0, BatchSize { rows: 500, bytes: MB });
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
        for (k, size) in [(K::Postgres, BatchSize { rows: 100, bytes: MB }), (K::Postgres, BatchSize { rows: 10_000, bytes: 4 * KB }), (K::Oracle, BatchSize { rows: 1000, bytes: 4 * KB })] {
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
            update_from_stage_sql(K::Postgres, "app.t", "s", &cols, &keys),
            vec![r#"UPDATE "app"."t" AS tg SET "name" = src."name", "qty" = src."qty" FROM "s" AS src WHERE tg."id" = src."id""#]
        );
        assert_eq!(update_from_stage_sql(K::Mysql, "t", "s", &cols, &keys), vec!["UPDATE `t` AS tg JOIN `s` AS src ON tg.`id` = src.`id` SET tg.`name` = src.`name`, tg.`qty` = src.`qty`"]);
        assert!(update_from_stage_sql(K::Postgres, "t", "s", &keys, &keys).is_empty(), "only keys: nothing to update");

        let pg = merge_from_stage_sql(K::Postgres, "t", "s", &cols, &keys);
        assert_eq!(pg.len(), 2);
        assert_eq!(pg[1], r#"INSERT INTO "t" ("id", "name", "qty") SELECT src."id", src."name", src."qty" FROM "s" AS src WHERE NOT EXISTS (SELECT 1 FROM "t" AS tg WHERE tg."id" = src."id")"#);
        assert_eq!(merge_from_stage_sql(K::Sqlite, "t", "s", &keys, &keys).len(), 1, "only keys: insert the missing ones");

        let ms = merge_from_stage_sql(K::Mssql, "dbo.t", "#s", &cols, &keys);
        assert_eq!(
            ms,
            vec!["MERGE INTO [dbo].[t] AS tg USING [#s] AS src ON tg.[id] = src.[id] WHEN MATCHED THEN UPDATE SET tg.[name] = src.[name], tg.[qty] = src.[qty] WHEN NOT MATCHED THEN INSERT ([id], [name], [qty]) VALUES (src.[id], src.[name], src.[qty]);"]
        );
        let ora = &update_from_stage_sql(K::Oracle, "APP.T", "APP.S", &cols, &keys)[0];
        assert!(ora.starts_with(r#"MERGE INTO "APP"."T" tg USING "APP"."S" src ON (tg."id" = src."id") WHEN MATCHED"#), "{ora}");
        assert!(!ora.contains("NOT MATCHED"));
        let bq = &merge_from_stage_sql(K::Bigquery, "ds.t", "ds.s", &cols, &keys)[0];
        assert!(bq.starts_with("MERGE `ds`.`t` AS tg USING `ds`.`s` AS src ON tg.`id` = src.`id` WHEN MATCHED THEN UPDATE SET `name` = src.`name`"), "{bq}");
    }
}
