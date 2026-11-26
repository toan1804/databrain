//! Catalog queries: batched metadata, columns, foreign keys and DDL.

use crate::ddl;
use crate::session::OracleSession;
use databrain_connector_core::ColumnInfo;
use databrain_connector_core::DbObject;
use databrain_connector_core::ForeignKey;
use databrain_connector_core::ObjectKind;
use databrain_connector_core::Result;

/// Rows per round trip for catalog queries (the driver default is 100).
pub(crate) const METADATA_FETCH: u32 = 2000;

/// `'A', 'B'` for an `in (…)` list of owners.
pub(crate) fn owner_list(owners: &[String]) -> String {
    owners.iter().map(|o| databrain_connector_core::quote_literal(o)).collect::<Vec<_>>().join(", ")
}

impl OracleSession {
    /// Three dictionary queries for the whole batch of owners, with primary
    /// keys joined once (not looked up per column) and large fetches.
    pub(crate) async fn bulk_metadata_batched(&self, schemas: &[String]) -> Result<Vec<databrain_connector_core::SchemaMetadata>> {
        if schemas.is_empty() {
            return Ok(vec![]);
        }
        let owners = owner_list(schemas);
        let obj_rows = self
            .query_dynamic(format!(
                "select o.owner, o.object_name, o.object_type, c.comments, to_char(t.num_rows) \
                 from all_objects o \
                 left join all_tab_comments c on c.owner = o.owner and c.table_name = o.object_name \
                 left join all_tables t on t.owner = o.owner and t.table_name = o.object_name \
                 where o.owner in ({owners}) and o.object_type in ('TABLE','VIEW','MATERIALIZED VIEW') and o.object_name not like 'BIN$%' \
                 order by o.owner, o.object_name, case o.object_type when 'MATERIALIZED VIEW' then 0 else 1 end"
            ))
            .await?;
        let mut objects: Vec<DbObject> = Vec::with_capacity(obj_rows.len());
        for r in obj_rows {
            let g = |i: usize| r.get(i).cloned().flatten();
            let (Some(owner), Some(name)) = (g(0), g(1)) else { continue };
            // A materialized view is listed twice (also as TABLE): keep the first.
            if objects.last().is_some_and(|o| o.schema == owner && o.name == name) {
                continue;
            }
            let kind = match g(2).as_deref() {
                Some("VIEW") => ObjectKind::View,
                Some("MATERIALIZED VIEW") => ObjectKind::MaterializedView,
                _ => ObjectKind::Table,
            };
            objects.push(DbObject { schema: owner, name, kind, comment: g(3), row_estimate: g(4).and_then(|n| n.parse().ok()) });
        }
        let col_rows = self
            .query_dynamic(format!(
                "with pk as ( \
                   select cc.owner, cc.table_name, cc.column_name from all_constraints k \
                   join all_cons_columns cc on cc.owner = k.owner and cc.constraint_name = k.constraint_name \
                   where k.constraint_type = 'P' and k.owner in ({owners})) \
                 select c.owner, c.table_name, c.column_name, {}, \
                   c.nullable, case when pk.column_name is not null then 'Y' else 'N' end, cm.comments \
                 from all_tab_columns c \
                 left join pk on pk.owner = c.owner and pk.table_name = c.table_name and pk.column_name = c.column_name \
                 left join all_col_comments cm on cm.owner = c.owner and cm.table_name = c.table_name and cm.column_name = c.column_name \
                 where c.owner in ({owners}) and c.table_name not like 'BIN$%' \
                 order by c.owner, c.table_name, c.column_id",
                column_type_sql!()
            ))
            .await?;
        let columns = col_rows
            .into_iter()
            .map(|r| {
                let g = |i: usize| r.get(i).cloned().flatten();
                (
                    g(0).unwrap_or_default(),
                    g(1).unwrap_or_default(),
                    ColumnInfo {
                        name: g(2).unwrap_or_default(),
                        data_type: g(3).unwrap_or_default(),
                        nullable: g(4).as_deref() != Some("N"),
                        is_primary_key: g(5).as_deref() == Some("Y"),
                        default: None,
                        comment: g(6),
                    },
                )
            })
            .collect();
        let fk_rows = self
            .query_dynamic(format!(
                "select a.owner, a.table_name, a.constraint_name, a.column_name, r.owner, r.table_name, r.column_name \
                 from all_constraints k \
                 join all_cons_columns a on a.owner = k.owner and a.constraint_name = k.constraint_name \
                 join all_cons_columns r on r.owner = k.r_owner and r.constraint_name = k.r_constraint_name and r.position = a.position \
                 where k.constraint_type = 'R' and k.owner in ({owners}) \
                 order by a.owner, a.table_name, a.constraint_name, a.position"
            ))
            .await?;
        // Rows of one constraint are adjacent (ordered by owner, table, name).
        let mut fks: Vec<(String, String, ForeignKey)> = Vec::new();
        let mut last: Option<(String, String, String)> = None;
        for r in fk_rows {
            let g = |i: usize| r.get(i).cloned().flatten().unwrap_or_default();
            let key = (g(0), g(1), g(2));
            if last.as_ref() == Some(&key) {
                if let Some((_, _, fk)) = fks.last_mut() {
                    fk.columns.push(g(3));
                    fk.ref_columns.push(g(6));
                }
            } else {
                fks.push((key.0.clone(), key.1.clone(), ForeignKey { columns: vec![g(3)], ref_schema: g(4), ref_table: g(5), ref_columns: vec![g(6)] }));
                last = Some(key);
            }
        }
        Ok(databrain_connector_core::assemble_metadata(schemas, objects, columns, Some(fks)))
    }

    /// One table's columns: its columns and comments, then its primary key
    /// columns, each a statement Oracle plans quickly (the schema-wide
    /// statement, asked for one table, took four times longer to plan and
    /// read every column of the owner: 92 ms against 9 ms on a 150,000
    /// column schema).
    async fn table_columns(&self, owner: &str, table: &str) -> Result<Vec<(String, ColumnInfo)>> {
        let rows = self
            .query_named(
                concat!(
                    "select c.column_name, ",
                    column_type_sql!(),
                    ", c.nullable, cm.comments \
                 from all_tab_columns c \
                 left join all_col_comments cm on cm.owner = c.owner and cm.table_name = c.table_name and cm.column_name = c.column_name \
                 where c.owner = :o and c.table_name = :n \
                 order by c.column_id"
                ),
                binds(owner, table),
            )
            .await?;
        if rows.is_empty() {
            return Ok(vec![]);
        }
        let pk: std::collections::HashSet<String> = self
            .query_named(
                "select cc.column_name from all_constraints k \
                 join all_cons_columns cc on cc.owner = k.owner and cc.constraint_name = k.constraint_name and cc.table_name = k.table_name \
                 where k.owner = :o and k.table_name = :n and k.constraint_type = 'P'",
                binds(owner, table),
            )
            .await?
            .into_iter()
            .filter_map(|r| r.into_iter().next().flatten())
            .collect();
        Ok(rows
            .into_iter()
            .map(|r| {
                let g = |i: usize| r.get(i).cloned().flatten();
                let name = g(0).unwrap_or_default();
                (
                    table.to_string(),
                    ColumnInfo {
                        is_primary_key: pk.contains(&name),
                        name,
                        data_type: g(1).unwrap_or_default(),
                        nullable: g(2).as_deref() != Some("N"),
                        default: None,
                        comment: g(3),
                    },
                )
            })
            .collect())
    }

    /// Columns of one table, or of every table of `owner`.
    pub(crate) async fn columns(&self, owner: &str, table: Option<&str>) -> Result<Vec<(String, ColumnInfo)>> {
        if let Some(t) = table {
            return self.table_columns(owner, t).await;
        }
        let rows = self
            .query_named(
                concat!(
                    "select c.table_name, c.column_name, ",
                    column_type_sql!(),
                    ", c.nullable, \
                   case when exists (select 1 from all_cons_columns cc join all_constraints k \
                        on k.owner = cc.owner and k.constraint_name = cc.constraint_name \
                        where k.constraint_type = 'P' and cc.owner = c.owner and cc.table_name = c.table_name and cc.column_name = c.column_name) \
                        then 'Y' else 'N' end, \
                   cm.comments \
                 from all_tab_columns c \
                 left join all_col_comments cm on cm.owner = c.owner and cm.table_name = c.table_name and cm.column_name = c.column_name \
                 where c.owner = :owner \
                 order by c.table_name, c.column_id"
                ),
                vec![("owner", Some(owner.to_string()))],
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let g = |i: usize| r.get(i).cloned().flatten();
                (
                    g(0).unwrap_or_default(),
                    ColumnInfo {
                        name: g(1).unwrap_or_default(),
                        data_type: g(2).unwrap_or_default(),
                        nullable: g(3).as_deref() != Some("N"),
                        is_primary_key: g(4).as_deref() == Some("Y"),
                        default: None,
                        comment: g(5),
                    },
                )
            })
            .collect())
    }

    /// One table's foreign keys: their columns, then (when it has any) the
    /// columns they reference ([`ddl::REF_KEYS_SQL`], as the DDL reads
    /// them), never both ALL_CONS_COLUMNS joins in one statement.
    async fn table_foreign_keys(&self, owner: &str, table: &str) -> Result<Vec<(String, ForeignKey)>> {
        let own = self
            .query_named(
                "select k.constraint_name, a.column_name from all_constraints k \
                 join all_cons_columns a on a.owner = k.owner and a.constraint_name = k.constraint_name and a.table_name = k.table_name \
                 where k.owner = :o and k.table_name = :n and k.constraint_type = 'R' \
                 order by k.constraint_name, a.position",
                binds(owner, table),
            )
            .await?;
        if own.is_empty() {
            return Ok(vec![]);
        }
        let refs = self.query_named(ddl::REF_KEYS_SQL, binds(owner, table)).await?;
        Ok(join_foreign_keys(&own, &refs).into_iter().map(|f| (table.to_string(), f)).collect())
    }

    /// Foreign keys of one table, or of every table of `owner`.
    pub(crate) async fn foreign_keys(&self, owner: &str, table: Option<&str>) -> Result<Vec<(String, ForeignKey)>> {
        if let Some(t) = table {
            return self.table_foreign_keys(owner, t).await;
        }
        let rows = self
            .query_named(
                "select a.table_name, a.constraint_name, a.column_name, r.owner, r.table_name, r.column_name \
                 from all_constraints k \
                 join all_cons_columns a on a.owner = k.owner and a.constraint_name = k.constraint_name \
                 join all_cons_columns r on r.owner = k.r_owner and r.constraint_name = k.r_constraint_name and r.position = a.position \
                 where k.constraint_type = 'R' and k.owner = :owner \
                 order by a.table_name, a.constraint_name, a.position",
                vec![("owner", Some(owner.to_string()))],
            )
            .await?;
        let mut out: Vec<(String, String, ForeignKey)> = Vec::new();
        for r in rows {
            let g = |i: usize| r.get(i).cloned().flatten().unwrap_or_default();
            let (t, n) = (g(0), g(1));
            match out.iter_mut().find(|(tt, nn, _)| *tt == t && *nn == n) {
                Some((_, _, fk)) => {
                    fk.columns.push(g(2));
                    fk.ref_columns.push(g(5));
                }
                None => out.push((t, n, ForeignKey { columns: vec![g(2)], ref_schema: g(3), ref_table: g(4), ref_columns: vec![g(5)] })),
            }
        }
        Ok(out.into_iter().map(|(t, _, f)| (t, f)).collect())
    }

    /// The object's real kind (a materialized view is also a TABLE).
    pub(crate) async fn object_kind(&self, owner: &str, name: &str) -> Result<Option<ObjectKind>> {
        let rows = self.query_named(ddl::OBJECT_TYPE_SQL, binds(owner, name)).await?;
        Ok(rows.first().and_then(|r| r.first().cloned().flatten()).map(|t| kind_of(&t)))
    }

    /// A dictionary query for one object; `fallback` (older Oracle) when the
    /// first fails (unknown column/view). Optional parts pass `None` there
    /// and get no rows instead of an error.
    pub(crate) async fn dict(&self, sql: &'static str, fallback: Option<&'static str>, owner: &str, name: &str) -> Result<ddl::Rows> {
        match self.query_named(sql, binds(owner, name)).await {
            Ok(r) => Ok(r),
            Err(e) => match fallback {
                Some(f) => self.query_named(f, binds(owner, name)).await.map_err(|_| e),
                None => Err(e),
            },
        }
    }

    /// Optional part of the DDL: no rows when the view isn't there or not readable.
    pub(crate) async fn dict_opt(&self, sql: &'static str, fallback: Option<&'static str>, owner: &str, name: &str) -> ddl::Rows {
        self.dict(sql, fallback, owner, name).await.unwrap_or_default()
    }

    pub(crate) async fn dictionary_ddl(&self, owner: &str, name: &str, kind: ObjectKind) -> Result<Option<String>> {
        Ok(match kind {
            ObjectKind::View => {
                let v = self.dict(ddl::VIEW_SQL, None, owner, name).await?;
                let cols = self.dict_opt(ddl::VIEW_COLUMNS_SQL, None, owner, name).await;
                ddl::view_ddl(owner, name, &v, &cols)
            }
            ObjectKind::MaterializedView => {
                let mv = self.dict(ddl::MVIEW_SQL, Some(ddl::MVIEW_SQL_NO_COMMENTS), owner, name).await?;
                let idx = self.dict_opt(ddl::INDEXES_SQL, None, owner, name).await;
                let exprs = self.index_expressions(&idx, owner, name).await;
                ddl::mview_ddl(owner, name, &mv, &idx, &exprs)
            }
            ObjectKind::Procedure | ObjectKind::Function | ObjectKind::Package | ObjectKind::Other => {
                let (ty, body_ty) = match kind {
                    ObjectKind::Procedure => ("PROCEDURE", None),
                    ObjectKind::Function => ("FUNCTION", None),
                    ObjectKind::Package => ("PACKAGE", Some("PACKAGE BODY")),
                    _ => ("TYPE", Some("TYPE BODY")),
                };
                let src = |t: &'static str| {
                    let mut b = binds(owner, name);
                    b.push(("t", Some(t.to_string())));
                    self.query_named(ddl::SOURCE_SQL, b)
                };
                let spec = ddl::source_ddl(&src(ty).await?);
                if let Some(bt) = body_ty {
                    // The body needs EXECUTE plus ownership or CREATE ANY: spec alone otherwise.
                    let body = ddl::source_ddl(&src(bt).await.unwrap_or_default());
                    spec.map(|s| match body {
                        Some(b) => format!("{s}\n\n{b}"),
                        None => s,
                    })
                } else {
                    spec
                }
            }
            ObjectKind::Sequence => ddl::sequence_ddl(owner, name, &self.dict(ddl::SEQUENCE_SQL, None, owner, name).await?),
            _ => {
                let columns = self.dict(ddl::COLUMNS_SQL, Some(ddl::COLUMNS_SQL_11G), owner, name).await?;
                let table = self.dict_opt(ddl::TABLE_SQL, None, owner, name).await;
                let constraints = self.dict_opt(ddl::CONSTRAINTS_SQL, Some(ddl::CONSTRAINTS_SQL_12_1), owner, name).await;
                // What most tables don't have isn't asked for: a foreign
                // key's referenced columns, index expressions, partitions.
                let has_fk = constraints.iter().any(|r| r.get(1).cloned().flatten().as_deref() == Some("R"));
                let ref_keys = if has_fk { self.dict_opt(ddl::REF_KEYS_SQL, None, owner, name).await } else { vec![] };
                let indexes = self.dict_opt(ddl::INDEXES_SQL, None, owner, name).await;
                let index_expressions = self.index_expressions(&indexes, owner, name).await;
                let comments = self.dict_opt(ddl::COMMENTS_SQL, None, owner, name).await;
                // ALL_TABLES says whether it is partitioned; without its row
                // (not readable) the partitioning view is asked.
                let partitioned = table.first().is_none_or(|r| r.get(3).cloned().flatten().as_deref() != Some("NO"));
                let part = if partitioned { self.dict_opt(ddl::PART_SQL, Some(ddl::PART_SQL_10G), owner, name).await } else { vec![] };
                let (part_keys, first_partition) = if part.is_empty() {
                    (vec![], vec![])
                } else {
                    (self.dict_opt(ddl::PART_KEYS_SQL, None, owner, name).await, self.dict_opt(ddl::FIRST_PARTITION_SQL, None, owner, name).await)
                };
                let parts = ddl::TableParts { columns, table, constraints, ref_keys, indexes, index_expressions, comments, part, part_keys, first_partition };
                ddl::table_ddl(owner, name, &parts)
            }
        })
    }

    /// The expressions of the table's function-based index columns, when
    /// `indexes` (rows of [`ddl::INDEXES_SQL`]) has such an index.
    async fn index_expressions(&self, indexes: &ddl::Rows, owner: &str, name: &str) -> ddl::Rows {
        if ddl::has_function_based(indexes) { self.dict_opt(ddl::INDEX_EXPRESSIONS_SQL, None, owner, name).await } else { vec![] }
    }

    /// DBMS_METADATA.GET_DDL (own objects, or with SELECT_CATALOG_ROLE).
    pub(crate) async fn metadata_ddl(&self, owner: &str, name: &str, kind: ObjectKind) -> Option<String> {
        let ty = match kind {
            ObjectKind::View => "VIEW",
            ObjectKind::MaterializedView => "MATERIALIZED_VIEW",
            ObjectKind::Procedure => "PROCEDURE",
            ObjectKind::Function => "FUNCTION",
            ObjectKind::Package => "PACKAGE",
            ObjectKind::Sequence => "SEQUENCE",
            _ => "TABLE",
        };
        let mut b = binds(owner, name);
        b.push(("t", Some(ty.to_string())));
        let rows = self.query_named("select dbms_metadata.get_ddl(:t, :n, :o) from dual", b).await.ok()?;
        rows.into_iter().next().and_then(|r| r.into_iter().next().flatten()).map(|d| d.trim().to_string()).filter(|d| !d.is_empty())
    }
}

/// A table's foreign keys from its own key columns (`own`: constraint,
/// column, in key order) and what they reference (`refs`: rows of
/// [`ddl::REF_KEYS_SQL`], in key order). A key whose referenced columns
/// this user can't see is left out, as a join of the two would.
pub(crate) fn join_foreign_keys(own: &[Vec<Option<String>>], refs: &[Vec<Option<String>>]) -> Vec<ForeignKey> {
    let g = |r: &[Option<String>], i: usize| r.get(i).cloned().flatten().unwrap_or_default();
    let mut out: Vec<(String, ForeignKey)> = Vec::new();
    for r in own {
        let name = g(r, 0);
        match out.last_mut() {
            Some((n, fk)) if *n == name => fk.columns.push(g(r, 1)),
            _ => out.push((name, ForeignKey { columns: vec![g(r, 1)], ref_schema: String::new(), ref_table: String::new(), ref_columns: vec![] })),
        }
    }
    for (name, fk) in &mut out {
        for r in refs.iter().filter(|r| g(r, 0) == *name) {
            fk.ref_schema = g(r, 1);
            fk.ref_table = g(r, 2);
            fk.ref_columns.push(g(r, 3));
        }
    }
    out.into_iter().map(|(_, fk)| fk).filter(|fk| fk.ref_columns.len() == fk.columns.len()).collect()
}

pub(crate) fn binds(owner: &str, name: &str) -> Vec<(&'static str, Option<String>)> {
    vec![("o", Some(owner.to_string())), ("n", Some(name.to_string()))]
}

/// Names of the materialized views among catalog rows (`name_i`: the
/// object name, `type_i`: its `object_type`).
pub(crate) fn mview_names(rows: &[Vec<Option<String>>], name_i: usize, type_i: usize) -> std::collections::HashSet<String> {
    rows.iter().filter(|r| r.get(type_i).cloned().flatten().as_deref() == Some("MATERIALIZED VIEW")).filter_map(|r| r.get(name_i).cloned().flatten()).collect()
}

/// The TABLE row Oracle also lists for a materialized view (its container
/// table): left out, so the object is a materialized view, not a table.
pub(crate) fn is_mview_table(r: &[Option<String>], name_i: usize, type_i: usize, mviews: &std::collections::HashSet<String>) -> bool {
    r.get(type_i).cloned().flatten().as_deref() == Some("TABLE") && r.get(name_i).cloned().flatten().is_some_and(|n| mviews.contains(&n))
}

/// A column's type as the explorer shows it (`c`: ALL_TAB_COLUMNS):
/// lengths in characters for text, in bytes for RAW (whose `char_length`
/// is 0), precision and scale for NUMBER. A literal, for `concat!`.
macro_rules! column_type_sql {
    () => {
        "c.data_type || case when c.data_type in ('VARCHAR2','NVARCHAR2','CHAR','NCHAR') then '(' || c.char_length || ')' \
         when c.data_type = 'RAW' then '(' || c.data_length || ')' \
         when c.data_type = 'NUMBER' and c.data_precision is not null then '(' || c.data_precision || ',' || c.data_scale || ')' \
         else '' end"
    };
}
pub(crate) use column_type_sql;

pub(crate) fn kind_of(object_type: &str) -> ObjectKind {
    match object_type {
        "VIEW" => ObjectKind::View,
        "MATERIALIZED VIEW" => ObjectKind::MaterializedView,
        "PROCEDURE" => ObjectKind::Procedure,
        "FUNCTION" => ObjectKind::Function,
        "PACKAGE" => ObjectKind::Package,
        "SEQUENCE" => ObjectKind::Sequence,
        "TYPE" => ObjectKind::Other,
        _ => ObjectKind::Table,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, t: &str) -> Vec<Option<String>> {
        vec![Some(name.into()), Some(t.into())]
    }

    /// A materialized view is listed as one, not as the TABLE Oracle also
    /// lists under its name; a plain table stays.
    #[test]
    fn a_materialized_view_is_not_listed_as_its_table() {
        let rows = vec![row("SALES_MV", "TABLE"), row("ORDERS", "TABLE"), row("SALES_MV", "MATERIALIZED VIEW")];
        let mv = mview_names(&rows, 0, 1);
        let kept: Vec<(String, String)> =
            rows.iter().filter(|r| !is_mview_table(r, 0, 1, &mv)).map(|r| (r[0].clone().unwrap(), r[1].clone().unwrap())).collect();
        assert_eq!(kept, [("ORDERS".to_string(), "TABLE".to_string()), ("SALES_MV".to_string(), "MATERIALIZED VIEW".to_string())]);
    }

    /// A key's columns pair with the referenced ones in key order; a key
    /// whose target isn't visible is left out.
    #[test]
    fn foreign_keys_from_two_reads() {
        let row = |v: &[&str]| v.iter().map(|x| Some(x.to_string())).collect::<Vec<_>>();
        let own = vec![row(&["FK_A", "X1"]), row(&["FK_A", "X2"]), row(&["FK_B", "Y"]), row(&["FK_HIDDEN", "Z"])];
        let refs = vec![row(&["FK_B", "APP", "B", "ID"]), row(&["FK_A", "OTHER", "A", "K1"]), row(&["FK_A", "OTHER", "A", "K2"])];
        let fks = join_foreign_keys(&own, &refs);
        assert_eq!(fks.len(), 2);
        assert_eq!((fks[0].columns.clone(), fks[0].ref_schema.as_str(), fks[0].ref_table.as_str()), (vec!["X1".to_string(), "X2".to_string()], "OTHER", "A"));
        assert_eq!(fks[0].ref_columns, ["K1", "K2"]);
        assert_eq!((fks[1].columns.clone(), fks[1].ref_table.as_str(), fks[1].ref_columns.clone()), (vec!["Y".to_string()], "B", vec!["ID".to_string()]));
        assert!(join_foreign_keys(&[], &refs).is_empty());
    }

    /// RAW's length is its data_length (char_length is 0 for it).
    #[test]
    fn raw_length_is_in_bytes() {
        let sql = column_type_sql!();
        assert!(sql.contains("when c.data_type = 'RAW' then '(' || c.data_length || ')'"));
        assert!(!sql.contains("'NCHAR','RAW'"));
    }
}
