//! Helpers connectors share for catalog work: small metadata queries read
//! as text, index and list parsing, and the default (listing-based)
//! implementations of the `Session` search and bulk-metadata methods.

use std::collections::HashMap;

use crate::*;

/// Run a small metadata query and return every cell as text (NULL → None;
/// lists as `[a, b]`). For catalog queries in `table_layout` implementations.
pub async fn query_text<S: Session + ?Sized>(s: &S, sql: &str) -> Result<Vec<Vec<Option<String>>>> {
    Ok(query_table(s, sql).await?.1)
}

/// [`query_text`] plus the column names.
#[allow(clippy::type_complexity)]
pub async fn query_table<S: Session + ?Sized>(s: &S, sql: &str) -> Result<(Vec<String>, Vec<Vec<Option<String>>>)> {
    use arrow::array::Array;
    use arrow::util::display::{ArrayFormatter, FormatOptions};
    let c = s.execute(sql, ExecOptions::default()).await?.collect().await?;
    let opts = FormatOptions::default();
    let mut rows = Vec::new();
    for b in &c.batches {
        let fmts: Vec<ArrayFormatter> = b
            .columns()
            .iter()
            .map(|a| ArrayFormatter::try_new(a.as_ref(), &opts).map_err(|e| ConnectorError::internal(e.to_string())))
            .collect::<Result<_>>()?;
        for i in 0..b.num_rows() {
            rows.push(b.columns().iter().zip(&fmts).map(|(a, f)| (!a.is_null(i)).then(|| f.value(i).to_string())).collect());
        }
    }
    let names = c.schema.map(|sc| sc.fields().iter().map(|f| f.name().clone()).collect()).unwrap_or_default();
    Ok((names, rows))
}

/// Group one-row-per-key-column index rows `(index, column, unique, primary,
/// method)` into indexes, keeping the first-seen order.
pub fn group_indexes(rows: impl IntoIterator<Item = (String, String, bool, bool, Option<String>)>) -> Vec<IndexInfo> {
    let mut out: Vec<IndexInfo> = Vec::new();
    for (name, col, unique, primary, method) in rows {
        match out.iter_mut().find(|i| i.name == name) {
            Some(i) => i.columns.push(col),
            None => out.push(IndexInfo { name, columns: vec![col], unique: unique || primary, primary, method }),
        }
    }
    out.sort_by_key(|i| !i.primary);
    out
}

/// `[a, b]` / `["a","b"]` / `a, b` → items (Databricks/DuckDB list text).
pub fn parse_list_text(s: &str) -> Vec<String> {
    let t = s.trim();
    let t = t.strip_prefix('[').and_then(|x| x.strip_suffix(']')).unwrap_or(t);
    split_key_list(t)
        .into_iter()
        .map(|x| {
            let x = x.trim();
            let x = x.strip_prefix('\'').and_then(|y| y.strip_suffix('\'')).unwrap_or(x);
            unquote_ident(x)
        })
        .filter(|x| !x.is_empty())
        .collect()
}

/// `RANGE (a, b)` / `LINEAR(a, b)` → ("range", ["a", "b"]); no parens → ("", [text]).
pub fn parse_call_list(s: &str) -> (String, Vec<String>) {
    let t = s.trim();
    match (t.find('('), t.rfind(')')) {
        (Some(o), Some(c)) if c > o => {
            let items = split_key_list(&t[o + 1..c]).into_iter().map(|x| unquote_ident(&x)).collect();
            (t[..o].trim().to_ascii_lowercase(), items)
        }
        _ if t.is_empty() => (String::new(), vec![]),
        _ => (String::new(), vec![unquote_ident(t)]),
    }
}

/// Truthy cell text from catalog queries (`t`, `true`, `1`, `YES`, `UNIQUE`).
pub fn truthy(v: &Option<String>) -> bool {
    matches!(v.as_deref().map(|x| x.trim().to_ascii_lowercase()).as_deref(), Some("t" | "true" | "1" | "yes" | "y" | "unique"))
}

/// Per-schema metadata (the default for [`Session::bulk_metadata`]).
/// Group flat catalog rows (from one multi-schema query each) into
/// [`SchemaMetadata`] per requested schema. Linear time, whatever the size.
/// `foreign_keys`: `None` when the engine's bulk read doesn't get them.
pub fn assemble_metadata(
    schemas: &[String],
    objects: Vec<DbObject>,
    columns: Vec<(String, String, ColumnInfo)>,
    foreign_keys: Option<Vec<(String, String, ForeignKey)>>,
) -> Vec<SchemaMetadata> {
    let foreign_keys_read = foreign_keys.is_some();
    let foreign_keys = foreign_keys.unwrap_or_default();
    let mut out: Vec<SchemaMetadata> =
        schemas.iter().map(|s| SchemaMetadata { schema: s.clone(), objects: vec![], columns: vec![], error: None, foreign_keys_read }).collect();
    let pos: HashMap<&str, usize> = schemas.iter().enumerate().map(|(i, s)| (s.as_str(), i)).collect();
    let mut tables: HashMap<(usize, String), usize> = HashMap::new();
    for o in objects {
        if let Some(&i) = pos.get(o.schema.as_str()) {
            out[i].objects.push(o);
        }
    }
    let mut cols: Vec<Vec<TableColumns>> = vec![Vec::new(); schemas.len()];
    for (schema, table, c) in columns {
        let Some(&i) = pos.get(schema.as_str()) else { continue };
        let k = (i, table);
        match tables.get(&k) {
            Some(&j) => cols[i][j].columns.push(c),
            None => {
                tables.insert(k.clone(), cols[i].len());
                cols[i].push(TableColumns { table: k.1, columns: vec![c], foreign_keys: vec![] });
            }
        }
    }
    for (schema, table, fk) in foreign_keys {
        let Some(&i) = pos.get(schema.as_str()) else { continue };
        if let Some(&j) = tables.get(&(i, table)) {
            cols[i][j].foreign_keys.push(fk);
        }
    }
    for (m, c) in out.iter_mut().zip(cols) {
        m.columns = c;
    }
    out
}

pub async fn default_bulk_metadata<S: Session + ?Sized>(s: &S, schemas: &[String]) -> Result<Vec<SchemaMetadata>> {
    let mut out = Vec::with_capacity(schemas.len());
    for sc in schemas {
        out.push(match s.list_objects(sc).await {
            Ok(objects) => {
                let (columns, error) = match s.schema_columns(sc).await {
                    Ok(c) => (c, None),
                    Err(e) => (vec![], Some(e.message)),
                };
                // `schema_columns` describes (keys included) unless the
                // engine reads columns its own way; those have no keys.
                SchemaMetadata { schema: sc.clone(), objects, columns, error, foreign_keys_read: true }
            }
            Err(e) => SchemaMetadata { schema: sc.clone(), objects: vec![], columns: vec![], error: Some(e.message), foreign_keys_read: false },
        });
    }
    Ok(out)
}

/// Catalog search by listing every schema (the default for
/// [`Session::search_objects`]); stops early once plenty of hits are found.
/// [`Session::search_schema`] by listing the schema and filtering.
pub async fn search_schema_by_listing<S: Session + ?Sized>(s: &S, schema: &str, query: &str, limit: usize) -> Result<Vec<DbObject>> {
    let q = query.trim().to_lowercase();
    let mut hits: Vec<DbObject> =
        s.list_objects(schema).await?.into_iter().filter(|o| o.kind.is_relation() && (q.is_empty() || o.name.to_lowercase().contains(&q))).collect();
    rank_matches(query, &mut hits, limit);
    Ok(hits)
}

/// [`Session::search_routines`] by listing one schema (the given one, else the default).
pub async fn search_routines_by_listing<S: Session + ?Sized>(s: &S, schema: Option<&str>, query: &str, limit: usize) -> Result<Vec<DbObject>> {
    let schema = match schema {
        Some(sc) => sc.to_string(),
        None => {
            let all = s.list_schemas().await?;
            match all.iter().find(|sc| sc.is_default).or(all.first()) {
                Some(sc) => sc.name.clone(),
                None => return Ok(vec![]),
            }
        }
    };
    let q = query.trim().to_lowercase();
    let mut hits: Vec<DbObject> =
        s.list_objects(&schema).await?.into_iter().filter(|o| o.kind.is_routine() && (q.is_empty() || o.name.to_lowercase().contains(&q))).collect();
    rank_matches(query, &mut hits, limit);
    Ok(hits)
}

pub async fn search_by_listing<S: Session + ?Sized>(s: &S, query: &str, limit: usize) -> Result<Vec<DbObject>> {
    let mut hits = Vec::new();
    for sc in s.list_schemas().await? {
        let Ok(objs) = s.list_objects(&sc.name).await else { continue };
        hits.extend(objs.into_iter().filter(|o| {
            !matches!(o.kind, ObjectKind::Function | ObjectKind::Procedure | ObjectKind::Package | ObjectKind::Sequence)
                && object_matches(query, &o.schema, &o.name)
        }));
        if hits.len() >= limit * 4 {
            break;
        }
    }
    rank_matches(query, &mut hits, limit);
    Ok(hits)
}
