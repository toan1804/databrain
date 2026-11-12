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
//! - A column's pattern (`18..90`, `%@gmail.com`, `INV-####`; see
//!   [`PATTERN_HELP`]) shapes its values: the user's, the AI's suggestion,
//!   or a default from the column's name (`age`, `price`, `birth_date`).
//!   Ranges must fit the column's type (`numeric(5,2)` holds up to 999.99,
//!   MySQL TIMESTAMP 1970–2038). Keys, foreign keys and columns the
//!   database fills take none.
//! - "Generate with AI" ([`QueryEngine::test_data_ai_ask`], then
//!   [`QueryEngine::generate_test_data_with`]): an AI model writes a few
//!   hundred realistic rows for the text columns (or every column it can),
//!   from the table's structure only. Each value is checked against its
//!   column (type, length, allowed values, NOT NULL, uniqueness); one that
//!   doesn't fit is made up as usual. Keys and foreign keys never come
//!   from the AI.

use std::collections::{HashMap, HashSet};

use databrain_connector_core::arrow::array::RecordBatch;
use databrain_connector_core::traits::RowLimit;
use databrain_connector_core::value::{MICROS_PER_DAY, days_from_civil, parse_date, parse_datetime, parse_time};
use databrain_connector_core::{
    BatchBuilder, ColType, Column, ColumnInfo, ConnectorKind, ForeignKey, ObjectKind, Value, query_text, quote_ident, quote_object,
};
use databrain_export::load as sqlgen;
use serde::{Deserialize, Serialize};

use crate::{EngineError, LoadRows, QueryEngine, Result};

#[path = "testdata_pattern.rs"]
mod pattern;
pub use pattern::PATTERN_HELP;
use pattern::{Limits, Pattern};

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
    /// Computed by the database (an expression, `rowversion`): an INSERT
    /// naming it fails, so it can't be filled at all.
    pub computed: bool,
    /// What goes in, in words ("emails", "numbers counting up").
    pub generator: String,
    /// Free text (names, emails, descriptions): what "Generate with AI"
    /// writes when it writes only text.
    pub ai_text: bool,
    /// The AI may write it when it writes every column it can: not keys,
    /// foreign keys, UUIDs or bytes.
    pub ai_ok: bool,
    /// The pattern used unless the spec gives one: from the column's name
    /// (`age` → `18..90`).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub pattern: Option<String>,
    /// An example pattern for its type; `None`: no pattern (keys, foreign
    /// keys, types without one).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub pattern_hint: Option<String>,
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
    /// Patterns by column (see [`PATTERN_HELP`]); a column not named here
    /// takes its default ([`TestDataColumn::pattern`]), and `""` none.
    #[serde(default)]
    pub patterns: HashMap<String, String>,
}

/// Generated rows plus what the user should know about them.
pub struct TestData {
    pub rows: LoadRows,
    pub seed: u64,
    pub notes: Vec<String>,
    /// The table, quoted.
    pub table: String,
}

/// Most rows the AI writes for one generation; more rows reuse them.
pub const MAX_AI_ROWS: u32 = 500;

/// A column the AI writes, and what its values must respect.
#[derive(Debug, Clone, PartialEq)]
pub struct AiColumn {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub unique: bool,
    pub max_chars: Option<usize>,
    /// Allowed values (enums, `bit(1)`); empty: any.
    pub choices: Vec<String>,
    /// What DataBrain would put in it otherwise ("emails (up to 80 characters)").
    pub hint: String,
    /// The pattern its values must follow ([`PATTERN_HELP`]).
    pub pattern: Option<String>,
}

/// What to ask the AI for: the table's structure, never its rows.
#[derive(Debug, Clone)]
pub struct AiAsk {
    pub schema: String,
    pub name: String,
    /// The table, quoted.
    pub table: String,
    pub dialect: String,
    /// CHECK constraints of the table that name a column the AI writes
    /// (the raw DDL isn't sent: defaults, index filters and comments
    /// would go with it).
    pub checks: Vec<AiCheck>,
    pub columns: Vec<AiColumn>,
    /// The table's other columns and what fills them, without naming
    /// other tables.
    pub others: Vec<String>,
    /// Rows to write.
    pub rows: usize,
}

/// A CHECK constraint, and the (lower-case) columns it names.
#[derive(Debug, Clone, PartialEq)]
pub struct AiCheck {
    pub sql: String,
    pub columns: Vec<String>,
}

/// Rows the AI wrote: `rows[r][c]` is `columns[c]`'s value (`None`: NULL).
#[derive(Debug, Clone, Default)]
pub struct AiSample {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
}

/// A table's plan, generators and the columns a spec fills.
struct Prepared {
    kind: ConnectorKind,
    plan: TestDataPlan,
    gens: Vec<Option<Gen>>,
    /// Indexes into `plan.columns`.
    chosen: Vec<usize>,
    /// Lower-case names of columns with a one-column UNIQUE constraint.
    unique: HashSet<String>,
    /// UNIQUE constraints and indexes of several columns (lower case).
    unique_groups: Vec<Vec<String>>,
    ddl: Option<String>,
}

/// What fills a column, in words for the AI (no other table named).
fn fill_words(c: &TestDataColumn, chosen: bool) -> &'static str {
    match (chosen, c.parent.is_some(), c.primary_key) {
        (false, _, _) => "left to the database",
        (true, true, _) => "existing keys of a parent table",
        (true, false, true) => "the primary key, counting up",
        (true, false, false) => "made up by DataBrain",
    }
}

impl Prepared {
    /// Position in `chosen` of the counting key column: of a primary key of
    /// several columns, the first that isn't a foreign key (`order_id,
    /// line_no` counts `line_no`, so every row's key differs).
    fn key(&self) -> Option<usize> {
        let pk = |own: bool| self.chosen.iter().position(|&i| self.plan.columns[i].primary_key && (!own || self.plan.columns[i].parent.is_none()));
        pk(true).or_else(|| pk(false))
    }

    /// Positions in `chosen` the AI writes: text only, or every column it may.
    fn ai_positions(&self, all_columns: bool) -> Vec<usize> {
        let key = self.key();
        (0..self.chosen.len())
            .filter(|&k| {
                let c = &self.plan.columns[self.chosen[k]];
                Some(k) != key && c.parent.is_none() && if all_columns { c.ai_ok } else { c.ai_text }
            })
            .collect()
    }
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
        self.generate_test_data_with(connection_id, schema, name, spec, None).await
    }

    /// What to ask the AI for to fill `spec`'s text columns (or, with
    /// `all_columns`, every column it may write): `None` when there is none.
    pub async fn test_data_ai_ask(
        &self,
        connection_id: &str,
        schema: &str,
        name: &str,
        spec: &TestDataSpec,
        all_columns: bool,
        ai_rows: u32,
    ) -> Result<Option<AiAsk>> {
        let p = self.prepare(connection_id, schema, name, spec).await?;
        let positions = p.ai_positions(all_columns);
        if positions.is_empty() {
            return Ok(None);
        }
        let columns = positions
            .iter()
            .map(|&k| {
                let i = p.chosen[k];
                let c = &p.plan.columns[i];
                let pattern = match p.gens[i].as_ref() {
                    Some(Gen::Patterned { text, .. }) => Some(text.clone()),
                    _ => None,
                };
                let g = p.gens[i].as_ref().expect("supported").base();
                AiColumn {
                    name: c.name.clone(),
                    data_type: c.data_type.clone(),
                    nullable: c.nullable,
                    unique: p.unique.contains(&c.name.to_lowercase()),
                    max_chars: match g {
                        Gen::Text { max, .. } => *max,
                        _ => None,
                    },
                    choices: match g {
                        Gen::Choice(v) => v.clone(),
                        _ => vec![],
                    },
                    hint: c.generator.clone(),
                    pattern,
                }
            })
            .collect();
        let others = p
            .plan
            .columns
            .iter()
            .filter(|c| !positions.iter().any(|&k| p.plan.columns[p.chosen[k]].name == c.name))
            .map(|c| format!("{} {} ({})", c.name, c.data_type, fill_words(c, p.chosen.iter().any(|&i| p.plan.columns[i].name == c.name))))
            .collect();
        let ai_names: Vec<String> = positions.iter().map(|&k| p.plan.columns[p.chosen[k]].name.to_lowercase()).collect();
        Ok(Some(AiAsk {
            schema: schema.to_string(),
            name: name.to_string(),
            table: p.plan.table.clone(),
            dialect: p.kind.dialect_name().to_string(),
            checks: check_constraints(p.ddl.as_deref(), &p.plan.columns).into_iter().filter(|c| c.columns.iter().any(|n| ai_names.contains(n))).collect(),
            columns,
            others,
            rows: ai_rows.clamp(1, MAX_AI_ROWS).min(spec.rows) as usize,
        }))
    }

    /// The patterns of `suggested` that work for their column (a column that
    /// takes a pattern, and a pattern that parses for its type).
    pub async fn test_data_valid_patterns(
        &self,
        connection_id: &str,
        schema: &str,
        name: &str,
        suggested: HashMap<String, String>,
    ) -> Result<HashMap<String, String>> {
        let kind = self.test_data_kind(connection_id)?;
        let detail = self.describe(connection_id, schema, name).await?;
        let ddl = self.object_ddl(connection_id, schema, name, ObjectKind::Table).await.ok().flatten();
        let (plan, gens) = plan_for(kind, schema, name, &detail.columns, &detail.foreign_keys, ddl.as_deref());
        Ok(suggested
            .into_iter()
            .filter(|(col, p)| {
                plan.columns.iter().zip(&gens).any(|(c, g)| {
                    c.name.eq_ignore_ascii_case(col)
                        && c.pattern_hint.is_some()
                        && g.as_ref().is_some_and(|g| Pattern::parse(g, &type_limits(kind, &c.data_type), p).is_ok())
                })
            })
            .collect())
    }

    async fn prepare(&self, connection_id: &str, schema: &str, name: &str, spec: &TestDataSpec) -> Result<Prepared> {
        if spec.rows == 0 || spec.rows > MAX_TEST_ROWS {
            return Err(EngineError::new("invalid", format!("Choose between 1 and {MAX_TEST_ROWS} rows")));
        }
        if spec.seed.is_some_and(|s| s > MAX_SEED) {
            return Err(EngineError::new("invalid", format!("Choose a seed between 0 and {MAX_SEED}")));
        }
        let kind = self.test_data_kind(connection_id)?;
        let detail = self.describe(connection_id, schema, name).await?;
        let ddl = self.object_ddl(connection_id, schema, name, ObjectKind::Table).await.ok().flatten();
        let (plan, mut gens) = plan_for(kind, schema, name, &detail.columns, &detail.foreign_keys, ddl.as_deref());
        let skip: HashSet<String> = spec.skip.iter().map(|s| s.to_lowercase()).collect();
        let fill: HashSet<String> = spec.fill.iter().map(|s| s.to_lowercase()).collect();
        if let Some(c) = plan.columns.iter().find(|c| c.computed && fill.contains(&c.name.to_lowercase())) {
            return Err(EngineError::new("invalid", format!("{} is computed by the database: it can't be filled. Untick it.", c.name)));
        }
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
        apply_patterns(kind, &plan, &mut gens, &chosen, &spec.patterns)?;
        let mut keys = unique_keys(ddl.as_deref(), &detail.columns);
        // Unique indexes as the server reports them (SQLite and DuckDB keep
        // them out of the table's DDL).
        if let Ok(layout) = self.table_layout(connection_id, schema, name).await {
            for ix in layout.indexes.iter().filter(|ix| ix.unique && !ix.primary) {
                let key: Option<Vec<String>> =
                    ix.columns.iter().map(|c| index_column(c).filter(|c| detail.columns.iter().any(|x| x.name.to_lowercase() == *c))).collect();
                if let Some(key) = key.filter(|k| !k.is_empty() && !keys.contains(k)) {
                    keys.push(key);
                }
            }
        }
        // A primary key of several columns is a unique key of several columns too.
        let pk: Vec<String> = detail.columns.iter().filter(|c| c.is_primary_key).map(|c| c.name.to_lowercase()).collect();
        if pk.len() > 1 && !keys.contains(&pk) {
            keys.push(pk);
        }
        let unique = keys.iter().filter(|k| k.len() == 1).flatten().cloned().collect();
        let unique_groups = keys.into_iter().filter(|k| k.len() > 1).collect();
        Ok(Prepared { kind, plan, gens, chosen, unique, unique_groups, ddl })
    }

    /// Rows for `spec`; with `sample`, the AI's values go into its columns
    /// where they fit.
    pub async fn generate_test_data_with(
        &self,
        connection_id: &str,
        schema: &str,
        name: &str,
        spec: &TestDataSpec,
        sample: Option<&AiSample>,
    ) -> Result<TestData> {
        let prepared = self.prepare(connection_id, schema, name, spec).await?;
        let Prepared { kind, plan, gens, chosen, unique, unique_groups, .. } = &prepared;
        let (kind, plan, gens, chosen) = (*kind, plan, gens, chosen);
        let mut notes = plan.warnings.clone();
        let seed = spec.seed.unwrap_or_else(new_seed);
        let mut rng = Rng(seed);

        // Sources per chosen column: random, a counting key, or parent keys.
        let mut sources: Vec<Source> = chosen.iter().map(|&i| Source::Random(gens[i].clone().expect("supported"))).collect();
        let pos = |col: &str| chosen.iter().position(|&i| plan.columns[i].name.eq_ignore_ascii_case(col));

        // The first primary key column counts up past the current maximum
        // (or row count): unique within the rows and against the table.
        if let Some(k) = prepared.key() {
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
            let start = key_start(&maker, &type_limits(kind, &col.data_type), found.as_deref(), spec.rows, &col.name, &col.data_type)
                .map_err(|m| EngineError::new("invalid", m))?;
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
        // The AI's values, in the columns still made up at random.
        if let Some(sample) = sample.filter(|s| !s.rows.is_empty()) {
            let mut used = Vec::new();
            for (k, &i) in chosen.iter().enumerate() {
                let c = &plan.columns[i];
                let Some(col) = sample.columns.iter().position(|n| n.eq_ignore_ascii_case(&c.name)) else { continue };
                if let Source::Random(g) = &sources[k] {
                    // A column of a primary key of several columns needn't be unique alone.
                    let own_key = c.primary_key && plan.columns.iter().filter(|c| c.primary_key).count() == 1;
                    sources[k] = Source::Sample { col, fallback: g.clone(), unique: own_key || unique.contains(&c.name.to_lowercase()) };
                    used.push(c.name.clone());
                }
            }
            if !used.is_empty() {
                let n = sample.rows.len();
                let reuse = if (spec.rows as usize) > n { format!("; the other {} rows reuse them", spec.rows as usize - n) } else { String::new() };
                notes.push(format!(
                    "AI wrote {n} row{} for {}{reuse}. The seed repeats only the other columns.",
                    if n == 1 { "" } else { "s" },
                    used.join(", ")
                ));
            }
        }

        let nullable: Vec<bool> = chosen.iter().map(|&i| plan.columns[i].nullable && !plan.columns[i].primary_key).collect();
        // Unique keys of several columns: positions in the row, when every
        // column of the key is filled and one of them is made up (random or AI).
        let groups: Vec<Vec<usize>> = unique_groups
            .iter()
            .filter_map(|key| key.iter().map(|c| chosen.iter().position(|&i| plan.columns[i].name.to_lowercase() == *c)).collect::<Option<Vec<usize>>>())
            .filter(|g| g.iter().any(|&k| made_up(&sources[k]).is_some()))
            .collect();
        // A primary key of foreign keys only (`user_id, role_id`): nothing
        // made up can tell repeated pairs apart.
        let pk: Vec<usize> = (0..chosen.len()).filter(|&k| plan.columns[chosen[k]].primary_key).collect();
        if pk.len() > 1 && pk.iter().all(|&k| matches!(sources[k], Source::Parent { .. })) {
            let names: Vec<&str> = pk.iter().map(|&k| plan.columns[chosen[k]].name.as_str()).collect();
            notes.push(format!(
                "The primary key ({}) is all foreign keys: rows may repeat a combination, and the load refuses repeats. Make fewer rows.",
                names.join(", ")
            ));
        }
        let unique_cols: Vec<bool> = chosen.iter().map(|&i| unique.contains(&plan.columns[i].name.to_lowercase())).collect();
        let mut counts = Counts::default();
        let batches = generate(
            &columns,
            &sources,
            &nullable,
            &parents,
            sample,
            &groups,
            &unique_cols,
            spec.rows as usize,
            spec.null_percent.min(100),
            &mut rng,
            &mut counts,
        )?;
        if counts.repeats > 0 {
            notes.push(format!(
                "{} row{} still repeat a unique column or key of several columns ({RETRIES} made-up values in a row were taken): the load will refuse them. Make fewer rows, raise the AI rows, or widen the column's pattern.",
                counts.repeats,
                if counts.repeats == 1 { "" } else { "s" }
            ));
        }
        let misfits = counts.misfits;
        if misfits > 0 {
            notes.push(format!(
                "{misfits} AI value{} didn't fit {} column (type, length, allowed values, NOT NULL or uniqueness) and {} made up instead.",
                if misfits == 1 { "" } else { "s" },
                if misfits == 1 { "its" } else { "their" },
                if misfits == 1 { "was" } else { "were" }
            ));
        }
        let schema = databrain_connector_core::value::schema_for(&columns);
        Ok(TestData { rows: LoadRows { schema, batches }, seed, notes, table: plan.table.clone() })
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

/// The patterns of the columns filled (`chosen`; a column left out keeps
/// whatever was typed for it, valid or not): the spec's, else the default
/// from the column's name. A pattern asked for a column that takes none
/// (key, foreign key, filled by the database) is refused.
fn apply_patterns(kind: ConnectorKind, plan: &TestDataPlan, gens: &mut [Option<Gen>], chosen: &[usize], patterns: &HashMap<String, String>) -> Result<()> {
    for &i in chosen {
        let c = &plan.columns[i];
        let given = patterns.iter().find(|(k, _)| k.eq_ignore_ascii_case(&c.name)).map(|(_, v)| v.trim().to_string());
        let text = match given {
            Some(t) => t,
            // Only a column that takes a pattern has a default.
            None if c.pattern_hint.is_some() => c.pattern.clone().unwrap_or_default(),
            None => String::new(),
        };
        if text.is_empty() {
            continue;
        }
        if c.pattern_hint.is_none() {
            return Err(EngineError::new(
                "invalid",
                format!("{}: keys, foreign keys, columns the database fills and {} columns don't take a pattern", c.name, c.data_type),
            ));
        }
        let Some(base) = gens[i].take() else { continue };
        let pattern = Pattern::parse(&base, &type_limits(kind, &c.data_type), &text)
            .map_err(|e| EngineError::new("invalid", format!("Pattern for {} ({text}): {e}", c.name)))?;
        gens[i] = Some(Gen::Patterned { base: Box::new(base), pattern, text });
    }
    Ok(())
}

/// `SELECT DISTINCT` of a parent's key columns, non-NULL, capped, in key
/// order so the same seed picks the same parents on another run.
fn parent_keys_sql(kind: ConnectorKind, parent: &str, cols: &[&str], n: usize) -> String {
    let list = cols.iter().map(|c| quote_ident(kind, c)).collect::<Vec<_>>().join(", ");
    let filter = cols.iter().map(|c| format!("{} IS NOT NULL", quote_ident(kind, c))).collect::<Vec<_>>().join(" AND ");
    match kind.traits().row_limit {
        RowLimit::Top => format!("SELECT DISTINCT TOP {n} {list} FROM {parent} WHERE {filter} ORDER BY {list}"),
        RowLimit::FetchFirst => format!("SELECT DISTINCT {list} FROM {parent} WHERE {filter} ORDER BY {list} FETCH FIRST {n} ROWS ONLY"),
        RowLimit::Limit => format!("SELECT DISTINCT {list} FROM {parent} WHERE {filter} ORDER BY {list} LIMIT {n}"),
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
    match g.base() {
        Gen::Int { .. } | Gen::Decimal { .. } | Gen::Float | Gen::Date | Gen::Timestamp | Gen::TimestampTz | Gen::Time => KeyRead::Max,
        Gen::Text { .. } | Gen::Binary { .. } | Gen::Inet => KeyRead::Count,
        Gen::Bool | Gen::Choice(_) | Gen::Uuid | Gen::Json => KeyRead::Nothing,
        Gen::Patterned { .. } => unreachable!("base() is never patterned"),
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

/// Microseconds since 1970 (UTC) of a date-time as databases print it,
/// with or without an offset (`2024-05-01 10:00:00+07`, `…+05:30`, `…-0800`,
/// `…Z`, `… UTC`).
fn parse_ts(s: &str) -> Option<i64> {
    let s = s.trim().trim_end_matches('Z');
    let s = s.strip_suffix(" UTC").unwrap_or(s);
    let (local, offset) = split_offset(s);
    parse_datetime(local.trim_end()).map(|m| m - offset * 1_000_000)
}

/// A date-time and its UTC offset in seconds (0 when it has none). The
/// offset is a sign then `hh`, `hh:mm`, `hhmm` or `hh:mm:ss` (Postgres
/// prints old local mean times so: `+07:06:40`) after the time.
fn split_offset(s: &str) -> (&str, i64) {
    let Some(p) = s.get(10..).and_then(|t| t.rfind(['+', '-'])).map(|p| p + 10) else { return (s, 0) };
    let tail = &s[p + 1..];
    if !tail.bytes().all(|b| b.is_ascii_digit() || b == b':') {
        return (s, 0);
    }
    let d: Vec<i64> = tail.bytes().filter(u8::is_ascii_digit).map(|b| i64::from(b - b'0')).collect();
    let two = |i: usize| d[i] * 10 + d[i + 1];
    let (h, m, sec) = match d.len() {
        2 => (two(0), 0, 0),
        4 => (two(0), two(2), 0),
        6 => (two(0), two(2), two(4)),
        _ => return (s, 0),
    };
    if h > 18 || m >= 60 || sec >= 60 {
        return (s, 0);
    }
    let sign = if s.as_bytes()[p] == b'-' { -1 } else { 1 };
    (&s[..p], sign * (h * 3600 + m * 60 + sec))
}

/// A date-time for a column without a time zone: as written, with no
/// offset (or a zero one) only; one with another offset doesn't fit.
fn parse_naive_ts(s: &str) -> Option<i64> {
    let t = s.trim().trim_end_matches('Z');
    let t = t.strip_suffix(" UTC").unwrap_or(t);
    match split_offset(t) {
        (local, 0) => parse_datetime(local.trim_end()),
        _ => None,
    }
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
/// unique keys (`limits`: what its type holds).
fn key_start(g: &Gen, limits: &Limits, found: Option<&str>, rows: u32, col: &str, data_type: &str) -> std::result::Result<i64, String> {
    let g = g.base();
    let rows = i64::from(rows.max(1));
    let full = |what: &str| format!("{col} ({data_type}) has no room for {rows} more unique keys: {what}. Choose fewer rows, or leave {col} to the database.");
    let day0 = days_from_civil(2020, 1, 1);
    let last_day = days_from_civil(9999, 12, 31);
    let after_count = || found.and_then(|c| c.trim().parse::<i64>().ok()).map_or(1, |c| c.saturating_add(1));
    if matches!(g, Gen::Int { .. } | Gen::Decimal { .. })
        && let Some(k) = negative_scale(&unwrapped_type(data_type))
    {
        return Err(format!(
            "{col} ({data_type}) rounds numbers to multiples of 1{}, so counting keys 1, 2, 3… would repeat. Leave {col} to the database (a sequence or trigger).",
            "0".repeat(k as usize)
        ));
    }
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
        Gen::Patterned { .. } => unreachable!("base() is never patterned"),
    };
    let last = start.checked_add(rows - 1).ok_or_else(|| full("the next key is past the largest 64-bit integer"))?;
    let int_max = limits.int_range().map(|(_, hi)| hi);
    // The last key as microseconds, for dates and date-times.
    let last_moment = match g {
        Gen::Date => Some(i128::from(last) * i128::from(MICROS_PER_DAY)),
        Gen::Timestamp | Gen::TimestampTz => Some(i128::from(last) * 1_000_000),
        _ => None,
    };
    let moment_max = limits.moments().map(|(_, hi)| i128::from(hi));
    match g {
        Gen::Int { .. } | Gen::Decimal { .. } if int_max.is_some_and(|m| i128::from(last) > m) => {
            Err(full(&format!("it holds up to {}", int_max.unwrap_or_default())))
        }
        Gen::Date | Gen::Timestamp | Gen::TimestampTz if last_moment.zip(moment_max).is_some_and(|(l, m)| l > m) => {
            Err(full(&format!("it holds {}", limits.span())))
        }
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
    /// `base` values that follow a pattern.
    Patterned {
        base: Box<Gen>,
        pattern: Pattern,
        text: String,
    },
}

/// An example pattern for a generator's type.
fn pattern_hint(g: &Gen) -> Option<String> {
    Some(
        match g {
            Gen::Int { .. } => "18..90",
            Gen::Float | Gen::Decimal { .. } => "1..1000",
            Gen::Date | Gen::Timestamp | Gen::TimestampTz => "1950-01-01..2005-12-31",
            Gen::Time => "08:00..18:00",
            Gen::Text { .. } => "%@gmail.com",
            Gen::Choice(_) => "{a|b}",
            Gen::Bool | Gen::Uuid | Gen::Json | Gen::Inet | Gen::Binary { .. } | Gen::Patterned { .. } => return None,
        }
        .to_string(),
    )
}

impl Gen {
    /// The generator under any pattern.
    fn base(&self) -> &Gen {
        match self {
            Gen::Patterned { base, .. } => base.base(),
            g => g,
        }
    }

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
            Gen::Patterned { base, .. } => base.col_type(),
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
            Gen::Patterned { pattern, text, .. } => pattern.describe(text),
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
            Gen::Patterned { base, .. } => base.key_describe(),
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
        let limits = type_limits(kind, &c.data_type);
        let fills = database_fills(kind, c, ddl);
        let computed = fills && computed(kind, c, ddl);
        let parent = parent_of(&c.name);
        let note = match (&maker, fills) {
            (None, _) => Some(if !c.nullable && c.default.is_none() && !fills {
                format!("DataBrain can't make up {} values; the column is NOT NULL without a default, so inserts will fail", c.data_type)
            } else {
                format!("DataBrain can't make up {} values: left to the database", c.data_type)
            }),
            (Some(_), true) if computed => Some("Computed by the database: it can't be filled".into()),
            (Some(_), true) => Some("The database fills it (identity, sequence or default): left out unless you turn it on".into()),
            _ => None,
        };
        let hint = match &maker {
            Some(g) if parent.is_none() && !c.is_primary_key && !fills => pattern_hint(g),
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
            computed,
            generator: match (&parent, &maker) {
                (Some(p), _) => format!("existing keys of {p}"),
                (None, Some(g)) if c.is_primary_key => g.key_describe(),
                (None, Some(g)) => g.describe(),
                (None, None) => "left to the database".into(),
            },
            ai_text: parent.is_none() && !c.is_primary_key && matches!(maker, Some(Gen::Text { .. })),
            ai_ok: parent.is_none() && !c.is_primary_key && matches!(&maker, Some(g) if !matches!(g, Gen::Uuid | Gen::Binary { .. })),
            // Keys, foreign keys and columns the database fills (even when
            // ticked) take no pattern, so no default either.
            pattern: match &maker {
                Some(g) if hint.is_some() => pattern::default_pattern(g, &limits, &c.name),
                _ => None,
            },
            pattern_hint: hint,
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
    column_ddl(c, ddl).any(|r| DDL.iter().any(|w| r.contains(w)))
}

/// What follows the column's name on each DDL line that starts with it, in
/// lower case and padded with spaces (`" int identity(1,1) not null, "`).
fn column_ddl<'a>(c: &ColumnInfo, ddl: Option<&'a str>) -> impl Iterator<Item = String> + 'a {
    let name = c.name.to_ascii_lowercase();
    ddl.unwrap_or("").lines().filter_map(move |line| {
        let l = line.trim().to_ascii_lowercase();
        let rest = ["\"", "`", "["]
            .iter()
            .find_map(|q| {
                let close = if *q == "[" { "]" } else { q };
                l.strip_prefix(q).and_then(|r| r.strip_prefix(name.as_str())).and_then(|r| r.strip_prefix(close))
            })
            .or_else(|| l.strip_prefix(name.as_str()).filter(|r| r.starts_with(char::is_whitespace)));
        rest.map(|r| format!(" {r} "))
    })
}

/// A column the database computes from an expression (SQL Server `AS (…)`,
/// `GENERATED ALWAYS AS (…)`, ClickHouse `MATERIALIZED` / `ALIAS`) or writes
/// itself (SQL Server `rowversion`): an INSERT naming it fails, so it can't
/// be filled (identity columns can, where the database allows it).
fn computed(kind: ConnectorKind, c: &ColumnInfo, ddl: Option<&str>) -> bool {
    if kind == ConnectorKind::Mssql && matches!(c.data_type.trim().to_ascii_lowercase().as_str(), "timestamp" | "rowversion") {
        return true;
    }
    column_ddl(c, ddl).any(|r| [" as (", " as(", " materialized ", " alias "].iter().any(|w| r.contains(w)))
}

/// The `k` of a negative scale (Oracle `NUMBER(10,-2)` rounds to 10^k).
fn negative_scale(t: &str) -> Option<u32> {
    let args = t.split_once('(')?.1.split_once(')')?.0;
    args.split(',').nth(1)?.trim().strip_prefix('-')?.trim().parse().ok().filter(|k| *k > 0)
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

/// The type in lower case, without ClickHouse's `Nullable(…)` and
/// `LowCardinality(…)`.
fn unwrapped_type(raw: &str) -> String {
    let mut t = raw.trim().to_ascii_lowercase();
    loop {
        let before = t.clone();
        for w in ["nullable(", "lowcardinality("] {
            if t.starts_with(w) && t.ends_with(')') {
                t = t[w.len()..t.len() - 1].trim().to_string();
            }
        }
        if t == before {
            return t;
        }
    }
}

/// What a database type holds, for patterns and counting keys: integer
/// sizes and signs, decimal precision and scale, and the dates of date and
/// date-time types that hold fewer than years 1–9999.
fn type_limits(kind: ConnectorKind, raw: &str) -> Limits {
    use ConnectorKind::*;
    let t = unwrapped_type(raw);
    let (plain, args) = split_type(&t);
    let unsigned = plain.contains(" unsigned");
    let plain = plain.replace(" unsigned", "").replace(" zerofill", "");
    let (a0, a1) = (args.first().copied().flatten(), args.get(1).copied().flatten());
    let ch = kind == Clickhouse;
    let bits = |b: u32, signed: bool| {
        if b >= 128 {
            Limits::Ints(if signed { i128::MIN } else { 0 }, i128::MAX)
        } else if signed {
            Limits::Ints(-(1i128 << (b - 1)), (1i128 << (b - 1)) - 1)
        } else {
            Limits::Ints(0, (1i128 << b) - 1)
        }
    };
    let int = |b: u32| bits(b, !unsigned);
    // Precision past 38 (ClickHouse Decimal256, BigQuery BIGNUMERIC,
    // Postgres numeric(60,2)) holds more whole numbers than DataBrain counts
    // (i128): no limit is checked there.
    let digits = |p: u32, s: u32| {
        let p = p.clamp(1, 1000);
        let s = s.min(p);
        if s == 0 {
            let m = 10i128.checked_pow(p).map_or(i128::MAX, |n| n - 1);
            Limits::Ints(-m, m)
        } else {
            Limits::Decimal { precision: p, scale: s }
        }
    };
    // First and last day, as moments (the last day's last second).
    let days = |from: (i64, u32, u32), to: (i64, u32, u32)| {
        Limits::Moments(days_from_civil(from.0, from.1, from.2) * MICROS_PER_DAY, (days_from_civil(to.0, to.1, to.2) + 1) * MICROS_PER_DAY - 1_000_000)
    };
    let any_day = days((1, 1, 1), (9999, 12, 31));
    match plain.as_str() {
        // Any integer type is 64-bit in SQLite.
        p if kind == Sqlite && p.contains("int") => bits(64, true),
        "int8" if ch => int(8),
        "int16" if ch => int(16),
        "int32" if ch => int(32),
        "int64" if ch => int(64),
        "int128" | "int256" if ch => int(128),
        "uint8" | "utinyint" => bits(8, false),
        "uint16" | "usmallint" => bits(16, false),
        "uint32" | "uinteger" => bits(32, false),
        "uint64" | "ubigint" => bits(64, false),
        "uint128" | "uint256" | "uhugeint" => bits(128, false),
        // SQL Server's tinyint is 0–255.
        "tinyint" | "int1" => bits(8, !unsigned && kind != Mssql),
        // Oracle's integer types are NUMBER(38).
        "int" | "integer" | "smallint" if kind == Oracle => digits(38, 0),
        "smallint" | "int2" | "smallserial" | "serial2" | "short" => int(16),
        "mediumint" => int(24),
        "int" | "integer" | "int4" | "serial" | "serial4" | "int32" => int(32),
        "bigint" | "int8" | "int64" | "bigserial" | "serial8" => int(64),
        "hugeint" | "int128" => int(128),
        "year" if kind == Mysql => Limits::Ints(1901, 2155),
        // ClickHouse Decimal32(S): the argument is the scale.
        "decimal32" => digits(9, a0.unwrap_or(0)),
        "decimal64" => digits(18, a0.unwrap_or(0)),
        "decimal128" => digits(38, a0.unwrap_or(0)),
        "decimal256" => digits(76, a0.unwrap_or(0)),
        "numeric" | "decimal" | "dec" | "number" | "bignumeric" | "bigdecimal" | "newdecimal" => match a0 {
            // Oracle NUMBER(10,-2): 10 digits, rounded to hundreds.
            Some(p) if let Some(k) = negative_scale(&t) => {
                let m = 10i128.checked_pow(p.clamp(1, 1000)).zip(10i128.checked_pow(k)).and_then(|(a, b)| (a - 1).checked_mul(b)).unwrap_or(i128::MAX);
                Limits::Ints(-m, m)
            }
            Some(p) => digits(p, a1.unwrap_or(0)),
            None => Limits::Any,
        },
        "date" if kind == Mysql => days((1000, 1, 1), (9999, 12, 31)),
        "date" if ch => days((1970, 1, 1), (2149, 6, 6)),
        "date32" | "datetime64" if ch => days((1900, 1, 1), (2299, 12, 31)),
        // 1970-01-01 00:00:01 to 2038-01-19 03:14:07 UTC; a day inside, for
        // any session time zone.
        "timestamp" if kind == Mysql => days((1970, 1, 2), (2038, 1, 18)),
        "datetime" if kind == Mysql => days((1000, 1, 1), (9999, 12, 31)),
        "datetime" if kind == Mssql => days((1753, 1, 1), (9999, 12, 31)),
        "datetime" if ch => days((1970, 1, 2), (2106, 2, 6)),
        // Up to 2079-06-06 23:59: the last whole day before, as seconds round to minutes.
        "smalldatetime" => days((1900, 1, 1), (2079, 6, 5)),
        "date"
        | "timestamp"
        | "timestamptz"
        | "datetime"
        | "datetime2"
        | "datetimeoffset"
        | "timestamp with time zone"
        | "timestamp without time zone"
        | "timestamp with local time zone"
        | "timestamp_tz"
        | "timestamp_ltz"
        | "timestamp_ntz" => any_day,
        _ => Limits::Any,
    }
}

/// The generator for a database type, `None` when DataBrain can't make it up.
fn parse_type(kind: ConnectorKind, raw: &str, column: &str) -> Option<Gen> {
    let t = unwrapped_type(raw);
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
                // ClickHouse Decimal32(S)…Decimal256(S): the argument is the scale.
                (s, _) if plain.starts_with("decimal") && plain.len() > "decimal".len() => (
                    match plain.as_str() {
                        "decimal32" => 9,
                        "decimal64" => 18,
                        "decimal128" => 38,
                        _ => 76,
                    },
                    s.unwrap_or(0),
                ),
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
    /// Column `col` of the AI's rows, checked; `fallback` when it doesn't fit.
    Sample {
        col: usize,
        fallback: Gen,
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
        Gen::Patterned { base, pattern, .. } => {
            let (flavor, max) = match base.base() {
                Gen::Text { max, flavor } => (*flavor, *max),
                _ => (Flavor::Word, None),
            };
            pattern.generate(base, flavor, max, rng)
        }
    }
}

/// Key number `i` (from [`key_start`], counting up) as a value: numbers
/// as they are, dates as days since 1970, date-times and times as
/// seconds, text, bytes and addresses encoded from it.
fn key_value(g: &Gen, i: i64, tag: &str, rng: &mut Rng) -> Value {
    match g.base() {
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
        Gen::Patterned { .. } => unreachable!("base() is never patterned"),
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
        ColType::Timestamp => parse_naive_ts(s).map(Value::Timestamp),
        ColType::TimestampTz => tz(s).map(Value::TimestampTz),
        ColType::Time => parse_time(s).map(Value::Time),
        ColType::Binary => None,
        ColType::Utf8 => Some(Value::Text(s.to_string())),
    };
    v.unwrap_or(Value::Null)
}

/// An AI value as the column's value, or `None` when it doesn't fit (type,
/// range, length, allowed values).
fn sample_value(g: &Gen, raw: &str) -> Option<Value> {
    let s = raw.trim();
    match g {
        // Bytes too: some types count bytes (Oracle VARCHAR2, SQL Server varchar).
        Gen::Text { max, .. } => (!s.is_empty() && max.is_none_or(|m| s.len() <= m)).then(|| Value::Text(s.to_string())),
        Gen::Choice(options) => options.iter().find(|o| o.eq_ignore_ascii_case(s)).map(|o| Value::Text(o.clone())),
        Gen::Int { min, max } => {
            // The generator's range: within the type, the sign and any CHECK it implies.
            let v: i64 = s.parse().ok()?;
            (*min..=*max).contains(&v).then_some(Value::Int(v))
        }
        Gen::Float => s.parse::<f64>().ok().filter(|f| f.is_finite()).map(Value::Float),
        Gen::Decimal { int_digits, scale } => {
            let body = s.strip_prefix('-').unwrap_or(s);
            let (ip, fp) = body.split_once('.').unwrap_or((body, ""));
            let digits = |t: &str| t.bytes().all(|b| b.is_ascii_digit());
            let ip_len = ip.trim_start_matches('0').len();
            (!ip.is_empty() && digits(ip) && digits(fp) && ip_len <= *int_digits as usize && fp.len() <= *scale as usize).then(|| Value::Text(s.to_string()))
        }
        Gen::Json => serde_json::from_str::<serde_json::Value>(s).ok().map(|_| Value::Text(s.to_string())),
        Gen::Inet => s.parse::<std::net::IpAddr>().ok().map(|_| Value::Text(s.to_string())),
        Gen::Date | Gen::Timestamp | Gen::TimestampTz if !real_date(s) => None,
        // Years every engine's types hold (MySQL TIMESTAMP 1970–2038, SQL Server smalldatetime 1900–2079).
        Gen::Date if !s.get(..4).and_then(|y| y.parse::<i32>().ok()).is_some_and(|y| (1901..=2078).contains(&y)) => None,
        Gen::Timestamp | Gen::TimestampTz if !s.get(..4).and_then(|y| y.parse::<i32>().ok()).is_some_and(|y| (1971..=2037).contains(&y)) => None,
        Gen::Bool | Gen::Date | Gen::Timestamp | Gen::TimestampTz | Gen::Time => match cell_value(g.col_type(), s) {
            Value::Null => None,
            v => Some(v),
        },
        Gen::Uuid | Gen::Binary { .. } => None,
        // The pattern decides (a range may go past the generator's own):
        // its ranges and values were checked against what the column's type
        // holds (`type_limits`: MySQL TIMESTAMP 1970–2038…), which stands in
        // for the years every engine holds above. Text also fits the column.
        Gen::Patterned { base, pattern, .. } => match base.base() {
            Gen::Text { .. } => sample_value(base, s).and_then(|_| pattern.accept(base, s)),
            _ => pattern.accept(base, s),
        },
    }
}

/// `YYYY-MM-DD…` names a day of the calendar (the parser rolls 02-30 over).
fn real_date(s: &str) -> bool {
    let part = |r: std::ops::Range<usize>| s.get(r).and_then(|t| t.parse::<i64>().ok());
    let (Some(y), Some(m), Some(d)) = (part(0..4), part(5..7), part(8..10)) else { return false };
    if !(1..=12).contains(&m) || d < 1 {
        return false;
    }
    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    d <= days_from_civil(ny, nm as u32, 1) - days_from_civil(y, m as u32, 1)
}

/// Make `row`'s values of a unique key of several columns a combination not
/// given yet (reused AI rows repeat whole rows; patterns and short lists
/// repeat too): a made-up text column of the key gets a numbered variant
/// (`Ann Lee 2`, `ann+2@x.com`) that still fits, else another made-up value.
/// A key with a NULL can repeat (NULLs are distinct), so it is left alone.
fn distinct_combination(row: &mut [Value], key: &[usize], sources: &[Source], seen: &mut Seen, n: u64, rng: &mut Rng) -> bool {
    let combo = |row: &[Value]| -> Option<String> {
        let mut parts = Vec::with_capacity(key.len());
        for &k in key {
            match &row[k] {
                Value::Null => return None,
                v => parts.push(seen_key(v)),
            }
        }
        Some(parts.join("\u{1}"))
    };
    let Some(first) = combo(row) else { return true };
    if seen.insert(first) {
        return true;
    }
    // A numbered variant of a made-up text column of the key…
    for &k in key {
        let (Some(fallback), Value::Text(t)) = (made_up(&sources[k]), &row[k]) else { continue };
        let Gen::Text { max, .. } = fallback.base() else { continue };
        let base = t.clone();
        // Numbering goes on from the last variant of this value, so many
        // repeats of a few AI rows don't each try 2, 3, 4… again.
        let counter = format!("{k}\u{1}{}", fold(&base));
        let from = seen.next_number(&counter);
        for n in from..from.saturating_add(10_000) {
            seen.numbered(&counter, n + 1);
            let Some(v) = variant(&base, n, *max) else { break };
            // Still following the column's pattern (`ann+2@gmail.com` does).
            if let Gen::Patterned { pattern, base: b, .. } = fallback
                && pattern.accept(b, &v).is_none()
            {
                break;
            }
            row[k] = Value::Text(v);
            if let Some(c) = combo(row)
                && seen.insert(c)
            {
                return true;
            }
        }
        row[k] = Value::Text(base);
    }
    // …else another made-up value for one of its columns (numbers, dates).
    for &k in key {
        let Some(fallback) = made_up(&sources[k]) else { continue };
        let before = row[k].clone();
        for _ in 0..RETRIES {
            row[k] = random_value(fallback, n, rng);
            if let Some(c) = combo(row)
                && seen.insert(c)
            {
                return true;
            }
        }
        row[k] = before;
    }
    false
}

/// The generator of a made-up column (random or AI); `None` for keys,
/// foreign keys and NULLs.
fn made_up(s: &Source) -> Option<&Gen> {
    match s {
        Source::Random(g) | Source::Sample { fallback: g, .. } => Some(g),
        _ => None,
    }
}

/// Tries for a made-up value not given yet.
const RETRIES: usize = 20;

/// `v`, or for a value given already a numbered variant of it (text) or
/// another made-up value; counted when none is found.
fn not_given(seen: &mut Seen, v: Value, g: &Gen, n: u64, rng: &mut Rng, counts: &mut Counts) -> Value {
    if seen.insert(seen_key(&v)) {
        return v;
    }
    if let Value::Text(t) = &v
        && let Some(d) = distinct(seen, t.clone(), g)
    {
        return d;
    }
    for _ in 0..RETRIES {
        let w = random_value(g, n, rng);
        if seen.insert(seen_key(&w)) {
            return w;
        }
    }
    counts.repeats += 1;
    v
}

/// What a value of a unique column is compared by: text ignoring case and
/// trailing spaces, as case-insensitive collations do (MySQL `_ci`, SQL
/// Server's default), so `Ann@x.com` and `ann@x.com ` are one value.
fn seen_key(v: &Value) -> String {
    match v {
        Value::Text(t) => fold(t),
        v => format!("{v:?}"),
    }
}

fn fold(t: &str) -> String {
    t.trim_end_matches(' ').to_lowercase()
}

/// Values (or combinations) of a unique column or key given so far, and the
/// next number to try for each value made distinct (`Ann 2`, `Ann 3`…).
#[derive(Default, Clone)]
struct Seen {
    given: HashSet<String>,
    next: HashMap<String, u32>,
}

impl Seen {
    fn insert(&mut self, key: String) -> bool {
        self.given.insert(key)
    }
    fn next_number(&self, of: &str) -> u32 {
        self.next.get(of).copied().unwrap_or(2)
    }
    fn numbered(&mut self, of: &str, next: u32) {
        self.next.insert(of.to_string(), next);
    }
}

/// AI values made up instead, and rows that still repeat a unique key.
#[derive(Debug, Default)]
struct Counts {
    misfits: usize,
    repeats: usize,
}

/// `t` numbered `n` (`ann+2@x.com`, `Ann 2`), or `None` when it wouldn't fit.
fn variant(t: &str, n: u32, max: Option<usize>) -> Option<String> {
    let v = match t.split_once('@') {
        Some((local, domain)) => format!("{local}+{n}@{domain}"),
        None => format!("{t} {n}"),
    };
    max.is_none_or(|m| v.len() <= m).then_some(v)
}

/// `t`, or a variant of it not given yet (`ann+2@x.com`, `Ann 2`) that
/// still fits the column; `None` when there is none.
fn distinct(seen: &mut Seen, t: String, g: &Gen) -> Option<Value> {
    if seen.insert(fold(&t)) {
        return Some(Value::Text(t));
    }
    let max = match g.base() {
        Gen::Text { max, .. } => *max,
        _ => return None,
    };
    let counter = fold(&t);
    let from = seen.next_number(&counter);
    for n in from..from.saturating_add(1000) {
        seen.numbered(&counter, n + 1);
        let v = variant(&t, n, max)?;
        // A variant must still follow the column's pattern (`ann+2@gmail.com` does).
        if let Gen::Patterned { pattern, base, .. } = g
            && pattern.accept(base, &v).is_none()
        {
            return None;
        }
        if seen.insert(fold(&v)) {
            return Some(Value::Text(v));
        }
    }
    None
}

/// Lower-case names of columns with a one-column UNIQUE constraint or
/// unique index in the DDL (`email text UNIQUE`, `UNIQUE (email)`,
/// `UNIQUE KEY uq (email)`, `CREATE UNIQUE INDEX … ON t (email)`).
#[cfg(test)]
fn unique_columns(ddl: Option<&str>, cols: &[ColumnInfo]) -> HashSet<String> {
    unique_keys(ddl, cols).into_iter().filter(|k| k.len() == 1).flatten().collect()
}

/// The column lists (lower case) of the UNIQUE constraints and unique
/// indexes in the DDL, one-column and several (`UNIQUE (first_name,
/// last_name)`). A list with a part that isn't a column is left out.
fn unique_keys(ddl: Option<&str>, cols: &[ColumnInfo]) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = Vec::new();
    let Some(ddl) = ddl else { return out };
    let norm = |s: &str| s.trim().trim_matches(['"', '`', '[', ']']).to_lowercase();
    let is_col = |n: &str| cols.iter().any(|c| c.name.to_lowercase() == n);
    let list = |l: &str| -> Option<Vec<String>> { top_level(l, ',').into_iter().map(|n| index_column(n).filter(|n| is_col(n))).collect() };
    let mut add = |k: Vec<String>| {
        if !k.is_empty() && !out.contains(&k) {
            out.push(k);
        }
    };
    for stmt in top_level(ddl, ';') {
        let upper = stmt.to_ascii_uppercase();
        let words: Vec<&str> = upper.split_whitespace().collect();
        if words.first() == Some(&"CREATE") && words.contains(&"UNIQUE") && words.contains(&"INDEX") {
            if let Some(on) = upper.find(" ON ")
                && let Some(k) = balanced(&stmt[on..]).and_then(list)
            {
                add(k);
            }
            continue;
        }
        let Some(open) = stmt.find('(') else { continue };
        let Some(close) = stmt.rfind(')') else { continue };
        if close <= open {
            continue;
        }
        for def in top_level(&stmt[open + 1..close], ',') {
            let def = def.trim();
            let du = def.to_ascii_uppercase();
            let Some(at) = du.find("UNIQUE") else { continue };
            let first = du.split_whitespace().next().unwrap_or("");
            if matches!(first, "UNIQUE" | "CONSTRAINT" | "PRIMARY" | "KEY" | "INDEX") {
                // A table constraint: its column list.
                if let Some(k) = balanced(&def[at..]).and_then(list) {
                    add(k);
                }
            } else if let Some(name) = def.split_whitespace().next().map(norm)
                && is_col(&name)
            {
                add(vec![name]);
            }
        }
    }
    out
}

/// The text inside the first parentheses of `t`, nested ones included.
fn balanced(t: &str) -> Option<&str> {
    let open = t.find('(')?;
    let mut depth = 0;
    for (i, ch) in t[open..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&t[open + 1..open + i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// The column an index key part is on (lower case): `"email"`, `[email] DESC`,
/// `` `email`(50) `` (MySQL prefix), `lower(email)`.
fn index_column(part: &str) -> Option<String> {
    let norm = |s: &str| s.trim().trim_matches(['"', '`', '[', ']']).to_lowercase();
    let mut t = part.trim();
    for suffix in [" DESC", " ASC", " desc", " asc"] {
        t = t.strip_suffix(suffix).unwrap_or(t).trim_end();
    }
    let name = match t.find('(') {
        Some(0) => return None,
        // A function of one column: `lower(email)`.
        Some(o) if t[..o].trim_end().chars().all(|c| c.is_ascii_alphanumeric() || c == '_') => balanced(t)?,
        // A prefix length: `email`(50).
        Some(o) => &t[..o],
        None => t,
    };
    let n = norm(name);
    (!n.is_empty() && n.chars().all(|c| c.is_alphanumeric() || c == '_' || c == ' ' || c == '$')).then_some(n)
}

/// CHECK constraints in the DDL, with the table's columns each names.
fn check_constraints(ddl: Option<&str>, cols: &[TestDataColumn]) -> Vec<AiCheck> {
    let Some(ddl) = ddl else { return vec![] };
    let upper = ddl.to_ascii_uppercase();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(at) = upper[from..].find("CHECK") {
        let at = from + at;
        from = at + 5;
        // The word, not part of one (`CHECKSUM`).
        let before_ok = at == 0 || !upper.as_bytes()[at - 1].is_ascii_alphanumeric();
        let rest = &ddl[at + 5..];
        if !before_ok || !rest.trim_start().starts_with('(') {
            continue;
        }
        let Some(body) = balanced(rest) else { continue };
        let words: HashSet<String> = body.split(|c: char| !(c.is_alphanumeric() || c == '_')).map(str::to_lowercase).collect();
        let columns: Vec<String> = cols.iter().map(|c| c.name.to_lowercase()).filter(|n| words.contains(n)).collect();
        if !columns.is_empty() {
            out.push(AiCheck { sql: format!("CHECK ({})", body.trim()), columns });
        }
    }
    out
}

/// `s` split at `sep` outside parentheses and quotes.
fn top_level(s: &str, sep: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut depth, mut quote, mut start) = (0i32, None::<char>, 0);
    for (i, ch) in s.char_indices() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"' | '`') => quote = Some(ch),
            (None, '[') => quote = Some(']'),
            (None, '(') => depth += 1,
            (None, ')') => depth -= 1,
            (None, c) if c == sep && depth == 0 => {
                parts.push(&s[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&s[start..]);
    parts
}

#[allow(clippy::too_many_arguments)]
fn generate(
    columns: &[Column],
    sources: &[Source],
    nullable: &[bool],
    parents: &[Vec<Vec<Option<String>>>],
    sample: Option<&AiSample>,
    groups: &[Vec<usize>],
    unique: &[bool],
    rows: usize,
    null_percent: u8,
    rng: &mut Rng,
    counts: &mut Counts,
) -> Result<Vec<RecordBatch>> {
    let mut b = BatchBuilder::new(columns, BATCH.min(rows));
    let mut out = Vec::new();
    // Values given so far, per unique AI column.
    let mut seen: Vec<Seen> = vec![Seen::default(); sources.len()];
    // The AI's columns draw from their own stream, so how many values of an
    // answer fit doesn't change the other columns: the seed still repeats them.
    let mut ai_rng = Rng(rng.0 ^ 0x5DEE_CE66_D1CE_4E5B);
    // Combinations given so far, per unique key of several columns.
    let mut seen_groups: Vec<Seen> = vec![Seen::default(); groups.len()];
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
                        let v = random_value(g, i as u64 + 1, rng);
                        // A unique column: a value not given yet (a pattern
                        // such as `%@gmail.com` has few of its own).
                        if unique.get(c).copied().unwrap_or(false) { not_given(&mut seen[c], v, g, i as u64 + 1, rng, counts) } else { v }
                    }
                }
                Source::Sample { col, fallback, unique } => {
                    let rng = &mut ai_rng;
                    // NULLs only as often as asked (the AI's NULLs are made up instead).
                    if nullable[c] && null_percent > 0 && rng.below(100) < null_percent as u64 {
                        return Value::Null;
                    }
                    // Row by row, so a row's AI values belong together (name and email).
                    let rows_of = sample.map(|s| s.rows.as_slice()).unwrap_or_default();
                    let raw = if rows_of.is_empty() { None } else { rows_of[i % rows_of.len()].get(*col).cloned().flatten() };
                    let fit = raw.as_deref().and_then(|r| sample_value(fallback, r));
                    let fit = match fit {
                        Some(Value::Text(t)) if *unique => distinct(&mut seen[c], t, fallback),
                        Some(v) if *unique => seen[c].insert(seen_key(&v)).then_some(v),
                        v => v,
                    };
                    match fit {
                        Some(v) => v,
                        None => {
                            if raw.is_some() {
                                counts.misfits += 1;
                            }
                            // Made up instead; for a unique column, one not given yet.
                            let v = random_value(fallback, i as u64 + 1, rng);
                            if *unique { not_given(&mut seen[c], v, fallback, i as u64 + 1, rng, counts) } else { v }
                        }
                    }
                }
            })
            .collect();
        let mut row = row;
        for (g, key) in groups.iter().enumerate() {
            if !distinct_combination(&mut row, key, sources, &mut seen_groups[g], i as u64 + 1, &mut ai_rng) {
                counts.repeats += 1;
            }
        }
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
        assert_eq!(g(Clickhouse, "Decimal32(4)"), Some(Gen::Decimal { int_digits: 5, scale: 4 }), "the argument is the scale");
        assert_eq!(g(Clickhouse, "Decimal256(40)"), Some(Gen::Decimal { int_digits: 6, scale: 6 }));
        assert_eq!(g(Clickhouse, "Decimal64(0)"), Some(Gen::Int { min: 1, max: 999_999 }));
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
        // The computed column can't be ticked; the identity one can.
        let computed: Vec<bool> = p.columns.iter().map(|c| c.computed).collect();
        assert_eq!(computed, [false, true, false, true]);
        assert!(p.columns[1].note.as_deref().unwrap().contains("can't be filled"));
        assert!(!p.columns[3].supported, "rowversion can't be written");
        assert!(p.warnings.is_empty(), "the database fills rowversion: no 'inserts will fail' {:?}", p.warnings);
        // Elsewhere TIMESTAMP is a date-time.
        assert!(!database_fills(ConnectorKind::Mysql, &col("rv", "timestamp", false, false), None));
        // Generated columns elsewhere; identity columns aren't computed.
        let pg = "CREATE TABLE t (\n  id integer GENERATED ALWAYS AS IDENTITY,\n  total numeric GENERATED ALWAYS AS (qty * price) STORED,\n  qty int\n);";
        let (p, _) = plan_for(
            ConnectorKind::Postgres,
            "public",
            "t",
            &[col("id", "integer", false, true), col("total", "numeric", true, false), col("qty", "integer", true, false)],
            &[],
            Some(pg),
        );
        assert_eq!(p.columns.iter().map(|c| c.computed).collect::<Vec<_>>(), [false, true, false]);
        let ch = "CREATE TABLE t\n(\n    `day` Date MATERIALIZED toDate(ts),\n    `ts` DateTime\n)";
        let (p, _) =
            plan_for(ConnectorKind::Clickhouse, "default", "t", &[col("day", "Date", false, false), col("ts", "DateTime", false, false)], &[], Some(ch));
        assert_eq!(p.columns.iter().map(|c| c.computed).collect::<Vec<_>>(), [true, false]);
    }

    /// Columns the database fills (computed, generated, identity), keys and
    /// foreign keys get no default pattern, so a plan's defaults never stop
    /// generation; only a pattern asked for such a column is refused.
    #[test]
    fn columns_without_a_pattern_get_no_default_one() {
        let ddl = "CREATE TABLE [dbo].[t] (\n    [id] int IDENTITY(1,1) NOT NULL,\n    [total] AS ([qty]*[price]) PERSISTED,\n    [qty] int NOT NULL,\n    [price] decimal(8,2) NOT NULL,\n    [age] int NOT NULL,\n    [amount] int NOT NULL\n);";
        let cols = vec![
            col("id", "int", false, true),
            col("total", "decimal(12,2)", true, false),
            col("qty", "int", false, false),
            col("price", "decimal(8,2)", false, false),
            col("age", "int", false, false),
            col("amount", "int", false, false),
        ];
        let fks = vec![ForeignKey { columns: vec!["amount".into()], ref_schema: String::new(), ref_table: "a".into(), ref_columns: vec!["id".into()] }];
        let (plan, gens) = plan_for(ConnectorKind::Mssql, "dbo", "t", &cols, &fks, Some(ddl));
        let by = |n: &str| plan.columns.iter().find(|c| c.name == n).unwrap();
        assert!(by("total").database_fills);
        for n in ["id", "total", "amount"] {
            assert_eq!((by(n).pattern.as_deref(), by(n).pattern_hint.as_deref()), (None, None), "{n}");
        }
        assert_eq!(by("price").pattern.as_deref(), Some("1..1000"));
        assert_eq!(by("age").pattern.as_deref(), Some("18..90"));
        // Every column ticked, the computed one too (`fill`), no pattern sent: the defaults go through.
        let all: Vec<usize> = (0..plan.columns.len()).collect();
        let mut g = gens.clone();
        apply_patterns(ConnectorKind::Mssql, &plan, &mut g, &all, &HashMap::new()).unwrap();
        assert!(matches!(g[3], Some(Gen::Patterned { .. })) && matches!(g[4], Some(Gen::Patterned { .. })));
        assert!(!matches!(g[1], Some(Gen::Patterned { .. })), "the computed column keeps its own values");
        // A pattern asked for the computed column or a key is refused.
        for n in ["total", "id"] {
            let e = apply_patterns(ConnectorKind::Mssql, &plan, &mut gens.clone(), &all, &HashMap::from([(n.to_string(), "1..5".to_string())])).unwrap_err();
            assert!(e.message.contains("don't take a pattern") && e.message.starts_with(n), "{}", e.message);
        }
        // Postgres and MySQL generated columns too.
        let pg = "CREATE TABLE t (\n    year integer GENERATED ALWAYS AS (2000 + n) STORED,\n    n integer\n);";
        let (p, _) = plan_for(ConnectorKind::Postgres, "public", "t", &[col("year", "integer", true, false), col("n", "integer", true, false)], &[], Some(pg));
        assert!(p.columns[0].database_fills && p.columns[0].pattern.is_none());
    }

    /// Only the columns filled have their patterns read: a pattern saved for
    /// a column left out doesn't stop generation, however wrong.
    #[test]
    fn patterns_of_columns_left_out_are_ignored() {
        let cols = vec![col("age", "int", false, false), col("email", "varchar(5)", true, false)];
        let (plan, gens) = plan_for(ConnectorKind::Postgres, "public", "t", &cols, &[], None);
        let patterns = HashMap::from([("email".to_string(), "INV-#####".to_string()), ("age".to_string(), "30..40".to_string())]);
        let mut g = gens.clone();
        apply_patterns(ConnectorKind::Postgres, &plan, &mut g, &[0], &patterns).unwrap();
        assert!(matches!(&g[0], Some(Gen::Patterned { text, .. }) if text == "30..40"));
        assert!(matches!(g[1], Some(Gen::Text { .. })));
        let e = apply_patterns(ConnectorKind::Postgres, &plan, &mut gens.clone(), &[0, 1], &patterns).unwrap_err();
        assert!(e.message.contains("Pattern for email"), "{}", e.message);
    }

    #[test]
    fn types_report_what_they_hold() {
        use ConnectorKind::*;
        let l = type_limits;
        assert_eq!(l(Mysql, "tinyint unsigned"), Limits::Ints(0, 255));
        assert_eq!(l(Mysql, "tinyint(4)"), Limits::Ints(-128, 127));
        assert_eq!(l(Mssql, "tinyint"), Limits::Ints(0, 255));
        assert_eq!(l(Postgres, "smallint"), Limits::Ints(-32_768, 32_767));
        assert_eq!(l(Mysql, "mediumint unsigned"), Limits::Ints(0, 16_777_215));
        assert_eq!(l(Mysql, "int unsigned"), Limits::Ints(0, 4_294_967_295));
        assert_eq!(l(Postgres, "bigint"), Limits::Ints(i64::MIN.into(), i64::MAX.into()));
        assert_eq!(l(Clickhouse, "Nullable(UInt8)"), Limits::Ints(0, 255));
        assert_eq!(l(Clickhouse, "Int8"), Limits::Ints(-128, 127), "ClickHouse Int8 is 8 bits");
        assert_eq!(l(Clickhouse, "Int16"), Limits::Ints(-32_768, 32_767));
        assert_eq!(l(Duckdb, "UTINYINT"), Limits::Ints(0, 255));
        assert_eq!(l(Sqlite, "SMALLINT"), Limits::Ints(i64::MIN.into(), i64::MAX.into()), "SQLite integers are 64-bit");
        assert_eq!(l(Oracle, "NUMBER(2)"), Limits::Ints(-99, 99));
        assert_eq!(l(Postgres, "numeric(5,2)"), Limits::Decimal { precision: 5, scale: 2 });
        assert_eq!(l(Clickhouse, "Decimal32(4)"), Limits::Decimal { precision: 9, scale: 4 }, "the argument is the scale");
        assert_eq!(l(Postgres, "numeric"), Limits::Any);
        // Past 38 digits: no limit on the whole part, the scale still holds.
        assert_eq!(l(Postgres, "numeric(60,2)"), Limits::Decimal { precision: 60, scale: 2 });
        assert_eq!(l(Bigquery, "BIGNUMERIC(50,20)"), Limits::Decimal { precision: 50, scale: 20 });
        assert_eq!(l(Clickhouse, "Decimal256(40)"), Limits::Decimal { precision: 76, scale: 40 });
        assert_eq!(l(Postgres, "numeric(50)"), Limits::Ints(-i128::MAX, i128::MAX));
        assert_eq!(l(Postgres, "numeric(60,2)").int_range(), Some((-i128::MAX, i128::MAX)));
        // Oracle negative scale: 10 digits rounded to hundreds.
        assert_eq!(l(Oracle, "NUMBER(10,-2)"), Limits::Ints(-999_999_999_900, 999_999_999_900));
        assert_eq!(l(Oracle, "NUMBER(10, -2)"), Limits::Ints(-999_999_999_900, 999_999_999_900));
        assert_eq!(l(Mysql, "year"), Limits::Ints(1901, 2155));
        let day = |y, m, d| days_from_civil(y, m, d) * MICROS_PER_DAY;
        assert_eq!(l(Mysql, "timestamp"), Limits::Moments(day(1970, 1, 2), day(2038, 1, 19) - 1_000_000));
        assert_eq!(l(Mssql, "smalldatetime").moments().unwrap().0, day(1900, 1, 1));
        assert_eq!(l(Mssql, "datetime").moments().unwrap().0, day(1753, 1, 1));
        assert_eq!(l(Postgres, "timestamp without time zone").moments().unwrap().1, day(10_000, 1, 1) - 1_000_000);
        assert_eq!(l(Postgres, "text"), Limits::Any);
    }

    /// Patterns, defaults and `{a|b}` values follow what the column's type
    /// holds, not only what its generator makes.
    #[test]
    fn patterns_fit_the_column_type() {
        use ConnectorKind::*;
        let parse = |k, t: &str, p: &str| Pattern::parse(&parse_type(k, t, "x").unwrap(), &type_limits(k, t), p);
        let e = parse(Postgres, "numeric(5,2)", "1..1000").unwrap_err();
        assert!(e.contains("holds -999.99 to 999.99"), "{e}");
        assert!(parse(Postgres, "numeric(5,2)", "1..999.99").is_ok());
        assert!(parse(Postgres, "numeric(5,2)", "0..999.996").is_err(), "rounds to 1000.00");
        assert!(parse(Oracle, "NUMBER(2)", "1..100").unwrap_err().contains("-99 to 99"));
        assert!(parse(Oracle, "NUMBER(2)", "1..99").is_ok());
        assert!(parse(Mysql, "tinyint unsigned", "-5..5").unwrap_err().contains("0 to 255"));
        assert!(parse(Mysql, "tinyint", "-5..5").is_ok());
        assert!(parse(Mysql, "smallint", "1..40000").is_err());
        assert!(parse(Clickhouse, "UInt8", "0..255").is_ok(), "past the generator's 0..200");
        // `{0|1}` on an int: 0 is outside the made-up values (1..), inside the type.
        assert!(matches!(parse(Postgres, "integer", "{0|1}"), Ok(Pattern::OneOf(v)) if v[0].1 == Value::Int(0)));
        assert!(parse(Mysql, "tinyint unsigned", "{-1|1}").unwrap_err().contains("\"-1\""));
        assert!(parse(Postgres, "numeric(12,2)", "{1234567.5}").is_ok(), "the real precision, not the generator's 6 digits");
        assert!(parse(Postgres, "numeric(5,2)", "{1234.5}").is_err());
        // Wider than 38 digits: patterns are taken.
        assert!(parse(Postgres, "numeric(60,2)", "1..99999999999999999999.99").is_ok());
        assert!(parse(Bigquery, "BIGNUMERIC(50,20)", "0.5..1000000").is_ok());
        assert!(parse(Clickhouse, "Decimal256(40)", "{12345678901234567890.123}").is_ok());
        assert!(parse(Oracle, "NUMBER(10,-2)", "100..900").is_ok());
        // MySQL TIMESTAMP: 1970–2038.
        let e = parse(Mysql, "timestamp", "1950..2005").unwrap_err();
        assert!(e.contains("1970-01-02 to 2038-01-18"), "{e}");
        assert!(parse(Mysql, "timestamp", "1980..2030").is_ok());
        assert!(parse(Mysql, "timestamp", "{1965-01-01 10:00:00}").is_err());
        assert!(parse(Mysql, "datetime", "1950..2005").is_ok());
        assert!(parse(Mssql, "smalldatetime", "1890..1950").is_err());
        assert!(parse(Mssql, "smalldatetime", "1900..2079-06-05").is_ok());
        assert!(parse(Mssql, "datetime", "1700..1800").is_err());
        assert!(parse(Mssql, "datetime2", "1700..1800").is_ok());
        // The defaults from names fit the type.
        let plan = |k, cols: &[(&str, &str)]| {
            let cols: Vec<ColumnInfo> = cols.iter().map(|(n, t)| col(n, t, true, false)).collect();
            plan_for(k, "s", "t", &cols, &[], None).0.columns.into_iter().map(|c| c.pattern).collect::<Vec<_>>()
        };
        let got =
            plan(Mysql, &[("price", "decimal(5,2)"), ("fee", "decimal(2,2)"), ("age", "tinyint unsigned"), ("total", "tinyint"), ("birth_date", "timestamp")]);
        assert_eq!(
            got,
            [Some("1..999.99".into()), None, Some("18..90".into()), Some("1..127".into()), Some("1970-01-02..2005-12-31".into())],
            "a decimal(2,2) holds no 1"
        );
        assert_eq!(plan(Postgres, &[("birth_date", "timestamp")]), [Some("1950-01-01..2005-12-31".into())]);
    }

    /// A counting key stays within its type (smallint stops at 32,767).
    #[test]
    fn keys_stay_within_the_type() {
        use ConnectorKind::*;
        let key = |k, t: &str, found: Option<&str>, rows| key_start(&parse_type(k, t, "id").unwrap(), &type_limits(k, t), found, rows, "id", t);
        let e = key(Postgres, "smallint", Some("32700"), 100).unwrap_err();
        assert!(e.contains("no room for 100 more unique keys") && e.contains("32767"), "{e}");
        assert_eq!(key(Postgres, "smallint", Some("32700"), 67), Ok(32_701));
        assert!(key(Mysql, "tinyint unsigned", None, 256).is_err());
        assert_eq!(key(Mysql, "tinyint unsigned", None, 255), Ok(1));
        assert!(key(Oracle, "NUMBER(3)", Some("990"), 10).is_err());
        assert!(key(Postgres, "numeric(4,2)", Some("90"), 10).is_err(), "numeric(4,2) holds up to 99.99");
        assert!(key(Mysql, "timestamp", Some("2038-01-18 23:59:00"), 100).unwrap_err().contains("2038-01-18"));
        assert!(key(Postgres, "timestamp", Some("2038-01-18 23:59:00"), 100).is_ok());
        // Past 38 digits, keys and patterns are allowed.
        assert_eq!(key(Postgres, "numeric(60,2)", Some("1234567890123.45"), 10), Ok(1_234_567_890_124));
        assert_eq!(key(Bigquery, "BIGNUMERIC(50,20)", None, 10), Ok(1));
        assert_eq!(key(Clickhouse, "Decimal256(40)", None, 10), Ok(1));
        // A negative scale rounds counted keys together: refused, with why.
        let e = key(Oracle, "NUMBER(10,-2)", None, 10).unwrap_err();
        assert!(e.contains("multiples of 100") && e.contains("Leave id to the database"), "{e}");
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
        assert_eq!(key_start(&int, &Limits::Any, Some("9007199254740993"), 10, "id", "bigint"), Ok(9_007_199_254_740_994));
        assert_eq!(key_start(&int, &Limits::Any, None, 10, "id", "bigint"), Ok(1));
        let e = key_start(&int, &Limits::Any, Some("9223372036854775800"), 100, "id", "bigint").unwrap_err();
        assert!(e.contains("no room for 100 more"), "{e}");
        assert!(key_start(&int, &Limits::Any, Some("9223372036854775807"), 1, "id", "bigint").is_err());
        assert!(key_start(&Gen::Float, &Limits::Any, Some("9007199254740990"), 10, "id", "float").is_err());
    }

    #[test]
    fn short_text_keys_stay_unique_and_continue_after_existing_rows() {
        let g = Gen::Text { max: Some(2), flavor: Flavor::Word };
        assert_eq!(key_start(&g, &Limits::Any, Some("0"), 1000, "code", "varchar(2)"), Ok(1));
        assert_eq!(key_start(&g, &Limits::Any, Some("100"), 10, "code", "varchar(2)"), Ok(101), "past the rows already there");
        let e = key_start(&g, &Limits::Any, Some("0"), 1296, "code", "varchar(2)").unwrap_err();
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
        assert_eq!(key_start(&Gen::Date, &Limits::Any, Some("2024-02-03"), 10, "d", "date"), Ok(day + 1));
        assert_eq!(key_start(&Gen::Date, &Limits::Any, Some("2024-02-03 00:00:00"), 10, "d", "date"), Ok(day + 1), "Oracle DATE");
        assert_eq!(key_start(&Gen::Date, &Limits::Any, None, 10, "d", "date"), Ok(days_from_civil(2020, 1, 1)));
        let ts = key_start(&Gen::Timestamp, &Limits::Any, Some("2024-02-03 04:05:06.5"), 10, "t", "timestamp").unwrap();
        assert_eq!(key_value(&Gen::Timestamp, ts, "", &mut rng), Value::Timestamp(parse_datetime("2024-02-03 04:05:07").unwrap()));
        assert!(key_start(&Gen::Time, &Limits::Any, Some("23:59:00"), 60, "t", "time").is_err());
        assert_eq!(key_start(&Gen::Time, &Limits::Any, Some("23:59:00"), 59, "t", "time"), Ok(86_341));
        assert!(key_start(&Gen::Binary { max: Some(1) }, &Limits::Any, Some("200"), 56, "b", "binary(1)").is_err());
        assert!(key_start(&Gen::Bool, &Limits::Any, None, 3, "b", "boolean").is_err());

        let columns = vec![Column::new("d", ColType::Date, "date"), Column::new("f", ColType::Float64, "real")];
        let sources = vec![Source::Key { maker: Gen::Date, start: day, tag: String::new() }, Source::Key { maker: Gen::Float, start: 7, tag: String::new() }];
        let b = generate(&columns, &sources, &[false, false], &[], None, &[], &[], 5000, 0, &mut rng, &mut Counts::default()).unwrap();
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
    fn ai_values_must_fit_their_column() {
        let text = Gen::Text { max: Some(10), flavor: Flavor::Word };
        assert_eq!(sample_value(&text, " Hà Nội "), Some(Value::Text("Hà Nội".into())), "9 bytes");
        assert_eq!(sample_value(&text, "much too long for ten"), None);
        assert_eq!(sample_value(&text, "  "), None);
        let status = Gen::Choice(vec!["new".into(), "paid".into()]);
        assert_eq!(sample_value(&status, "PAID"), Some(Value::Text("paid".into())));
        assert_eq!(sample_value(&status, "shipped"), None);
        assert_eq!(sample_value(&Gen::Int { min: 0, max: 100 }, "42"), Some(Value::Int(42)));
        assert_eq!(sample_value(&Gen::Int { min: 0, max: 100 }, "4200"), None);
        assert_eq!(sample_value(&Gen::Int { min: 0, max: 100 }, "4.5"), None);
        assert_eq!(sample_value(&Gen::Int { min: 0, max: 100 }, "-5"), None, "unsigned and tinyint columns");
        assert_eq!(sample_value(&Gen::Int { min: 1990, max: 2030 }, "1500"), None, "MySQL YEAR");
        // Bytes as well as characters: VARCHAR2(6) holds "Nguyen", not "Nguyễn".
        let six = Gen::Text { max: Some(6), flavor: Flavor::LastName };
        assert_eq!(sample_value(&six, "Nguyen"), Some(Value::Text("Nguyen".into())));
        assert_eq!(sample_value(&six, "Nguyễn"), None);
        // Years every engine's date and date-time types hold.
        assert_eq!(sample_value(&Gen::Timestamp, "1965-03-04 10:00:00"), None, "MySQL TIMESTAMP starts in 1970");
        assert_eq!(sample_value(&Gen::Date, "1850-01-01"), None);
        assert!(sample_value(&Gen::Date, "1965-03-04").is_some());
        let money = Gen::Decimal { int_digits: 4, scale: 2 };
        assert_eq!(sample_value(&money, "1234.5"), Some(Value::Text("1234.5".into())));
        assert_eq!(sample_value(&money, "-0012.25"), Some(Value::Text("-0012.25".into())));
        assert_eq!(sample_value(&money, "12345"), None);
        assert_eq!(sample_value(&money, "1.234"), None);
        assert_eq!(sample_value(&money, "1e3"), None);
        assert_eq!(sample_value(&Gen::Float, "NaN"), None);
        assert_eq!(sample_value(&Gen::Date, "2024-02-30"), None);
        assert!(matches!(sample_value(&Gen::Date, "2024-02-03"), Some(Value::Date(_))));
        assert_eq!(sample_value(&Gen::Bool, "maybe"), None);
        assert_eq!(sample_value(&Gen::Json, "{\"a\": 1}"), Some(Value::Text("{\"a\": 1}".into())));
        assert_eq!(sample_value(&Gen::Json, "{a: 1}"), None);
        assert_eq!(sample_value(&Gen::Inet, "10.0.0.300"), None);
        assert_eq!(sample_value(&Gen::Uuid, "not for the AI"), None);
    }

    #[test]
    fn unique_columns_are_read_from_the_ddl() {
        let cols =
            vec![col("id", "integer", false, true), col("email", "text", false, false), col("code", "text", false, false), col("a", "text", true, false)];
        let ddl = "CREATE TABLE t (\n  id integer PRIMARY KEY,\n  \"email\" text NOT NULL UNIQUE,\n  code text,\n  a text,\n  UNIQUE (code),\n  CONSTRAINT two UNIQUE (a, code)\n);";
        let u = unique_columns(Some(ddl), &cols);
        assert_eq!(u, HashSet::from(["email".to_string(), "code".to_string()]), "a is only unique together with code");
        let u = unique_columns(Some("CREATE TABLE t (id int, a text);\nCREATE UNIQUE INDEX ix ON t (a);"), &cols);
        assert_eq!(u, HashSet::from(["a".to_string()]));
        // Several columns on one line (SQLite keeps the CREATE as typed), types with commas.
        let u =
            unique_columns(Some("create table t (id integer primary key, code numeric(8,2) check (code in (1, 2)), email varchar(40) unique, a text)"), &cols);
        assert_eq!(u, HashSet::from(["email".to_string()]));
        let u = unique_columns(Some("CREATE TABLE `t` (\n  `id` int NOT NULL,\n  `a` varchar(9),\n  PRIMARY KEY (`id`),\n  UNIQUE KEY `uq_a` (`a`)\n)"), &cols);
        assert_eq!(u, HashSet::from(["a".to_string()]), "MySQL");
        let u = unique_columns(Some("CREATE UNIQUE INDEX ux ON t (lower(email));"), &cols);
        assert_eq!(u, HashSet::from(["email".to_string()]), "an expression of one column");
        assert_eq!(index_column("`email`(50)").as_deref(), Some("email"));
        assert_eq!(index_column("[email] DESC").as_deref(), Some("email"));
        assert_eq!(index_column("\"Email\"").as_deref(), Some("email"));
        assert_eq!(index_column("(a + b)"), None);
        // Keys of several columns.
        let keys = unique_keys(
            Some(
                "CREATE TABLE t (id int, email text, code text, a text, UNIQUE (code, a), UNIQUE (a, lower(x)));\nCREATE UNIQUE INDEX ux ON t (\"email\", code);",
            ),
            &cols,
        );
        assert_eq!(keys, vec![vec!["code".to_string(), "a".to_string()], vec!["email".to_string(), "code".to_string()]], "x isn't a column");
        assert!(unique_columns(None, &cols).is_empty());
    }

    #[test]
    fn reused_ai_rows_keep_unique_keys_of_several_columns_unique() {
        let columns = vec![
            Column::new("first", ColType::Utf8, "varchar(20)"),
            Column::new("last", ColType::Utf8, "varchar(20)"),
            Column::new("team", ColType::Int64, "int"),
        ];
        let text = Gen::Text { max: Some(20), flavor: Flavor::FirstName };
        let sources = vec![
            Source::Sample { col: 0, fallback: text.clone(), unique: false },
            Source::Sample { col: 1, fallback: text, unique: false },
            Source::Key { maker: Gen::Int { min: 1, max: 9 }, start: 1, tag: String::new() },
        ];
        let sample = AiSample {
            columns: vec!["first".into(), "last".into()],
            rows: vec![vec![Some("Ann".into()), Some("Lee".into())], vec![Some("Bo".into()), Some("Kim".into())]],
        };
        let strings = |b: &[RecordBatch], c: usize| -> Vec<String> {
            b[0].column(c).as_any().downcast_ref::<StringArray>().unwrap().iter().map(|v| v.unwrap().to_string()).collect()
        };
        // UNIQUE (first, last): the 3rd row on reuses "Ann Lee", so a numbered variant.
        let b = generate(&columns, &sources, &[false; 3], &[], Some(&sample), &[vec![0, 1]], &[], 6, 0, &mut Rng(1), &mut Counts::default()).unwrap();
        let (first, last) = (strings(&b, 0), strings(&b, 1));
        assert_eq!(first, ["Ann", "Bo", "Ann 2", "Bo 2", "Ann 3", "Bo 3"]);
        assert_eq!(last, ["Lee", "Kim", "Lee", "Kim", "Lee", "Kim"], "one column of the key changes, the other keeps the AI's value");
        // UNIQUE (first, team): the counting key already makes each row distinct.
        let b = generate(&columns, &sources, &[false; 3], &[], Some(&sample), &[vec![0, 2]], &[], 6, 0, &mut Rng(1), &mut Counts::default()).unwrap();
        assert_eq!(strings(&b, 0), ["Ann", "Bo", "Ann", "Bo", "Ann", "Bo"]);
        // A variant that wouldn't fit the column isn't used: the next text column of the key changes.
        let tight = vec![
            Source::Sample { col: 0, fallback: Gen::Text { max: Some(3), flavor: Flavor::FirstName }, unique: false },
            Source::Sample { col: 1, fallback: Gen::Text { max: Some(20), flavor: Flavor::LastName }, unique: false },
            Source::Random(Gen::Int { min: 1, max: 1 }),
        ];
        let b = generate(&columns, &tight, &[false; 3], &[], Some(&sample), &[vec![0, 1]], &[], 4, 0, &mut Rng(1), &mut Counts::default()).unwrap();
        assert_eq!(strings(&b, 0), ["Ann", "Bo", "Ann", "Bo"]);
        assert_eq!(strings(&b, 1), ["Lee", "Kim", "Lee 2", "Kim 2"]);
    }

    #[test]
    fn reused_ai_numbers_and_dates_keep_unique_keys_unique() {
        let columns = vec![Column::new("age", ColType::Int64, "int"), Column::new("born", ColType::Date, "date")];
        let age = Gen::Int { min: 18, max: 90 };
        let sources = vec![Source::Sample { col: 0, fallback: age.clone(), unique: false }, Source::Sample { col: 1, fallback: Gen::Date, unique: false }];
        let sample = AiSample { columns: vec!["age".into(), "born".into()], rows: vec![vec![Some("40".into()), Some("1985-02-03".into())]] };
        let mut counts = Counts::default();
        let b = generate(&columns, &sources, &[false; 2], &[], Some(&sample), &[vec![0, 1]], &[], 50, 0, &mut Rng(1), &mut counts).unwrap();
        let ages = b[0].column(0).as_any().downcast_ref::<Int64Array>().unwrap().values().to_vec();
        let days = b[0].column(1).as_any().downcast_ref::<Date32Array>().unwrap().values().to_vec();
        let pairs: HashSet<(i64, i32)> = ages.iter().copied().zip(days.iter().copied()).collect();
        assert_eq!(pairs.len(), 50, "UNIQUE (age, born) holds");
        assert_eq!((ages[0], days[0]), (40, days_from_civil(1985, 2, 3) as i32), "the AI's row as it wrote it");
        assert!(ages.iter().all(|a| (18..=90).contains(a)), "made-up values stay in the column's range");
        assert_eq!(counts.repeats, 0);

        // A unique column with fewer values than rows: the rows that can't be unique are counted.
        let tiny = vec![Source::Sample { col: 0, fallback: Gen::Int { min: 1, max: 3 }, unique: true }];
        let one = AiSample { columns: vec!["age".into()], rows: vec![vec![Some("2".into())]] };
        let mut counts = Counts::default();
        let b = generate(&columns[..1], &tiny, &[false], &[], Some(&one), &[], &[], 5, 0, &mut Rng(1), &mut counts).unwrap();
        let v: HashSet<i64> = b[0].column(0).as_any().downcast_ref::<Int64Array>().unwrap().values().iter().copied().collect();
        assert_eq!(v, HashSet::from([1, 2, 3]));
        assert_eq!(counts.repeats, 2, "5 rows, 3 values");
    }

    #[test]
    fn check_constraints_are_found_with_their_columns() {
        let mk = |n: &str| TestDataColumn {
            name: n.into(),
            data_type: "text".into(),
            nullable: true,
            primary_key: false,
            supported: true,
            database_fills: false,
            computed: false,
            generator: String::new(),
            ai_text: true,
            ai_ok: true,
            pattern: None,
            pattern_hint: None,
            parent: None,
            note: None,
        };
        let cols = vec![mk("tier"), mk("qty"), mk("note")];
        let ddl = "CREATE TABLE t (tier text CHECK (tier IN ('gold', 'silver')), qty int, note text DEFAULT 'ops@corp.com', CONSTRAINT c CHECK ((qty > 0)), checksum int)";
        let c = check_constraints(Some(ddl), &cols);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0], AiCheck { sql: "CHECK (tier IN ('gold', 'silver'))".into(), columns: vec!["tier".into()] });
        assert_eq!(c[1].columns, vec!["qty".to_string()]);
        assert!(!c.iter().any(|c| c.sql.contains("ops@corp.com")), "defaults aren't sent");
    }

    #[test]
    fn ai_rows_go_in_checked_and_unique() {
        let columns = vec![
            Column::new("name", ColType::Utf8, "varchar(12)"),
            Column::new("email", ColType::Utf8, "varchar(40)"),
            Column::new("n", ColType::Int64, "int"),
        ];
        let name = Gen::Text { max: Some(12), flavor: Flavor::FullName };
        let email = Gen::Text { max: Some(40), flavor: Flavor::Email };
        let sources = vec![
            Source::Sample { col: 0, fallback: name, unique: false },
            Source::Sample { col: 1, fallback: email, unique: true },
            Source::Random(Gen::Int { min: 1, max: 9 }),
        ];
        let sample = AiSample {
            columns: vec!["name".into(), "email".into()],
            rows: vec![
                vec![Some("Ann Lee".into()), Some("ann@x.com".into())],
                vec![Some("A name far too long".into()), Some("bob@x.com".into())],
                vec![None, None],
            ],
        };
        let mut counts = Counts::default();
        let b = generate(&columns, &sources, &[false, false, false], &[], Some(&sample), &[], &[], 6, 0, &mut Rng(1), &mut counts).unwrap();
        let text =
            |c: usize| -> Vec<String> { b[0].column(c).as_any().downcast_ref::<StringArray>().unwrap().iter().map(|v| v.unwrap().to_string()).collect() };
        let names = text(0);
        let emails = text(1);
        // Rows cycle through the AI's rows, so a row's values stay together.
        assert_eq!((names[0].as_str(), emails[0].as_str()), ("Ann Lee", "ann@x.com"));
        assert_eq!(names[3], "Ann Lee");
        assert_eq!(emails[3], "ann+2@x.com", "unique: a repeat gets a variant");
        assert_eq!(emails[4], "bob+2@x.com");
        // Too long, and NULL in NOT NULL columns: made up instead, and counted.
        assert_ne!(names[1], "A name far too long");
        assert!(names[1].chars().count() <= 12);
        assert!(!names[2].is_empty() && !emails[2].is_empty());
        assert_eq!(counts.misfits, 2, "the too-long name, twice (the AI's NULLs are made up, not counted)");
        assert_eq!(emails.iter().collect::<HashSet<_>>().len(), 6);

        // NULLs come from the NULL share only, not from the AI.
        let b = generate(&columns[..1], &sources[..1], &[true], &[], Some(&sample), &[], &[], 3, 0, &mut Rng(1), &mut Counts::default()).unwrap();
        assert_eq!(b[0].column(0).null_count(), 0);

        // The same seed gives the same other columns, whatever the AI wrote.
        let other = AiSample { columns: sample.columns.clone(), rows: vec![vec![Some("Way too long a name".into()), Some("x@y.z".into())]] };
        let nums = |s: &AiSample| -> Vec<i64> {
            let b = generate(&columns, &sources, &[false, false, false], &[], Some(s), &[], &[], 6, 0, &mut Rng(9), &mut Counts::default()).unwrap();
            b[0].column(2).as_any().downcast_ref::<Int64Array>().unwrap().values().to_vec()
        };
        assert_eq!(nums(&sample), nums(&other));
    }

    #[test]
    fn unique_values_ignore_case_and_trailing_spaces_and_numbering_goes_on() {
        let columns = vec![Column::new("email", ColType::Utf8, "varchar(40)"), Column::new("name", ColType::Utf8, "varchar(40)")];
        let email = Gen::Text { max: Some(40), flavor: Flavor::Email };
        let name = Gen::Text { max: Some(40), flavor: Flavor::FullName };
        let sources = vec![Source::Sample { col: 0, fallback: email, unique: true }, Source::Sample { col: 1, fallback: name, unique: false }];
        // A case-insensitive unique index (MySQL `_ci`, SQL Server) sees these as one value.
        let sample = AiSample {
            columns: vec!["email".into(), "name".into()],
            rows: vec![vec![Some("Ann@x.com".into()), Some("Ann".into())], vec![Some("ann@x.com ".into()), Some("ANN".into())]],
        };
        let strings = |b: &[RecordBatch], c: usize| -> Vec<String> {
            b.iter()
                .flat_map(|b| b.column(c).as_any().downcast_ref::<StringArray>().unwrap().iter().map(|v| v.unwrap().to_string()).collect::<Vec<_>>())
                .collect()
        };
        let b = generate(&columns, &sources, &[false; 2], &[], Some(&sample), &[], &[], 4, 0, &mut Rng(1), &mut Counts::default()).unwrap();
        let emails = strings(&b, 0);
        assert_eq!(emails[0], "Ann@x.com");
        assert_eq!(emails[1], "ann+2@x.com", "a repeat but for case (and a trailing space)");
        assert_eq!(emails.iter().map(|e| e.trim_end().to_lowercase()).collect::<HashSet<_>>().len(), 4);
        // A key of several columns compares the same way.
        let b = generate(&columns, &sources, &[false; 2], &[], Some(&sample), &[vec![1]], &[], 2, 0, &mut Rng(1), &mut Counts::default()).unwrap();
        assert_eq!(strings(&b, 1), ["Ann", "ANN 2"]);

        // Many rows from a few AI rows: numbering goes on from the last
        // variant instead of trying 2, 3, 4… again, so it stays fast.
        let start = std::time::Instant::now();
        let mut counts = Counts::default();
        let rows = 100_000;
        let b = generate(&columns, &sources, &[false; 2], &[], Some(&sample), &[vec![0, 1]], &[], rows, 0, &mut Rng(1), &mut counts).unwrap();
        assert_eq!(counts.repeats, 0);
        assert_eq!(strings(&b, 0).iter().map(|e| e.trim_end().to_lowercase()).collect::<HashSet<_>>().len(), rows);
        assert!(start.elapsed() < std::time::Duration::from_secs(20), "{:?}", start.elapsed());
    }

    #[test]
    fn a_primary_key_of_several_columns_counts_the_one_that_isnt_a_foreign_key() {
        let cols = vec![col("order_id", "integer", false, true), col("line_no", "integer", false, true), col("qty", "integer", true, false)];
        let fks = vec![ForeignKey { columns: vec!["order_id".into()], ref_schema: String::new(), ref_table: "orders".into(), ref_columns: vec!["id".into()] }];
        let (plan, gens) = plan_for(ConnectorKind::Postgres, "public", "lines", &cols, &fks, None);
        let p = Prepared { kind: ConnectorKind::Postgres, plan, gens, chosen: vec![0, 1, 2], unique: HashSet::new(), unique_groups: vec![], ddl: None };
        assert_eq!(p.key(), Some(1), "line_no counts; order_id takes the parent's keys");
        // A single-column key, or one of foreign keys only: the first.
        let (plan, gens) = plan_for(ConnectorKind::Postgres, "public", "t", &cols[..1], &fks, None);
        let p = Prepared { kind: ConnectorKind::Postgres, plan, gens, chosen: vec![0], unique: HashSet::new(), unique_groups: vec![], ddl: None };
        assert_eq!(p.key(), Some(0));
    }

    #[test]
    fn date_times_with_offsets_are_read_in_utc() {
        let utc = parse_ts("2024-05-01 03:00:00").unwrap();
        for s in [
            "2024-05-01 10:00:00+07",
            "2024-05-01 10:00:00+07:00",
            "2024-05-01T10:00:00+0700",
            "2024-05-01 10:00:00 +07:00",
            "2024-05-01 03:00:00Z",
            "2024-05-01 03:00:00+00:00",
            "2024-05-01 03:00:00 UTC",
            "2024-04-30 19:00:00-08",
            "2024-05-01 08:30:00+05:30",
        ] {
            assert_eq!(parse_ts(s), Some(utc), "{s}");
        }
        assert_eq!(parse_ts("2024-05-01 10:00:00.5+07"), Some(utc + 500_000));
        assert_eq!(parse_ts("2024-05-01"), parse_ts("2024-05-01 00:00:00"));
        assert_eq!(parse_ts("2024-05-01 10:00:00+7x"), None);
        // Not an offset, or not ASCII: read as is (no panic).
        for s in ["2024-05-01 10:00:00+1é1", "2024-05-01 10:00:00+é", "2024-05-01 10:00:00+12:3é", "2024-05-01 10:00:00-ééé"] {
            assert_eq!(parse_ts(s), None, "{s}");
            assert_eq!(cell_value(ColType::TimestampTz, s), Value::Null);
        }
        // Postgres local mean time: an offset with seconds.
        assert_eq!(parse_ts("1890-01-01 00:00:00+07:06:40"), parse_ts("1889-12-31 16:53:20"));
        // A column without a time zone takes no shifted value.
        assert_eq!(cell_value(ColType::Timestamp, "2024-05-01 03:00:00"), Value::Timestamp(utc));
        assert_eq!(cell_value(ColType::Timestamp, "2024-05-01 03:00:00+00:00"), Value::Timestamp(utc));
        assert_eq!(cell_value(ColType::Timestamp, "2024-05-01 10:00:00+07"), Value::Null);
        // Keys continue after the largest, offset or not; a timestamptz
        // parent key keeps its moment.
        let g = Gen::TimestampTz;
        assert_eq!(key_start(&g, &Limits::Any, Some("2024-05-01 10:00:00+07"), 1, "t", "timestamptz"), Ok(utc / 1_000_000 + 1));
        assert_eq!(cell_value(ColType::TimestampTz, "2024-05-01 10:00:00+07"), Value::TimestampTz(utc));
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
        assert_eq!(
            parent_keys_sql(ConnectorKind::Postgres, "\"p\"", &["id"], 10),
            "SELECT DISTINCT \"id\" FROM \"p\" WHERE \"id\" IS NOT NULL ORDER BY \"id\" LIMIT 10"
        );
        assert_eq!(
            parent_keys_sql(ConnectorKind::Mssql, "[p]", &["a", "b"], 10),
            "SELECT DISTINCT TOP 10 [a], [b] FROM [p] WHERE [a] IS NOT NULL AND [b] IS NOT NULL ORDER BY [a], [b]"
        );
        assert!(parent_keys_sql(ConnectorKind::Oracle, "\"P\"", &["ID"], 10).ends_with("ORDER BY \"ID\" FETCH FIRST 10 ROWS ONLY"));
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
        let a = generate(&columns, &mk(), &nullable, &parents, None, &[], &[], 10_000, 50, &mut Rng(7), &mut Counts::default()).unwrap();
        let b = generate(&columns, &mk(), &nullable, &parents, None, &[], &[], 10_000, 50, &mut Rng(7), &mut Counts::default()).unwrap();
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
