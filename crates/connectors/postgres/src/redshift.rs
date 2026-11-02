//! Amazon Redshift through the Postgres connector. Redshift speaks the
//! Postgres protocol but its catalog is PostgreSQL 8.0's (no partitions,
//! `prokind`, arrays, `relispartition`, …) with Redshift additions: distribution
//! and sort keys in `pg_attribute`, `svv_table_info`, `SHOW TABLE` for DDL.
//! Metadata is read with plain text queries (no bound parameters).

use databrain_connector_core::{
    ColumnInfo, DbObject, ForeignKey, ObjectDetail, ObjectKind, Result, TableColumns, TableLayout, query_text, quote_literal, split_key_list,
};

use super::{PgSession, quote};

/// `PRIMARY KEY (a, "b c")` / `FOREIGN KEY (a) REFERENCES s.t(b)` → the column lists (and target).
pub(crate) fn parse_key_columns(def: &str) -> Vec<String> {
    let Some(open) = def.find('(') else { return vec![] };
    let Some(close) = def[open..].find(')') else { return vec![] };
    split_key_list(&def[open + 1..open + close]).into_iter().map(|c| unquote(&c)).collect()
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    match s.strip_prefix('"').and_then(|x| x.strip_suffix('"')) {
        Some(inner) => inner.replace("\"\"", "\""),
        None => s.to_string(),
    }
}

/// `FOREIGN KEY (a, b) REFERENCES s.t(c, d)` → key (`default_schema` when unqualified).
pub(crate) fn parse_foreign_key(def: &str, default_schema: &str) -> Option<ForeignKey> {
    let i = def.to_ascii_uppercase().find("REFERENCES")?;
    let columns = parse_key_columns(&def[..i]);
    let rest = def[i + "REFERENCES".len()..].trim();
    let open = rest.find('(')?;
    let target = rest[..open].trim();
    let ref_columns = parse_key_columns(&rest[open..]);
    let parts = databrain_connector_core::split_path(databrain_connector_core::ConnectorKind::Postgres, target);
    let (ref_schema, ref_table) = match parts.as_slice() {
        [t] => (default_schema.to_string(), t.clone()),
        [.., s, t] => (s.clone(), t.clone()),
        _ => return None,
    };
    (!columns.is_empty() && columns.len() == ref_columns.len()).then_some(ForeignKey { columns, ref_schema, ref_table, ref_columns })
}

fn kind_of(relkind: Option<&str>) -> ObjectKind {
    match relkind {
        Some("v") => ObjectKind::View,
        _ => ObjectKind::Table,
    }
}

fn est(v: &Option<String>) -> Option<i64> {
    v.as_deref().and_then(|x| x.parse::<f64>().ok()).filter(|e| *e >= 0.0).map(|e| e as i64)
}

const NOT_SYSTEM: &str = "n.nspname !~ '^pg_' and n.nspname <> 'information_schema'";

impl PgSession {
    async fn rs_objects(&self, filter: &str) -> Result<Vec<DbObject>> {
        let rows = query_text(
            self,
            &format!(
                "select n.nspname::text, c.relname::text, c.relkind::text, d.description, c.reltuples::float8 \
                 from pg_catalog.pg_class c join pg_catalog.pg_namespace n on n.oid = c.relnamespace \
                 left join pg_catalog.pg_description d on d.objoid = c.oid and d.objsubid = 0 \
                 where c.relkind in ('r','v') and {filter}"
            ),
        )
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| DbObject {
                schema: r[0].clone().unwrap_or_default(),
                name: r[1].clone().unwrap_or_default(),
                kind: kind_of(r[2].as_deref()),
                comment: r[3].clone().filter(|c| !c.is_empty()),
                row_estimate: est(&r[4]),
            })
            .collect())
    }

    /// Stored procedures and functions of a schema (procedures from `pg_proc_info`, which only Redshift has).
    async fn rs_routines(&self, filter: &str) -> Result<Vec<DbObject>> {
        let with_kind = format!(
            "select distinct n.nspname::text, p.proname::text, p.prokind::text from pg_catalog.pg_proc_info p \
             join pg_catalog.pg_namespace n on n.oid = p.pronamespace where {NOT_SYSTEM} and {filter}"
        );
        let rows = match query_text(self, &with_kind).await {
            Ok(r) => r,
            Err(_) => {
                query_text(
                    self,
                    &format!(
                        "select distinct n.nspname::text, p.proname::text, 'f' from pg_catalog.pg_proc p \
                         join pg_catalog.pg_namespace n on n.oid = p.pronamespace where {NOT_SYSTEM} and {filter}"
                    ),
                )
                .await?
            }
        };
        Ok(rows
            .into_iter()
            .map(|r| DbObject {
                schema: r[0].clone().unwrap_or_default(),
                name: r[1].clone().unwrap_or_default(),
                kind: if r[2].as_deref() == Some("p") { ObjectKind::Procedure } else { ObjectKind::Function },
                comment: None,
                row_estimate: None,
            })
            .collect())
    }

    pub(crate) async fn rs_list_objects(&self, schema: &str, names: Option<&[String]>) -> Result<Vec<DbObject>> {
        let s = quote_literal(schema);
        let mut out = self.rs_objects(&format!("n.nspname = {s} order by c.relkind = 'v', c.relname")).await?;
        out.extend(self.rs_routines(&format!("n.nspname = {s}")).await?);
        if let Some(names) = names {
            out.retain(|o| names.iter().any(|n| n == &o.name));
        }
        Ok(out)
    }

    pub(crate) async fn rs_search_objects(&self, term: &str) -> Result<Vec<DbObject>> {
        self.rs_objects(&format!("{NOT_SYSTEM} and strpos(lower(c.relname), {}) > 0 limit 2000", quote_literal(term))).await
    }

    pub(crate) async fn rs_search_schema(&self, schema: &str, term: &str, limit: usize) -> Result<Vec<DbObject>> {
        let t = quote_literal(term);
        self.rs_objects(&format!(
            "n.nspname = {} and strpos(lower(c.relname), {t}) > 0 order by strpos(lower(c.relname), {t}) <> 1, length(c.relname), c.relname limit {}",
            quote_literal(schema),
            limit.clamp(1, 1000)
        ))
        .await
    }

    pub(crate) async fn rs_search_routines(&self, schema: Option<&str>, term: &str) -> Result<Vec<DbObject>> {
        let scope = match schema {
            Some(s) => format!("n.nspname = {}", quote_literal(s)),
            None => "n.nspname = current_schema()".into(),
        };
        self.rs_routines(&format!("{scope} and strpos(lower(p.proname), {}) > 0", quote_literal(term))).await
    }

    /// Columns (with the informational primary key) of one table, or of every table and view of the schema.
    async fn rs_columns(&self, schema: &str, table: Option<&str>) -> Result<Vec<(String, ColumnInfo)>> {
        let s = quote_literal(schema);
        let t = table.map(|t| format!(" and c.relname = {}", quote_literal(t))).unwrap_or_default();
        let rows = query_text(
            self,
            &format!(
                "select c.relname::text, a.attname::text, pg_catalog.format_type(a.atttypid, a.atttypmod), not a.attnotnull, \
                        ad.adsrc, cd.description \
                 from pg_catalog.pg_attribute a \
                 join pg_catalog.pg_class c on c.oid = a.attrelid \
                 join pg_catalog.pg_namespace n on n.oid = c.relnamespace \
                 left join pg_catalog.pg_attrdef ad on ad.adrelid = a.attrelid and ad.adnum = a.attnum \
                 left join pg_catalog.pg_description cd on cd.objoid = a.attrelid and cd.objsubid = a.attnum \
                 where n.nspname = {s}{t} and c.relkind in ('r','v') and a.attnum > 0 and not a.attisdropped \
                 order by c.relname, a.attnum"
            ),
        )
        .await?;
        let pks = query_text(
            self,
            &format!(
                "select c.relname::text, pg_catalog.pg_get_constraintdef(k.oid) from pg_catalog.pg_constraint k \
                 join pg_catalog.pg_class c on c.oid = k.conrelid join pg_catalog.pg_namespace n on n.oid = c.relnamespace \
                 where k.contype = 'p' and n.nspname = {s}{t}"
            ),
        )
        .await
        .unwrap_or_default();
        let pk = |table: &str, col: &str| {
            pks.iter().any(|r| r[0].as_deref() == Some(table) && r[1].as_deref().is_some_and(|d| parse_key_columns(d).iter().any(|c| c == col)))
        };
        Ok(rows
            .into_iter()
            .map(|r| {
                let table = r[0].clone().unwrap_or_default();
                let name = r[1].clone().unwrap_or_default();
                let is_primary_key = pk(&table, &name);
                (
                    table,
                    ColumnInfo {
                        name,
                        data_type: r[2].clone().unwrap_or_default(),
                        nullable: r[3].as_deref() != Some("false"),
                        is_primary_key,
                        default: r[4].clone(),
                        comment: r[5].clone().filter(|c| !c.is_empty()),
                    },
                )
            })
            .collect())
    }

    async fn rs_foreign_keys(&self, schema: &str, table: Option<&str>) -> Result<Vec<(String, ForeignKey)>> {
        let t = table.map(|t| format!(" and c.relname = {}", quote_literal(t))).unwrap_or_default();
        let rows = query_text(
            self,
            &format!(
                "select c.relname::text, pg_catalog.pg_get_constraintdef(k.oid) from pg_catalog.pg_constraint k \
                 join pg_catalog.pg_class c on c.oid = k.conrelid join pg_catalog.pg_namespace n on n.oid = c.relnamespace \
                 where k.contype = 'f' and n.nspname = {}{t}",
                quote_literal(schema)
            ),
        )
        .await
        .unwrap_or_default();
        Ok(rows.into_iter().filter_map(|r| Some((r[0].clone()?, parse_foreign_key(r[1].as_deref()?, schema)?))).collect())
    }

    pub(crate) async fn rs_describe(&self, schema: &str, name: &str) -> Result<ObjectDetail> {
        let obj = self
            .rs_objects(&format!("n.nspname = {} and c.relname = {}", quote_literal(schema), quote_literal(name)))
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| databrain_connector_core::ConnectorError::query(format!("object not found: {schema}.{name}")))?;
        let columns: Vec<ColumnInfo> = self.rs_columns(schema, Some(name)).await?.into_iter().map(|(_, c)| c).collect();
        let ddl = match self.rs_show(obj.kind, schema, name).await {
            Some(d) => Some(d),
            None if obj.kind == ObjectKind::Table => Some(super::table_ddl(schema, name, &columns)),
            None => None,
        };
        let foreign_keys = self.rs_foreign_keys(schema, Some(name)).await?.into_iter().map(|(_, f)| f).collect();
        Ok(ObjectDetail { object: obj, columns, ddl, foreign_keys })
    }

    /// `SHOW TABLE` / `SHOW VIEW` / `SHOW PROCEDURE`: the DDL as Redshift prints it.
    async fn rs_show(&self, kind: ObjectKind, schema: &str, name: &str) -> Option<String> {
        let what = match kind {
            ObjectKind::Table => "TABLE",
            ObjectKind::View | ObjectKind::MaterializedView => "VIEW",
            ObjectKind::Procedure => "PROCEDURE",
            _ => return None,
        };
        let rows = query_text(self, &format!("SHOW {what} {}.{}", quote(schema), quote(name))).await.ok()?;
        let text: Vec<String> = rows.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect();
        (!text.is_empty()).then(|| text.join("\n"))
    }

    pub(crate) async fn rs_object_ddl(&self, schema: &str, name: &str, kind: ObjectKind) -> Result<Option<String>> {
        if let Some(d) = self.rs_show(kind, schema, name).await {
            return Ok(Some(d));
        }
        if kind.is_relation() {
            return Ok(self.rs_describe(schema, name).await?.ddl);
        }
        Ok(None)
    }

    pub(crate) async fn rs_schema_columns(&self, schema: &str) -> Result<Vec<TableColumns>> {
        let mut out: Vec<TableColumns> = Vec::new();
        for (table, col) in self.rs_columns(schema, None).await? {
            match out.last_mut() {
                Some(t) if t.table == table => t.columns.push(col),
                _ => out.push(TableColumns { table, columns: vec![col], foreign_keys: vec![] }),
            }
        }
        for (table, fk) in self.rs_foreign_keys(schema, None).await? {
            if let Some(t) = out.iter_mut().find(|t| t.table == table) {
                t.foreign_keys.push(fk);
            }
        }
        Ok(out)
    }

    /// Distribution style and key, sort keys and row count (what makes Redshift queries fast).
    pub(crate) async fn rs_table_layout(&self, schema: &str, name: &str) -> Result<TableLayout> {
        let (s, t) = (quote_literal(schema), quote_literal(name));
        let keys = query_text(
            self,
            &format!(
                "select a.attname::text, a.attisdistkey, a.attsortkeyord from pg_catalog.pg_attribute a \
                 join pg_catalog.pg_class c on c.oid = a.attrelid join pg_catalog.pg_namespace n on n.oid = c.relnamespace \
                 where n.nspname = {s} and c.relname = {t} and a.attnum > 0 and (a.attisdistkey or a.attsortkeyord <> 0) \
                 order by abs(a.attsortkeyord)"
            ),
        )
        .await
        .unwrap_or_default();
        let mut l = TableLayout::default();
        let mut interleaved = false;
        for r in &keys {
            let col = r[0].clone().unwrap_or_default();
            let ord: i64 = r[2].as_deref().and_then(|x| x.parse().ok()).unwrap_or(0);
            if ord != 0 {
                interleaved |= ord < 0;
                l.cluster_by.push(col.clone());
            }
            if r[1].as_deref() == Some("true") {
                l.notes.push(format!("DISTKEY ({col})"));
            }
        }
        if interleaved {
            l.notes.push("INTERLEAVED SORTKEY".into());
        }
        if let Some(r) = query_text(self, &format!("select diststyle::text, tbl_rows::bigint from svv_table_info where \"schema\" = {s} and \"table\" = {t}"))
            .await
            .ok()
            .and_then(|v| v.into_iter().next())
        {
            if let Some(d) = r[0].clone().filter(|d| !d.is_empty() && !d.starts_with("KEY")) {
                l.notes.push(format!("DISTSTYLE {d}"));
            }
            l.row_estimate = r[1].as_deref().and_then(|x| x.parse().ok());
        }
        Ok(l)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_constraint_defs() {
        assert_eq!(parse_key_columns("PRIMARY KEY (id, \"Order No\")"), vec!["id", "Order No"]);
        let fk = parse_foreign_key("FOREIGN KEY (customer_id, region) REFERENCES sales.customers(id, region)", "public").unwrap();
        assert_eq!(
            (fk.columns, fk.ref_schema.as_str(), fk.ref_table.as_str(), fk.ref_columns),
            (vec!["customer_id".to_string(), "region".into()], "sales", "customers", vec!["id".to_string(), "region".into()])
        );
        let fk = parse_foreign_key("FOREIGN KEY (c) REFERENCES \"Cust\"(id)", "public").unwrap();
        assert_eq!((fk.ref_schema.as_str(), fk.ref_table.as_str()), ("public", "Cust"));
        assert!(parse_foreign_key("CHECK (x > 0)", "public").is_none());
    }
}
