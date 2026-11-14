//! DuckDB SQL comparing two outputs. With key columns: rows `added`,
//! `removed` and `changed` (with `<col> (before)` for changed values).
//! Without keys: whole-row differences (multiset).

use std::collections::HashSet;

use databrain_result_store::{ColumnMeta, display};
use serde::{Deserialize, Serialize};

use crate::{EngineError, Result};

use crate::outputs::{OutputInfo, q_min};

/// Pair the columns of two outputs as (before, after) names: user pairs
/// first (`extra`, e.g. `val_dt` ↔ `value_date`), then equal names, then
/// names equal ignoring case (`test` ↔ `Test`). Each column is used once.
pub fn column_pairs(before: &OutputInfo, after: &OutputInfo, extra: &[(String, String)]) -> Result<Vec<(String, String)>> {
    column_pairs_with(before, after, extra, true)
}

/// [`column_pairs`]; with `auto` false only the `extra` pairs are used (the
/// user chose every pair, including leaving some columns out).
pub fn column_pairs_with(before: &OutputInfo, after: &OutputInfo, extra: &[(String, String)], auto: bool) -> Result<Vec<(String, String)>> {
    let bnames: Vec<&str> = before.columns.iter().map(|c| c.name.as_str()).collect();
    let anames: Vec<&str> = after.columns.iter().map(|c| c.name.as_str()).collect();
    let mut used_b: HashSet<String> = HashSet::new();
    let mut used_a: HashSet<String> = HashSet::new();
    let mut pairs: Vec<(String, String)> = Vec::new();
    for (bc, ac) in extra {
        if !bnames.contains(&bc.as_str()) {
            return Err(EngineError::new("invalid", format!("column {bc} is not in {}", before.handle)));
        }
        if !anames.contains(&ac.as_str()) {
            return Err(EngineError::new("invalid", format!("column {ac} is not in {}", after.handle)));
        }
        // A column may be compared with several others (debit = debit,
        // debit = credit); the same pair twice is a mistake.
        if pairs.iter().any(|(b, a)| b == bc && a == ac) {
            return Err(EngineError::new("invalid", format!("{bc} = {ac} is matched twice")));
        }
        used_b.insert(bc.clone());
        used_a.insert(ac.clone());
        pairs.push((bc.clone(), ac.clone()));
    }
    for exact in [true, false].into_iter().filter(|_| auto) {
        for ac in &anames {
            if used_a.contains(*ac) {
                continue;
            }
            let hit = bnames.iter().find(|bc| !used_b.contains(**bc) && if exact { *bc == ac } else { bc.eq_ignore_ascii_case(ac) });
            if let Some(bc) = hit {
                used_b.insert(bc.to_string());
                used_a.insert(ac.to_string());
                pairs.push((bc.to_string(), ac.to_string()));
            }
        }
    }
    // Keep the after output's column order.
    pairs.sort_by_key(|(_, a)| anames.iter().position(|x| x == a).unwrap_or(usize::MAX));
    Ok(pairs)
}

pub fn diff_sql(before: &OutputInfo, after: &OutputInfo, keys: &[String]) -> Result<String> {
    diff_sql_mapped(before, after, keys, &[])
}

/// Diff SQL over matched columns (see [`column_pairs`]). Before-side columns
/// are renamed to the after-side names; `keys` use after-side names (or the
/// before-side name of a pair, or any case).
pub fn diff_sql_mapped(before: &OutputInfo, after: &OutputInfo, keys: &[String], extra: &[(String, String)]) -> Result<String> {
    diff_sql_with(before, after, keys, extra, true)
}

/// [`diff_sql_mapped`]; `auto` false: compare only the given pairs. Values
/// are compared by meaning with the default [`CompareOptions`].
pub fn diff_sql_with(before: &OutputInfo, after: &OutputInfo, keys: &[String], extra: &[(String, String)], auto: bool) -> Result<String> {
    diff_sql_opts(before, after, keys, extra, auto, &CompareOptions::default())
}

/// How the values of a column pair are compared: both sides are turned into
/// one canonical text first, so the database types don't matter
/// (`TIMESTAMP 2026-11-11 00:00:00` = `'2026-11-11'`, `10.50` = `'10.5'`).
/// A value that doesn't parse as the kind is compared as text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum CompareKind {
    /// Date and time; a time of 00:00:00 equals a plain date.
    DateTime,
    /// The date part only (the time is ignored).
    Date,
    /// Numeric value: `10.50` = `10.5` = `'10.5'`; with a tolerance, close values are equal.
    Number,
    /// Text, trimmed (and lower-cased, with "ignore case").
    Text,
    /// `true`/`false`; `1`/`0`, `y`/`n`, `yes`/`no`, `t`/`f` accepted.
    Bool,
    /// The value as the database gave it, as text (no trimming).
    Exact,
}

impl CompareKind {
    pub fn label(self) -> &'static str {
        match self {
            CompareKind::DateTime => "date/time",
            CompareKind::Date => "date only",
            CompareKind::Number => "number",
            CompareKind::Text => "text",
            CompareKind::Bool => "boolean",
            CompareKind::Exact => "exact",
        }
    }
}

/// Options of a compare. The defaults compare by meaning: trimmed text,
/// case matters, empty text is not NULL, numbers exact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
#[serde(default)]
pub struct CompareOptions {
    /// Ignore leading and trailing spaces (CHAR padding).
    pub trim: bool,
    pub ignore_case: bool,
    /// Empty text counts as NULL.
    pub empty_is_null: bool,
    /// Numbers this close are equal (with keys; without keys, numbers are
    /// rounded to steps of it).
    pub tolerance: Option<f64>,
    /// After-side column → kind, overriding the automatic one.
    pub kinds: Vec<(String, CompareKind)>,
}

impl Default for CompareOptions {
    fn default() -> Self {
        Self { trim: true, ignore_case: false, empty_is_null: false, tolerance: None, kinds: vec![] }
    }
}

/// A matched column pair as the compare treats it.
#[derive(Debug, Clone, PartialEq, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct ComparePair {
    pub before: String,
    pub after: String,
    /// The pair's column in the compare result: the after name, or
    /// `before → after` when that after column is in another pair too.
    /// Kinds, keys and changed columns use it.
    pub label: String,
    /// Types as the outputs hold them (Arrow), e.g. `Timestamp(µs)`, `Utf8`.
    pub before_type: String,
    pub after_type: String,
    /// What the types suggest.
    pub auto_kind: CompareKind,
    /// What is used: `auto_kind` unless the options override it.
    pub kind: CompareKind,
}

/// The kind two columns' types suggest: a date on either side compares
/// dates, a number numbers, a boolean booleans; otherwise text.
pub fn auto_kind(before: &ColumnMeta, after: &ColumnMeta) -> CompareKind {
    use display::TypeFamily as F;
    let fam = [before.family, after.family];
    let any = |f: F| fam.contains(&f);
    let other = |f: F| fam.iter().all(|x| *x == f || matches!(x, F::Text | F::Number | F::Bool | F::Date));
    if any(F::Binary) || any(F::Other) {
        CompareKind::Exact
    } else if any(F::Date) && other(F::Date) {
        CompareKind::DateTime
    } else if any(F::Bool) {
        CompareKind::Bool
    } else if any(F::Number) {
        CompareKind::Number
    } else {
        CompareKind::Text
    }
}

/// The pairs a compare uses, with their types and kinds.
pub fn compare_pairs(before: &OutputInfo, after: &OutputInfo, extra: &[(String, String)], auto: bool, opts: &CompareOptions) -> Result<Vec<ComparePair>> {
    let meta = |o: &OutputInfo, n: &str| o.columns.iter().find(|c| c.name == n).cloned();
    let pairs = column_pairs_with(before, after, extra, auto)?;
    let labels = pair_labels(&pairs);
    pairs
        .into_iter()
        .zip(labels)
        .map(|((bc, ac), label)| {
            let (Some(bm), Some(am)) = (meta(before, &bc), meta(after, &ac)) else {
                return Err(EngineError::new("internal", format!("column {bc} or {ac} not found")));
            };
            let auto_kind = auto_kind(&bm, &am);
            let kind = opts.kinds.iter().find(|(c, _)| *c == label).map(|(_, k)| *k).unwrap_or(auto_kind);
            Ok(ComparePair { before: bc, after: ac, label, before_type: bm.data_type, after_type: am.data_type, auto_kind, kind })
        })
        .collect()
}

/// Each pair's result column (see [`ComparePair::label`]). Of the pairs on
/// one after column, the one with the same name (else the first) keeps it.
pub fn pair_labels(pairs: &[(String, String)]) -> Vec<String> {
    let keeper = |a: &str| {
        pairs
            .iter()
            .position(|(b, x)| x == a && b == a)
            .or_else(|| pairs.iter().position(|(b, x)| x == a && b.eq_ignore_ascii_case(a)))
            .or_else(|| pairs.iter().position(|(_, x)| x == a))
    };
    pairs.iter().enumerate().map(|(i, (b, a))| if keeper(a) == Some(i) { a.clone() } else { format!("{b} → {a}") }).collect()
}

/// A tolerance as a SQL literal (positive, finite).
fn tolerance_literal(t: f64) -> Result<String> {
    if !t.is_finite() || t <= 0.0 {
        return Err(EngineError::new("invalid", "the number tolerance must be a positive number"));
    }
    Ok(format!("CAST({t:?} AS DECIMAL(38,10))"))
}

/// DuckDB expression: the canonical text of column `col` (already quoted)
/// whose type family is `fam`, compared as `kind`. NULL stays NULL.
fn canonical(col: &str, fam: display::TypeFamily, zoned: bool, kind: CompareKind, opts: &CompareOptions, round_to: Option<&str>) -> String {
    use display::TypeFamily as F;
    let raw = format!("CAST({col} AS VARCHAR)");
    let text = {
        let mut t = raw.clone();
        if opts.trim {
            t = format!("trim({t})");
        }
        if opts.ignore_case {
            t = format!("lower({t})");
        }
        if opts.empty_is_null {
            t = format!("nullif({t}, '')");
        }
        t
    };
    // Text is trimmed before parsing it as a date or number.
    let parse = |ty: &str, native: bool| if native { format!("try_cast({col} AS {ty})") } else { format!("try_cast(trim({raw}) AS {ty})") };
    // A date/time as a plain timestamp. Time-zone values become their UTC
    // time (as the grid shows them), whatever the session's time zone is:
    // DuckDB's own casts would use the computer's zone. Text with an offset
    // (`…07:00:00+07:00`, `…Z`) is read the same way.
    let utc = |e: String| format!("make_timestamp(epoch_us({e}))");
    let timestamp = || {
        if zoned {
            utc(col.to_string())
        } else if fam == F::Date {
            parse("TIMESTAMP", true)
        } else {
            let t = format!("trim({raw})");
            format!(
                "CASE WHEN regexp_matches({t}, '\\d:\\d{{2}}(:\\d{{2}}(\\.\\d+)?)?\\s*([+-]\\d{{2}}(:?\\d{{2}})?|Z)$') THEN {} ELSE {} END",
                utc(format!("try_cast({t} AS TIMESTAMPTZ)")),
                parse("TIMESTAMP", false)
            )
        }
    };
    match kind {
        CompareKind::Exact => raw,
        CompareKind::Text => text,
        CompareKind::DateTime => {
            let ts = timestamp();
            format!(
                "coalesce(CASE WHEN {ts} = date_trunc('day', {ts}) THEN strftime({ts}, '%Y-%m-%d') ELSE strftime({ts}, '%Y-%m-%d %H:%M:%S.%f') END, {text})"
            )
        }
        CompareKind::Date => {
            let ts = timestamp();
            format!("coalesce(strftime({ts}, '%Y-%m-%d'), {text})")
        }
        CompareKind::Number => {
            let dec = parse("DECIMAL(38,10)", fam == F::Number);
            let dec = match round_to {
                Some(t) => format!("CAST(round({dec} / {t}) * {t} AS DECIMAL(38,10))"),
                None => dec,
            };
            // 10.5000000000 → 10.5, 3.0000000000 → 3; beyond DECIMAL(38,10), as a double.
            let dbl = parse("DOUBLE", fam == F::Number);
            format!("coalesce(regexp_replace(CAST({dec} AS VARCHAR), '(\\.[0-9]*[1-9])0+$|\\.0+$', '\\1'), CAST({dbl} AS VARCHAR), {text})")
        }
        CompareKind::Bool => {
            let l = format!("lower(trim({raw}))");
            format!("CASE WHEN {l} IN ('true', 't', '1', 'y', 'yes') THEN 'true' WHEN {l} IN ('false', 'f', '0', 'n', 'no') THEN 'false' ELSE {text} END")
        }
    }
}

/// What a compare's SQL is built from: the matched pairs, the key pairs, the
/// two sides with their canonical columns (`__cmp_<i>`, see [`canonical`])
/// and the header. Diff and summary use the same plan, so their counts agree.
struct DiffPlan<'o> {
    before: &'o OutputInfo,
    after: &'o OutputInfo,
    pairs: Vec<ComparePair>,
    key_idx: Vec<usize>,
    tol: Option<String>,
    header: String,
    with: String,
}

fn sql_text(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

impl<'o> DiffPlan<'o> {
    fn new(before: &'o OutputInfo, after: &'o OutputInfo, keys: &[String], extra: &[(String, String)], auto: bool, opts: &CompareOptions) -> Result<Self> {
        let q = |s: &str| q_min(s);
        let pairs = compare_pairs(before, after, extra, auto, opts)?;
        if pairs.is_empty() {
            return Err(EngineError::new("invalid", format!("{} and {} have no matching columns; match some columns by hand", before.handle, after.handle)));
        }
        let mut key_idx: Vec<usize> = Vec::new();
        for k in keys {
            let hit = pairs
                .iter()
                .position(|p| p.label == *k)
                .or_else(|| pairs.iter().position(|p| p.after == *k || p.before == *k))
                .or_else(|| pairs.iter().position(|p| p.after.eq_ignore_ascii_case(k) || p.before.eq_ignore_ascii_case(k)));
            match hit {
                Some(i) if !key_idx.contains(&i) => key_idx.push(i),
                Some(_) => {}
                None => return Err(EngineError::new("invalid", format!("key column {k} is not matched in both outputs"))),
            }
        }
        let tol = opts.tolerance.map(tolerance_literal).transpose()?;
        // Without keys a tolerance rounds numbers (rows are compared whole).
        let round_to = if key_idx.is_empty() { tol.as_deref() } else { None };
        let side = |o: &OutputInfo, before_side: bool| -> String {
            let mut cols: Vec<String> = Vec::new();
            for p in &pairs {
                let n = if before_side { &p.before } else { &p.after };
                cols.push(if zoned_of(o, n) {
                    // Shown in UTC, as the grid shows it (not in the computer's zone).
                    format!("make_timestamp(epoch_us({})) AS {}", q(n), q(&p.label))
                } else if n == &p.label {
                    q(&p.label)
                } else {
                    format!("{} AS {}", q(n), q(&p.label))
                });
            }
            for (i, p) in pairs.iter().enumerate() {
                let n = if before_side { &p.before } else { &p.after };
                cols.push(format!("{} AS __cmp_{i}", canonical(&q(n), family_of(o, n), zoned_of(o, n), p.kind, opts, round_to)));
            }
            cols.join(", ")
        };
        let with =
            format!("WITH a AS (SELECT {} FROM {}), b AS (SELECT {} FROM {})", side(before, true), before.reference(), side(after, false), after.reference());

        let mut header = format!("-- Differences from {} (before) to {} (after)\n", before.handle, after.handle);
        let renamed: Vec<String> = pairs.iter().filter(|p| p.before != p.after).map(|p| format!("{} → {}", p.before, p.after)).collect();
        if !renamed.is_empty() {
            header.push_str(&format!("-- Matched columns: {}\n", renamed.join(", ")));
        }
        let kinds: Vec<String> = pairs.iter().filter(|p| p.kind != CompareKind::Text).map(|p| format!("{} as {}", p.label, p.kind.label())).collect();
        if !kinds.is_empty() {
            header.push_str(&format!("-- Compared as: {}\n", kinds.join(", ")));
        }
        let mut flags = Vec::new();
        if !opts.trim {
            flags.push("spaces count".to_string());
        }
        if opts.ignore_case {
            flags.push("case ignored".to_string());
        }
        if opts.empty_is_null {
            flags.push("empty text = NULL".to_string());
        }
        if let Some(t) = opts.tolerance {
            flags.push(format!("numbers within {t} are equal"));
        }
        if !flags.is_empty() {
            header.push_str(&format!("-- Options: {}\n", flags.join(", ")));
        }
        Ok(Self { before, after, pairs, key_idx, tol, header, with })
    }

    fn keyed(&self) -> bool {
        !self.key_idx.is_empty()
    }

    /// Indexes of the compared (non-key) pairs.
    fn values(&self) -> Vec<usize> {
        (0..self.pairs.len()).filter(|i| !self.key_idx.contains(i)).collect()
    }

    /// `x.__cmp_k = y.__cmp_k AND …` over the keys.
    fn on(&self, x: &str, y: &str) -> String {
        self.key_idx.iter().map(|i| format!("{x}.__cmp_{i} = {y}.__cmp_{i}")).collect::<Vec<_>>().join(" AND ")
    }

    /// Without keys: the whole canonical row and its duplicate number match.
    fn same_row(&self, x: &str, y: &str) -> String {
        let mut v: Vec<String> = (0..self.pairs.len()).map(|i| format!("{x}.__cmp_{i} IS NOT DISTINCT FROM {y}.__cmp_{i}")).collect();
        v.push(format!("{x}.__n = {y}.__n"));
        v.join(" AND ")
    }

    /// Numbered sides for the keyless compare.
    fn numbered(&self) -> String {
        let all = (0..self.pairs.len()).map(|i| format!("__cmp_{i}")).collect::<Vec<_>>().join(", ");
        format!(
            "{},\n\
             an AS (SELECT *, row_number() OVER (PARTITION BY {all}) AS __n FROM a),\n\
             bn AS (SELECT *, row_number() OVER (PARTITION BY {all}) AS __n FROM b)",
            self.with
        )
    }

    /// Pair `i` differs between `b` (after) and `a` (before) of one key.
    fn differs(&self, i: usize) -> String {
        let q = |s: &str| q_min(s);
        let p = &self.pairs[i];
        let base = format!("b.__cmp_{i} IS DISTINCT FROM a.__cmp_{i}");
        match (&self.tol, p.kind) {
            // Close numbers are equal; anything that isn't a number compares as above.
            (Some(t), CompareKind::Number) => {
                let num = |side: &str, fam: display::TypeFamily| {
                    if fam == display::TypeFamily::Number {
                        format!("try_cast({side}.{} AS DECIMAL(38,10))", q(&p.label))
                    } else {
                        format!("try_cast(trim(CAST({side}.{} AS VARCHAR)) AS DECIMAL(38,10))", q(&p.label))
                    }
                };
                let (bn, an) = (num("b", family_of(self.after, &p.after)), num("a", family_of(self.before, &p.before)));
                format!("({base} AND NOT coalesce(abs({bn} - {an}) <= {t}, false))")
            }
            _ => base,
        }
    }

    fn shown(&self, t: &str) -> String {
        self.pairs.iter().map(|p| format!("{t}.{}", q_min(&p.label))).collect::<Vec<_>>().join(", ")
    }

    fn diff_sql(&self) -> String {
        let q = |s: &str| q_min(s);
        let header = &self.header;
        if !self.keyed() {
            return format!(
                "{header}{}\n\
                 SELECT 'removed' AS _change, {} FROM an ANTI JOIN bn ON {}\n\
                 UNION ALL BY NAME\n\
                 SELECT 'added' AS _change, {} FROM bn ANTI JOIN an ON {}\n\
                 ORDER BY ALL;",
                self.numbered(),
                self.shown("an"),
                self.same_row("an", "bn"),
                self.shown("bn"),
                self.same_row("bn", "an"),
            );
        }
        let key_list = self.key_idx.iter().map(|i| q(&self.pairs[*i].label)).collect::<Vec<_>>().join(", ");
        let values = self.values();
        let mut changed_cols: Vec<String> = self.key_idx.iter().map(|i| format!("b.{}", q(&self.pairs[*i].label))).collect();
        for i in &values {
            let n = &self.pairs[*i].label;
            changed_cols.push(format!("b.{0} AS {0}, a.{0} AS {1}", q(n), q(&format!("{n} (before)"))));
        }
        // Which columns changed in the row (names as the after output has them).
        if !values.is_empty() {
            let names =
                values.iter().map(|i| format!("CASE WHEN {} THEN {} END", self.differs(*i), sql_text(&self.pairs[*i].label))).collect::<Vec<_>>().join(", ");
            changed_cols.push(format!("concat_ws(', ', {names}) AS _changed"));
        }
        let changed_where = if values.is_empty() { "false".to_string() } else { values.iter().map(|i| self.differs(*i)).collect::<Vec<_>>().join("\n   OR ") };
        format!(
            "{header}{}\n\
             SELECT 'added' AS _change, {} FROM b ANTI JOIN a ON {}\n\
             UNION ALL BY NAME\n\
             SELECT 'removed' AS _change, {} FROM a ANTI JOIN b ON {}\n\
             UNION ALL BY NAME\n\
             SELECT 'changed' AS _change, {}\n\
             FROM b JOIN a ON {}\n\
             WHERE {changed_where}\n\
             ORDER BY _change, {key_list};",
            self.with,
            self.shown("b"),
            self.on("b", "a"),
            self.shown("a"),
            self.on("a", "b"),
            changed_cols.join(", "),
            self.on("b", "a"),
        )
    }

    /// One row: [`SUMMARY_COLUMNS`], then the changed count of each compared column ([`Self::values`]).
    fn summary_sql(&self) -> String {
        let header = self.header.replacen("-- Differences from", "-- Summary of the differences from", 1);
        if !self.keyed() {
            return format!(
                "{header}{}\n\
                 SELECT (SELECT count(*) FROM a) AS before_rows, (SELECT count(*) FROM b) AS after_rows,\n\
                 (SELECT count(*) FROM an SEMI JOIN bn ON {}) AS same, 0 AS changed,\n\
                 (SELECT count(*) FROM bn ANTI JOIN an ON {}) AS added,\n\
                 (SELECT count(*) FROM an ANTI JOIN bn ON {}) AS removed,\n\
                 0 AS before_duplicate_keys, 0 AS after_duplicate_keys;",
                self.numbered(),
                self.same_row("an", "bn"),
                self.same_row("bn", "an"),
                self.same_row("an", "bn"),
            );
        }
        let values = self.values();
        let flags = values.iter().map(|i| format!("({}) AS __d_{i}", self.differs(*i))).collect::<Vec<_>>();
        let any = if values.is_empty() { "false".to_string() } else { values.iter().map(|i| format!("__d_{i}")).collect::<Vec<_>>().join(" OR ") };
        let keys = self.key_idx.iter().map(|i| format!("__cmp_{i}")).collect::<Vec<_>>().join(", ");
        let per_col = values
            .iter()
            .map(|i| format!(",\n(SELECT count(*) FROM j WHERE __d_{i}) AS {}", q_min(&format!("col:{}", self.pairs[*i].label))))
            .collect::<String>();
        format!(
            "{header}{},\n\
             j AS (SELECT true AS __m{} FROM b JOIN a ON {})\n\
             SELECT (SELECT count(*) FROM a) AS before_rows, (SELECT count(*) FROM b) AS after_rows,\n\
             (SELECT count(*) FROM j WHERE NOT ({any})) AS same,\n\
             (SELECT count(*) FROM j WHERE {any}) AS changed,\n\
             (SELECT count(*) FROM b ANTI JOIN a ON {}) AS added,\n\
             (SELECT count(*) FROM a ANTI JOIN b ON {}) AS removed,\n\
             (SELECT count(*) FROM (SELECT 1 FROM a GROUP BY {keys} HAVING count(*) > 1)) AS before_duplicate_keys,\n\
             (SELECT count(*) FROM (SELECT 1 FROM b GROUP BY {keys} HAVING count(*) > 1)) AS after_duplicate_keys{per_col};",
            self.with,
            flags.iter().map(|f| format!(", {f}")).collect::<String>(),
            self.on("b", "a"),
            self.on("b", "a"),
            self.on("a", "b"),
        )
    }
}

/// A timestamp with a time zone (`Timestamp(µs, "UTC")`): an instant, stored in UTC.
fn zoned_of(o: &OutputInfo, n: &str) -> bool {
    o.columns.iter().find(|c| c.name == n).is_some_and(|c| is_zoned(&c.data_type))
}

fn is_zoned(data_type: &str) -> bool {
    data_type.starts_with("Timestamp(") && (data_type.contains("Some(") || (data_type.contains(',') && !data_type.contains("None")))
}

fn family_of(o: &OutputInfo, n: &str) -> display::TypeFamily {
    o.columns.iter().find(|c| c.name == n).map(|c| c.family).unwrap_or(display::TypeFamily::Text)
}

/// [`diff_sql_with`] with compare options. Each matched column gets a hidden
/// canonical column on both sides; keys join on them and changes are found
/// on them, while the result shows the values as each output holds them.
///
/// With keys: rows `added`, `removed` and `changed` (with `<col> (before)`
/// and `_changed`, the columns that differ). Without keys: rows that differ
/// as a multiset (duplicates count), as `removed` and `added`.
pub fn diff_sql_opts(
    before: &OutputInfo,
    after: &OutputInfo,
    keys: &[String],
    extra: &[(String, String)],
    auto: bool,
    opts: &CompareOptions,
) -> Result<String> {
    Ok(DiffPlan::new(before, after, keys, extra, auto, opts)?.diff_sql())
}

/// The first columns of [`compare_summary_sql`]'s one row.
pub const SUMMARY_COLUMNS: [&str; 8] = ["before_rows", "after_rows", "same", "changed", "added", "removed", "before_duplicate_keys", "after_duplicate_keys"];

/// SQL for the counts of a compare (one row: [`SUMMARY_COLUMNS`], then one
/// count per compared column, named `col:<column>`), and those columns'
/// names. Same plan as [`diff_sql_opts`], so the counts match its rows.
pub fn compare_summary_sql(
    before: &OutputInfo,
    after: &OutputInfo,
    keys: &[String],
    extra: &[(String, String)],
    auto: bool,
    opts: &CompareOptions,
) -> Result<(String, Vec<String>)> {
    let plan = DiffPlan::new(before, after, keys, extra, auto, opts)?;
    let cols = if plan.keyed() { plan.values().iter().map(|i| plan.pairs[*i].label.clone()).collect() } else { vec![] };
    Ok((plan.summary_sql(), cols))
}

/// The key columns as the diff names them (after-side names).
pub fn compare_keys(
    before: &OutputInfo,
    after: &OutputInfo,
    keys: &[String],
    extra: &[(String, String)],
    auto: bool,
    opts: &CompareOptions,
) -> Result<Vec<String>> {
    let plan = DiffPlan::new(before, after, keys, extra, auto, opts)?;
    Ok(plan.key_idx.iter().map(|i| plan.pairs[*i].label.clone()).collect())
}

/// Columns that could be keys, as after-side names: id-like names first
/// (`id`, `*_id`, `key`, `code`, `*_no`, `*number`), then the rest; at most 8.
pub fn key_candidates(pairs: &[(String, String)]) -> Vec<String> {
    let score = |n: &str| -> usize {
        let l = n.to_ascii_lowercase();
        if l == "id" {
            0
        } else if l.ends_with("_id") || (n.len() > 2 && n.ends_with("Id")) {
            1
        } else if matches!(l.as_str(), "key" | "code" | "uuid" | "guid")
            || l.ends_with("_key")
            || l.ends_with("_code")
            || l.ends_with("_no")
            || l.ends_with("number")
        {
            2
        } else {
            3
        }
    };
    let mut v: Vec<(usize, usize, String)> = pairs.iter().enumerate().map(|(i, (_, a))| (score(a), i, a.clone())).collect();
    v.sort();
    v.into_iter().take(8).map(|(_, _, n)| n).collect()
}

/// One row: for each candidate, then each pair of the first four, `true`
/// when it is unique and not NULL in both outputs.
pub fn unique_check_sql(before: &OutputInfo, after: &OutputInfo, pairs: &[(String, String)], cands: &[String]) -> String {
    let q = |s: &str| q_min(s);
    let before_name = |a: &str| pairs.iter().find(|(_, x)| x == a).map(|(b, _)| b.clone()).unwrap_or_else(|| a.to_string());
    let test = |cols: &[&String]| -> String {
        let side = |names: Vec<String>, r: &str| {
            let list = names.iter().map(|n| q(n)).collect::<Vec<_>>().join(", ");
            let not_null = names.iter().map(|n| format!("{} IS NOT NULL", q(n))).collect::<Vec<_>>().join(" AND ");
            format!("(SELECT count(*) = count(DISTINCT ({list})) AND bool_and({not_null}) FROM {r})")
        };
        format!(
            "coalesce({} AND {}, false)",
            side(cols.iter().map(|c| before_name(c)).collect(), &before.reference()),
            side(cols.iter().map(|c| c.to_string()).collect(), &after.reference())
        )
    };
    let mut exprs: Vec<String> = cands.iter().map(|c| test(&[c])).collect();
    let head: Vec<&String> = cands.iter().take(4).collect();
    for i in 0..head.len() {
        for j in i + 1..head.len() {
            exprs.push(test(&[head[i], head[j]]));
        }
    }
    format!("SELECT {}", exprs.join(",\n  "))
}

/// The key [`unique_check_sql`]'s row says to use (`true`/`false` cells).
pub fn pick_key(cands: &[String], row: &[Option<String>]) -> Vec<String> {
    let yes = |i: usize| row.get(i).and_then(|v| v.as_deref()).is_some_and(|v| v.eq_ignore_ascii_case("true"));
    if let Some(i) = (0..cands.len()).find(|i| yes(*i)) {
        return vec![cands[i].clone()];
    }
    let head = cands.len().min(4);
    let mut k = cands.len();
    for i in 0..head {
        for j in i + 1..head {
            if yes(k) {
                return vec![cands[i].clone(), cands[j].clone()];
            }
            k += 1;
        }
    }
    vec![]
}

/// Counts of a compare.
#[derive(Debug, Clone, Default, PartialEq, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct CompareSummary {
    #[ts(as = "f64")]
    pub before_rows: i64,
    #[ts(as = "f64")]
    pub after_rows: i64,
    /// Rows equal on both sides (by key, or whole rows without keys).
    #[ts(as = "f64")]
    pub same: i64,
    #[ts(as = "f64")]
    pub changed: i64,
    /// Only in the after output.
    #[ts(as = "f64")]
    pub added: i64,
    /// Only in the before output.
    #[ts(as = "f64")]
    pub removed: i64,
    /// Key values found more than once on that side (they make "changed" unreliable).
    #[ts(as = "f64")]
    pub before_duplicate_keys: i64,
    #[ts(as = "f64")]
    pub after_duplicate_keys: i64,
    /// Changed rows per compared column, in column order.
    pub columns: Vec<ColumnChanges>,
}

#[derive(Debug, Clone, PartialEq, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct ColumnChanges {
    pub column: String,
    #[ts(as = "f64")]
    pub changed: i64,
}

impl CompareSummary {
    /// From the summary query's one row (as display strings).
    pub fn from_row(row: &[Option<String>], columns: &[String]) -> Result<Self> {
        let n = |i: usize| -> Result<i64> {
            row.get(i)
                .and_then(|v| v.as_deref())
                .and_then(|v| v.trim().parse::<i64>().ok())
                .ok_or_else(|| EngineError::new("internal", format!("the compare summary has no count in column {}", i + 1)))
        };
        Ok(Self {
            before_rows: n(0)?,
            after_rows: n(1)?,
            same: n(2)?,
            changed: n(3)?,
            added: n(4)?,
            removed: n(5)?,
            before_duplicate_keys: n(6)?,
            after_duplicate_keys: n(7)?,
            columns: columns
                .iter()
                .enumerate()
                .map(|(i, c)| Ok(ColumnChanges { column: c.clone(), changed: n(SUMMARY_COLUMNS.len() + i)? }))
                .collect::<Result<_>>()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outputs::OutputRegistry;
    use crate::outputs::registry::tests::{add, reg};

    #[test]
    fn several_user_column_pairs_and_exact_mapping() {
        let r = reg();
        let mut b = add(&r, "t1", 1);
        let mut a = add(&r, "t2", 1);
        let col = |o: &OutputInfo, n: &str| databrain_result_store::ColumnMeta { name: n.into(), ..o.columns[0].clone() };
        b.columns = ["id", "a", "b", "c"].iter().map(|n| col(&b, n)).collect();
        a.columns = ["id", "a1", "b2", "c"].iter().map(|n| col(&a, n)).collect();
        let m = vec![("a".to_string(), "a1".to_string()), ("b".to_string(), "b2".to_string())];
        let p = column_pairs(&b, &a, &m).unwrap();
        let s = |v: &[(String, String)]| v.iter().map(|(x, y)| format!("{x}>{y}")).collect::<Vec<_>>();
        assert_eq!(s(&p), ["id>id", "a>a1", "b>b2", "c>c"]);
        // Exact: only the given pairs (c and id left out).
        assert_eq!(s(&column_pairs_with(&b, &a, &m, false).unwrap()), ["a>a1", "b>b2"]);
        // An auto match can be overridden: b.c ↔ a.a1, and a.c not compared.
        let o = vec![("id".to_string(), "id".to_string()), ("c".to_string(), "a1".to_string())];
        assert_eq!(s(&column_pairs_with(&b, &a, &o, false).unwrap()), ["id>id", "c>a1"]);
        let sql = diff_sql_with(&b, &a, &["id".into()], &m, true).unwrap();
        assert!(sql.contains("-- Matched columns: a → a1, b → b2"), "{sql}");
        assert!(sql.contains("\"a\" AS a1") || sql.contains("a AS a1"), "{sql}");
    }

    /// Oracle-like before (typed) vs text after.
    fn typed_pair(r: &OutputRegistry) -> (OutputInfo, OutputInfo) {
        use databrain_result_store::display::TypeFamily as F;
        let mut b = add(r, "t1", 1);
        let mut a = add(r, "t2", 1);
        let col = |n: &str, t: &str, f: F| ColumnMeta { name: n.into(), data_type: t.into(), db_type: None, family: f };
        b.columns = vec![
            col("id", "Int32", F::Number),
            col("d", "Timestamp(µs)", F::Date),
            col("amt", "Decimal128(10, 2)", F::Number),
            col("code", "Utf8", F::Text),
            col("ok", "Boolean", F::Bool),
            col("raw", "Binary", F::Binary),
        ];
        a.columns = ["id", "d", "amt", "code", "ok", "raw"].iter().map(|n| col(n, "Utf8", F::Text)).collect();
        (b, a)
    }

    #[test]
    fn compare_kinds_follow_either_sides_type() {
        let r = reg();
        let (b, a) = typed_pair(&r);
        let p = compare_pairs(&b, &a, &[], true, &CompareOptions::default()).unwrap();
        let kinds: Vec<(&str, CompareKind)> = p.iter().map(|x| (x.after.as_str(), x.kind)).collect();
        assert_eq!(
            kinds,
            [
                ("id", CompareKind::Number),
                ("d", CompareKind::DateTime),
                ("amt", CompareKind::Number),
                ("code", CompareKind::Text),
                ("ok", CompareKind::Bool),
                ("raw", CompareKind::Exact)
            ]
        );
        assert_eq!((p[1].before_type.as_str(), p[1].after_type.as_str()), ("Timestamp(µs)", "Utf8"));
        // An override wins; the automatic kind is still reported.
        let o = CompareOptions { kinds: vec![("d".into(), CompareKind::Date)], ..Default::default() };
        let d = &compare_pairs(&b, &a, &[], true, &o).unwrap()[1];
        assert_eq!((d.kind, d.auto_kind), (CompareKind::Date, CompareKind::DateTime));
    }

    #[test]
    fn a_column_can_be_compared_with_several_others() {
        let r = reg();
        let (b, a) = typed_pair(&r);
        let m = |v: &[(&str, &str)]| v.iter().map(|(x, y)| (x.to_string(), y.to_string())).collect::<Vec<_>>();
        // amt = amt and code = code stay; amt = code is added, not swapped in.
        let map = m(&[("id", "id"), ("amt", "amt"), ("code", "code"), ("amt", "code")]);
        let p = compare_pairs(&b, &a, &map, false, &CompareOptions::default()).unwrap();
        let labels: Vec<&str> = p.iter().map(|x| x.label.as_str()).collect();
        assert_eq!(labels, ["id", "amt", "code", "amt → code"]);
        let sql = diff_sql_opts(&b, &a, &["id".into()], &map, false, &CompareOptions::default()).unwrap();
        assert!(sql.contains("amt AS \"amt → code\"") && sql.contains("code AS \"amt → code\""), "{sql}");
        assert!(sql.contains("a.\"amt → code\" AS \"amt → code (before)\""), "{sql}");
        assert!(sql.contains("THEN 'amt → code'"), "{sql}");
        // The same pair twice is refused.
        let err = compare_pairs(&b, &a, &m(&[("amt", "code"), ("amt", "code")]), false, &CompareOptions::default()).unwrap_err();
        assert!(err.to_string().contains("matched twice"), "{err}");
        // A kind is set per pair, by its label.
        let o = CompareOptions { kinds: vec![("amt → code".into(), CompareKind::Text)], ..Default::default() };
        let p = compare_pairs(&b, &a, &map, false, &o).unwrap();
        assert_eq!((p[2].kind, p[3].kind), (CompareKind::Text, CompareKind::Text));
        assert_eq!(p[1].kind, CompareKind::Number);
    }

    #[test]
    fn time_zone_values_compare_and_show_in_utc() {
        use databrain_result_store::display::TypeFamily as F;
        assert!(is_zoned(r#"Timestamp(µs, "UTC")"#) && is_zoned(r#"Timestamp(Microsecond, Some("UTC"))"#));
        assert!(!is_zoned("Timestamp(µs)") && !is_zoned("Timestamp(Microsecond, None)") && !is_zoned("Utf8"));
        let r = reg();
        let (b, mut a) = typed_pair(&r);
        a.columns[1] = ColumnMeta { name: "d".into(), data_type: r#"Timestamp(µs, "UTC")"#.into(), db_type: None, family: F::Date };
        let sql = diff_sql_with(&b, &a, &["id".into()], &[], true).unwrap();
        // Not DuckDB's casts, which use the computer's time zone.
        assert!(sql.contains("make_timestamp(epoch_us(d)) AS d"), "{sql}");
        assert!(sql.contains("strftime(make_timestamp(epoch_us(d)), '%Y-%m-%d')"), "{sql}");
        // Text with an offset is read as an instant too.
        let (b, a) = typed_pair(&r);
        let sql = diff_sql_with(&a, &b, &["id".into()], &[], true).unwrap();
        assert!(sql.contains("THEN make_timestamp(epoch_us(try_cast(trim(CAST(d AS VARCHAR)) AS TIMESTAMPTZ)))"), "{sql}");
    }

    #[test]
    fn compare_sql_joins_and_compares_canonical_values_but_shows_the_originals() {
        let r = reg();
        let (b, a) = typed_pair(&r);
        let sql = diff_sql_with(&b, &a, &["id".into()], &[], true).unwrap();
        assert!(sql.contains("-- Compared as: id as number, d as date/time, amt as number, ok as boolean, raw as exact"), "{sql}");
        // Keys join on the canonical value, changes compare canonical values.
        assert!(sql.contains("ANTI JOIN a ON b.__cmp_0 = a.__cmp_0"), "{sql}");
        assert!(sql.contains("b.__cmp_1 IS DISTINCT FROM a.__cmp_1"), "{sql}");
        // A typed date parses natively, text is trimmed first; padding is trimmed.
        assert!(sql.contains("try_cast(d AS TIMESTAMP)") && sql.contains("try_cast(trim(CAST(d AS VARCHAR)) AS TIMESTAMP)"), "{sql}");
        assert!(sql.contains("trim(CAST(code AS VARCHAR)) AS __cmp_3"), "{sql}");
        // Shown columns are the outputs' own, never the canonical helpers.
        assert!(sql.contains("SELECT 'added' AS _change, b.id, b.d, b.amt, b.code, b.ok, b.raw FROM b"), "{sql}");
        assert!(sql.contains("a.d AS \"d (before)\""), "{sql}");
        // Without keys: a numbered multiset compare on the canonical values.
        let nokey = diff_sql_with(&b, &a, &[], &[], true).unwrap();
        assert!(nokey.contains("row_number() OVER (PARTITION BY __cmp_0, __cmp_1, __cmp_2, __cmp_3, __cmp_4, __cmp_5)"), "{nokey}");
        assert!(nokey.contains("an.__cmp_1 IS NOT DISTINCT FROM bn.__cmp_1") && nokey.contains("an.__n = bn.__n"), "{nokey}");
        assert!(!nokey.contains("EXCEPT"), "{nokey}");
    }

    #[test]
    fn compare_options_change_the_sql_and_say_so() {
        let r = reg();
        let (b, a) = typed_pair(&r);
        let o = CompareOptions { trim: false, ignore_case: true, empty_is_null: true, tolerance: Some(0.01), kinds: vec![] };
        let sql = diff_sql_opts(&b, &a, &["id".into()], &[], true, &o).unwrap();
        assert!(sql.contains("-- Options: spaces count, case ignored, empty text = NULL, numbers within 0.01 are equal"), "{sql}");
        assert!(sql.contains("nullif(lower(CAST(code AS VARCHAR)), '')"), "{sql}");
        assert!(sql.contains("abs(") && sql.contains("<= CAST(0.01 AS DECIMAL(38,10))"), "{sql}");
        // Without keys a tolerance rounds instead.
        let nokey = diff_sql_opts(&b, &a, &[], &[], true, &o).unwrap();
        assert!(nokey.contains("round(") && !nokey.contains("abs("), "{nokey}");
        for bad in [0.0, -1.0, f64::NAN] {
            let o = CompareOptions { tolerance: Some(bad), ..Default::default() };
            assert!(diff_sql_opts(&b, &a, &["id".into()], &[], true, &o).is_err(), "{bad}");
        }
        // Exact compares the values as given (no trimming).
        let o = CompareOptions { kinds: vec![("code".into(), CompareKind::Exact)], ..Default::default() };
        assert!(diff_sql_opts(&b, &a, &["id".into()], &[], true, &o).unwrap().contains("CAST(code AS VARCHAR) AS __cmp_3"));
    }

    #[test]
    fn key_candidates_put_id_like_names_first_and_the_check_picks_the_first_unique() {
        let p = |v: &[&str]| v.iter().map(|n| (n.to_string(), n.to_string())).collect::<Vec<_>>();
        let c = key_candidates(&p(&["name", "amount", "customerId", "order_no", "id", "region_code"]));
        assert_eq!(c, ["id", "customerId", "order_no", "region_code", "name", "amount"]);
        assert_eq!(key_candidates(&p(&["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"])).len(), 8);
        let cands: Vec<String> = ["id", "order_no", "line"].iter().map(|s| s.to_string()).collect();
        let t = |v: &[&str]| v.iter().map(|x| Some(x.to_string())).collect::<Vec<_>>();
        // Singles first (id is not unique, order_no is).
        assert_eq!(pick_key(&cands, &t(&["false", "true", "false", "true", "true", "true"])), ["order_no"]);
        // No single: the first unique pair (id+order_no, id+line, order_no+line).
        assert_eq!(pick_key(&cands, &t(&["false", "false", "false", "false", "true", "true"])), ["id", "line"]);
        assert!(pick_key(&cands, &t(&["false"; 6])).is_empty());
    }

    #[test]
    fn unique_check_reads_each_side_by_its_own_column_names() {
        let r = reg();
        let (b, a) = typed_pair(&r);
        let pairs = vec![("id".to_string(), "ID".to_string()), ("code".to_string(), "code".to_string())];
        let sql = unique_check_sql(&b, &a, &pairs, &["ID".to_string(), "code".to_string()]);
        assert!(sql.contains("count(DISTINCT (id))") && sql.contains("count(DISTINCT (\"ID\"))"), "{sql}");
        assert!(sql.contains("count(DISTINCT (id, code))"), "{sql}");
    }
}
