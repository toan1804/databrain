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
                 select c.owner, c.table_name, c.column_name, \
                   c.data_type || case when c.data_type in ('VARCHAR2','NVARCHAR2','CHAR','NCHAR','RAW') then '(' || c.char_length || ')' \
                                       when c.data_type = 'NUMBER' and c.data_precision is not null then '(' || c.data_precision || ',' || c.data_scale || ')' \
                                       else '' end, \
                   c.nullable, case when pk.column_name is not null then 'Y' else 'N' end, cm.comments \
                 from all_tab_columns c \
                 left join pk on pk.owner = c.owner and pk.table_name = c.table_name and pk.column_name = c.column_name \
                 left join all_col_comments cm on cm.owner = c.owner and cm.table_name = c.table_name and cm.column_name = c.column_name \
                 where c.owner in ({owners}) and c.table_name not like 'BIN$%' \
                 order by c.owner, c.table_name, c.column_id"
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

    pub(crate) async fn columns(&self, owner: &str, table: Option<&str>) -> Result<Vec<(String, ColumnInfo)>> {
        let rows = self
            .query_named(
                "select c.table_name, c.column_name, \
                   c.data_type || case when c.data_type in ('VARCHAR2','NVARCHAR2','CHAR','NCHAR','RAW') then '(' || c.char_length || ')' \
                                       when c.data_type = 'NUMBER' and c.data_precision is not null then '(' || c.data_precision || ',' || c.data_scale || ')' \
                                       else '' end, \
                   c.nullable, \
                   case when exists (select 1 from all_cons_columns cc join all_constraints k \
                        on k.owner = cc.owner and k.constraint_name = cc.constraint_name \
                        where k.constraint_type = 'P' and cc.owner = c.owner and cc.table_name = c.table_name and cc.column_name = c.column_name) \
                        then 'Y' else 'N' end, \
                   cm.comments \
                 from all_tab_columns c \
                 left join all_col_comments cm on cm.owner = c.owner and cm.table_name = c.table_name and cm.column_name = c.column_name \
                 where c.owner = :owner and (:tbl is null or c.table_name = :tbl) \
                 order by c.table_name, c.column_id",
                vec![("owner", Some(owner.to_string())), ("tbl", table.map(str::to_string))],
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

    pub(crate) async fn foreign_keys(&self, owner: &str, table: Option<&str>) -> Result<Vec<(String, ForeignKey)>> {
        let rows = self
            .query_named(
                "select a.table_name, a.constraint_name, a.column_name, r.owner, r.table_name, r.column_name \
                 from all_constraints k \
                 join all_cons_columns a on a.owner = k.owner and a.constraint_name = k.constraint_name \
                 join all_cons_columns r on r.owner = k.r_owner and r.constraint_name = k.r_constraint_name and r.position = a.position \
                 where k.constraint_type = 'R' and k.owner = :owner and (:tbl is null or k.table_name = :tbl) \
                 order by a.table_name, a.constraint_name, a.position",
                vec![("owner", Some(owner.to_string())), ("tbl", table.map(str::to_string))],
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
                ddl::mview_ddl(owner, name, &mv, &idx)
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
                let parts = ddl::TableParts {
                    columns: self.dict(ddl::COLUMNS_SQL, Some(ddl::COLUMNS_SQL_11G), owner, name).await?,
                    table: self.dict_opt(ddl::TABLE_SQL, None, owner, name).await,
                    constraints: self.dict_opt(ddl::CONSTRAINTS_SQL, None, owner, name).await,
                    indexes: self.dict_opt(ddl::INDEXES_SQL, None, owner, name).await,
                    col_comments: self.dict_opt(ddl::COL_COMMENTS_SQL, None, owner, name).await,
                    part: self.dict_opt(ddl::PART_SQL, Some(ddl::PART_SQL_10G), owner, name).await,
                    part_keys: vec![],
                    partitions: vec![],
                };
                let partitioned = !parts.part.is_empty();
                let parts = if partitioned {
                    ddl::TableParts {
                        part_keys: self.dict_opt(ddl::PART_KEYS_SQL, None, owner, name).await,
                        partitions: self.dict_opt(ddl::PARTITIONS_SQL, None, owner, name).await,
                        ..parts
                    }
                } else {
                    parts
                };
                ddl::table_ddl(owner, name, &parts)
            }
        })
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

pub(crate) fn binds(owner: &str, name: &str) -> Vec<(&'static str, Option<String>)> {
    vec![("o", Some(owner.to_string())), ("n", Some(name.to_string()))]
}

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
