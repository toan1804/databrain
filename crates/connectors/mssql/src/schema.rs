//! Catalog queries, batched metadata and DDL reconstruction.

use crate::session::MssqlSession;
use databrain_connector_core::ColumnInfo;
use databrain_connector_core::ConnectorKind;
use databrain_connector_core::DbObject;
use databrain_connector_core::ForeignKey;
use databrain_connector_core::ObjectKind;
use databrain_connector_core::Result;
use databrain_connector_core::quote_ident;

/// `[db]` quoted for T-SQL.
pub(crate) fn qname(n: &str) -> String {
    format!("[{}]", n.replace(']', "]]"))
}

/// Run `sql` (with `@P1..@Pn` parameters) in database `db`'s context, so
/// `sys.*`, `schema_name()` and `object_id()` refer to that database.
pub(crate) fn in_db(db: Option<&str>, sql: &str, params: usize) -> String {
    let Some(db) = db else { return sql.to_string() };
    let mut out = format!("exec {}.sys.sp_executesql N'{}'", qname(db), sql.replace('\'', "''"));
    if params > 0 {
        let decl = (1..=params).map(|i| format!("@P{i} nvarchar(max)")).collect::<Vec<_>>().join(", ");
        let pass = (1..=params).map(|i| format!("@P{i} = @P{i}")).collect::<Vec<_>>().join(", ");
        out.push_str(&format!(", N'{decl}', {pass}"));
    }
    out
}

/// One batch running `sql` in each database (a database that fails, e.g. no
/// access, is skipped). `db_name()` in `sql` gives the database.
pub(crate) fn each_db(dbs: &[String], sql: &str) -> String {
    dbs.iter().map(|d| format!("begin try {} end try begin catch end catch;", in_db(Some(d), sql, 0))).collect::<Vec<_>>().join("\n")
}

impl MssqlSession {
    pub(crate) async fn bulk_metadata_batched(&self, schemas: &[String]) -> Result<Vec<databrain_connector_core::SchemaMetadata>> {
        if schemas.is_empty() {
            return Ok(vec![]);
        }
        // One database at a time (all-databases mode), each with three queries.
        let mut by_db: Vec<(Option<String>, Vec<String>)> = Vec::new();
        for id in schemas {
            let (db, sch) = self.split(id);
            let db = db.map(str::to_string);
            match by_db.iter_mut().find(|(d, _)| *d == db) {
                Some((_, l)) => l.push(sch.to_string()),
                None => by_db.push((db, vec![sch.to_string()])),
            }
        }
        let mut out = Vec::new();
        for (db, plain) in by_db {
            out.extend(self.bulk_metadata_db(db.as_deref(), &plain).await?);
        }
        Ok(out)
    }

    /// `bulk_metadata` of plain schema names of one database; results use schema ids.
    pub(crate) async fn bulk_metadata_db(&self, db: Option<&str>, schemas: &[String]) -> Result<Vec<databrain_connector_core::SchemaMetadata>> {
        let list = schemas.iter().map(|s| format!("N{}", databrain_connector_core::quote_literal(s))).collect::<Vec<_>>().join(", ");
        let s = |r: &tiberius::Row, i: usize| r.get::<&str, _>(i).map(str::to_string);
        let id = |r: &tiberius::Row, i: usize| self.id(db, &s(r, i).unwrap_or_default());
        let objects = self
            .rows_dyn_in(
                db,
                format!(
                    "select schema_name(o.schema_id), o.name, rtrim(o.type), cast(ep.value as nvarchar(4000)), \
                   (select sum(p.rows) from sys.partitions p where p.object_id = o.object_id and p.index_id in (0,1)) \
                 from sys.objects o \
                 left join sys.extended_properties ep on ep.class = 1 and ep.major_id = o.object_id and ep.minor_id = 0 and ep.name = 'MS_Description' \
                 where schema_name(o.schema_id) in ({list}) and o.type in ('U','V') and o.is_ms_shipped = 0 order by 1, 2"
                ),
            )
            .await?
            .iter()
            .map(|r| DbObject {
                schema: id(r, 0),
                name: s(r, 1).unwrap_or_default(),
                kind: if r.get::<&str, _>(2) == Some("V") { ObjectKind::View } else { ObjectKind::Table },
                comment: s(r, 3),
                row_estimate: r.get::<i64, _>(4),
            })
            .collect();
        // Same columns as COLUMNS_SQL, primary keys joined once instead of a subquery per column.
        let columns = self
            .rows_dyn_in(db, format!(
                "select o.name, c.name, \
                   t.name + case when t.name in ('varchar','char','varbinary','binary') then '(' + case when c.max_length = -1 then 'max' else cast(c.max_length as varchar(10)) end + ')' \
                                 when t.name in ('nvarchar','nchar') then '(' + case when c.max_length = -1 then 'max' else cast(c.max_length / 2 as varchar(10)) end + ')' \
                                 when t.name in ('decimal','numeric') then '(' + cast(c.precision as varchar(10)) + ',' + cast(c.scale as varchar(10)) + ')' else '' end, \
                   c.is_nullable, cast(case when pk.column_id is not null then 1 else 0 end as bit), \
                   object_definition(c.default_object_id), cast(ep.value as nvarchar(4000)), schema_name(o.schema_id) \
                 from sys.columns c join sys.objects o on o.object_id = c.object_id join sys.types t on t.user_type_id = c.user_type_id \
                 left join (select ic.object_id, ic.column_id from sys.index_columns ic join sys.indexes i on i.object_id = ic.object_id and i.index_id = ic.index_id where i.is_primary_key = 1) pk \
                   on pk.object_id = c.object_id and pk.column_id = c.column_id \
                 left join sys.extended_properties ep on ep.class = 1 and ep.major_id = c.object_id and ep.minor_id = c.column_id and ep.name = 'MS_Description' \
                 where schema_name(o.schema_id) in ({list}) and o.type in ('U','V') \
                 order by schema_name(o.schema_id), o.name, c.column_id"
            ))
            .await?
            .iter()
            .map(|r| {
                let (table, col) = column_from(r);
                (id(r, 7), table, col)
            })
            .collect();
        let fk_rows = self
            .rows_dyn_in(
                db,
                format!(
                    "select schema_name(fk.schema_id), object_name(fk.parent_object_id), fk.name, pc.name, rs.name, rt.name, rc.name \
                 from sys.foreign_keys fk \
                 join sys.foreign_key_columns fkc on fkc.constraint_object_id = fk.object_id \
                 join sys.columns pc on pc.object_id = fkc.parent_object_id and pc.column_id = fkc.parent_column_id \
                 join sys.tables rt on rt.object_id = fkc.referenced_object_id \
                 join sys.schemas rs on rs.schema_id = rt.schema_id \
                 join sys.columns rc on rc.object_id = fkc.referenced_object_id and rc.column_id = fkc.referenced_column_id \
                 where schema_name(fk.schema_id) in ({list}) \
                 order by 1, 2, 3, fkc.constraint_column_id"
                ),
            )
            .await?;
        let mut fks: Vec<(String, String, ForeignKey)> = Vec::new();
        let mut last: Option<(String, String, String)> = None;
        for r in &fk_rows {
            let g = |i: usize| s(r, i).unwrap_or_default();
            let key = (self.id(db, &g(0)), g(1), g(2));
            if last.as_ref() == Some(&key) {
                if let Some((_, _, fk)) = fks.last_mut() {
                    fk.columns.push(g(3));
                    fk.ref_columns.push(g(6));
                }
            } else {
                fks.push((
                    key.0.clone(),
                    key.1.clone(),
                    ForeignKey { columns: vec![g(3)], ref_schema: self.id(db, &g(4)), ref_table: g(5), ref_columns: vec![g(6)] },
                ));
                last = Some(key);
            }
        }
        let ids: Vec<String> = schemas.iter().map(|x| self.id(db, x)).collect();
        Ok(databrain_connector_core::assemble_metadata(&ids, objects, columns, fks))
    }

    pub(crate) async fn foreign_keys(&self, schema_id: &str, table: Option<&str>) -> Result<Vec<(String, ForeignKey)>> {
        let (db, schema) = self.split(schema_id);
        let rows = self
            .rows_in(
                db,
                "select object_name(fk.parent_object_id), fk.name, pc.name, rs.name, rt.name, rc.name \
                 from sys.foreign_keys fk \
                 join sys.foreign_key_columns fkc on fkc.constraint_object_id = fk.object_id \
                 join sys.columns pc on pc.object_id = fkc.parent_object_id and pc.column_id = fkc.parent_column_id \
                 join sys.tables rt on rt.object_id = fkc.referenced_object_id \
                 join sys.schemas rs on rs.schema_id = rt.schema_id \
                 join sys.columns rc on rc.object_id = fkc.referenced_object_id and rc.column_id = fkc.referenced_column_id \
                 where schema_name(fk.schema_id) = @P1 and (@P2 = '' or object_name(fk.parent_object_id) = @P2) \
                 order by 1, fk.name, fkc.constraint_column_id",
                vec![schema.into(), table.unwrap_or("").into()],
            )
            .await?;
        let mut out: Vec<(String, String, ForeignKey)> = Vec::new();
        for r in rows {
            let s = |i: usize| r.get::<&str, _>(i).unwrap_or_default().to_string();
            let (t, name) = (s(0), s(1));
            match out.iter_mut().find(|(tt, n, _)| *tt == t && *n == name) {
                Some((_, _, fk)) => {
                    fk.columns.push(s(2));
                    fk.ref_columns.push(s(5));
                }
                None => out.push((t, name, ForeignKey { columns: vec![s(2)], ref_schema: self.id(db, &s(3)), ref_table: s(4), ref_columns: vec![s(5)] })),
            }
        }
        Ok(out.into_iter().map(|(t, _, f)| (t, f)).collect())
    }
}

pub(crate) const EXPLORER_FINGERPRINTS_SQL: &str = "select s.name, concat( \
                   (select count(*) from sys.objects o where o.schema_id = s.schema_id and o.type in ('U','V','P','FN','IF','TF','SO') and o.is_ms_shipped = 0), '/', \
                   (select checksum_agg(checksum(o.object_id, o.modify_date)) from sys.objects o \
                      where o.schema_id = s.schema_id and o.type in ('U','V','P','FN','IF','TF','SO') and o.is_ms_shipped = 0), '/', \
                   (select checksum_agg(checksum(ep.major_id, ep.minor_id, cast(ep.value as nvarchar(4000)))) from sys.extended_properties ep \
                      join sys.objects o on o.object_id = ep.major_id where ep.class = 1 and ep.name = 'MS_Description' and o.schema_id = s.schema_id)) \
                 from sys.schemas s";

/// Schemas of the current database, `(name, is the user's default)`.
pub(crate) const SCHEMAS_SQL: &str = "select s.name, cast(case when s.name = schema_name() then 1 else 0 end as bit) from sys.schemas s \
    where s.name not like 'db[_]%' and s.name not in ('sys','INFORMATION_SCHEMA','guest') \
    order by case when s.name = schema_name() then 0 else 1 end, s.name";
pub(crate) const SCHEMA_FILTER: &str = "s.name not like 'db[_]%' and s.name not in ('sys','INFORMATION_SCHEMA','guest')";

pub(crate) const COLUMNS_SQL: &str = "select o.name, c.name, \
    t.name + case when t.name in ('varchar','char','varbinary','binary') then '(' + case when c.max_length = -1 then 'max' else cast(c.max_length as varchar(10)) end + ')' \
                  when t.name in ('nvarchar','nchar') then '(' + case when c.max_length = -1 then 'max' else cast(c.max_length / 2 as varchar(10)) end + ')' \
                  when t.name in ('decimal','numeric') then '(' + cast(c.precision as varchar(10)) + ',' + cast(c.scale as varchar(10)) + ')' else '' end, \
    c.is_nullable, \
    cast(case when exists (select 1 from sys.index_columns ic join sys.indexes i on i.object_id = ic.object_id and i.index_id = ic.index_id \
         where i.is_primary_key = 1 and ic.object_id = c.object_id and ic.column_id = c.column_id) then 1 else 0 end as bit), \
    object_definition(c.default_object_id), cast(ep.value as nvarchar(4000)), \
    case when c.is_identity = 1 then N'IDENTITY(' + cast(cast(idc.seed_value as decimal(38,0)) as nvarchar(40)) + N',' \
                                      + cast(cast(idc.increment_value as decimal(38,0)) as nvarchar(40)) + N')' \
         when c.is_computed = 1 then N'AS ' + cc.definition + case when cc.is_persisted = 1 then N' PERSISTED' else N'' end end \
  from sys.columns c join sys.objects o on o.object_id = c.object_id join sys.types t on t.user_type_id = c.user_type_id \
  left join sys.extended_properties ep on ep.major_id = c.object_id and ep.minor_id = c.column_id and ep.name = 'MS_Description' \
  left join sys.identity_columns idc on idc.object_id = c.object_id and idc.column_id = c.column_id \
  left join sys.computed_columns cc on cc.object_id = c.object_id and cc.column_id = c.column_id \
  where schema_name(o.schema_id) = @P1 and o.type in ('U','V') and (@P2 = '' or o.name = @P2) \
  order by o.name, c.column_id";

pub(crate) fn column_from(r: &tiberius::Row) -> (String, ColumnInfo) {
    let s = |i: usize| r.get::<&str, _>(i).map(str::to_string);
    (
        s(0).unwrap_or_default(),
        ColumnInfo {
            name: s(1).unwrap_or_default(),
            data_type: s(2).unwrap_or_default(),
            nullable: r.get::<bool, _>(3).unwrap_or(true),
            is_primary_key: r.get::<bool, _>(4).unwrap_or(false),
            default: s(5),
            comment: s(6),
        },
    )
}

/// Keys, foreign keys and checks (inside CREATE TABLE) and other indexes
/// (after it) of the table `@P1`, as DDL lines.
pub(crate) const MSSQL_TABLE_EXTRAS: &str = "\
  select 0 as part, 'CONSTRAINT ' + quotename(k.name) + case when k.type = 'PK' then ' PRIMARY KEY ' else ' UNIQUE ' end \
       + case when i.type = 1 then 'CLUSTERED ' else 'NONCLUSTERED ' end + '(' \
       + (select string_agg(quotename(c.name) + case when ic.is_descending_key = 1 then ' DESC' else '' end, ', ') within group (order by ic.key_ordinal) \
          from sys.index_columns ic join sys.columns c on c.object_id = ic.object_id and c.column_id = ic.column_id \
          where ic.object_id = i.object_id and ic.index_id = i.index_id and ic.is_included_column = 0) + ')', \
       case k.type when 'PK' then 0 else 1 end as o1, k.name as o2 \
  from sys.key_constraints k join sys.indexes i on i.object_id = k.parent_object_id and i.index_id = k.unique_index_id \
  where k.parent_object_id = object_id(@P1) \
  union all \
  select 0, 'CONSTRAINT ' + quotename(f.name) + ' FOREIGN KEY (' \
       + (select string_agg(quotename(c.name), ', ') within group (order by fc.constraint_column_id) from sys.foreign_key_columns fc \
          join sys.columns c on c.object_id = fc.parent_object_id and c.column_id = fc.parent_column_id where fc.constraint_object_id = f.object_id) \
       + ') REFERENCES ' + quotename(schema_name(r.schema_id)) + '.' + quotename(r.name) + ' (' \
       + (select string_agg(quotename(c.name), ', ') within group (order by fc.constraint_column_id) from sys.foreign_key_columns fc \
          join sys.columns c on c.object_id = fc.referenced_object_id and c.column_id = fc.referenced_column_id where fc.constraint_object_id = f.object_id) + ')' \
       + case f.delete_referential_action when 1 then ' ON DELETE CASCADE' when 2 then ' ON DELETE SET NULL' when 3 then ' ON DELETE SET DEFAULT' else '' end \
       + case f.update_referential_action when 1 then ' ON UPDATE CASCADE' when 2 then ' ON UPDATE SET NULL' when 3 then ' ON UPDATE SET DEFAULT' else '' end, \
       2, f.name \
  from sys.foreign_keys f join sys.objects r on r.object_id = f.referenced_object_id where f.parent_object_id = object_id(@P1) \
  union all \
  select 0, 'CONSTRAINT ' + quotename(cc.name) + ' CHECK ' + cc.definition, 3, cc.name \
  from sys.check_constraints cc where cc.parent_object_id = object_id(@P1) \
  union all \
  select 1, 'CREATE ' + case when i.is_unique = 1 then 'UNIQUE ' else '' end + case when i.type = 1 then 'CLUSTERED ' else 'NONCLUSTERED ' end \
       + 'INDEX ' + quotename(i.name) + ' ON ' + @P1 + ' (' \
       + (select string_agg(quotename(c.name) + case when ic.is_descending_key = 1 then ' DESC' else '' end, ', ') within group (order by ic.key_ordinal) \
          from sys.index_columns ic join sys.columns c on c.object_id = ic.object_id and c.column_id = ic.column_id \
          where ic.object_id = i.object_id and ic.index_id = i.index_id and ic.is_included_column = 0) + ')' \
       + coalesce(' INCLUDE (' + (select string_agg(quotename(c.name), ', ') from sys.index_columns ic join sys.columns c \
          on c.object_id = ic.object_id and c.column_id = ic.column_id where ic.object_id = i.object_id and ic.index_id = i.index_id and ic.is_included_column = 1) + ')', '') \
       + coalesce(' WHERE ' + i.filter_definition, ''), \
       4, i.name \
  from sys.indexes i where i.object_id = object_id(@P1) and i.is_primary_key = 0 and i.is_unique_constraint = 0 and i.type in (1, 2) and i.is_hypothetical = 0 \
  order by 1, 3, 4";

/// How the database fills a column of a `COLUMNS_SQL` row: `IDENTITY(1,1)`
/// or `AS (<expression>) [PERSISTED]` (computed); `None` for other columns.
pub(crate) fn column_fill(r: &tiberius::Row) -> Option<String> {
    r.try_get::<&str, _>(7).ok().flatten().map(str::to_string)
}

/// One column of CREATE TABLE. Identity and computed columns say so, so
/// readers of the DDL (test data, schema compare) know the database fills them.
pub(crate) fn column_line(c: &ColumnInfo, fill: Option<&str>) -> String {
    let name = quote_ident(ConnectorKind::Mssql, &c.name);
    if let Some(computed) = fill.filter(|f| f.starts_with("AS ")) {
        return format!("    {name} {computed}");
    }
    let mut l = format!("    {name} {}", c.data_type);
    if let Some(identity) = fill {
        l.push(' ');
        l.push_str(identity);
    }
    if !c.nullable {
        l.push_str(" NOT NULL");
    }
    if let Some(d) = &c.default {
        l.push_str(" DEFAULT ");
        l.push_str(d);
    }
    l
}

pub(crate) fn table_ddl(full: &str, columns: &[ColumnInfo], fills: &[Option<String>]) -> String {
    let mut lines: Vec<String> = columns.iter().enumerate().map(|(i, c)| column_line(c, fills.get(i).and_then(|f| f.as_deref()))).collect();
    let pk: Vec<String> = columns.iter().filter(|c| c.is_primary_key).map(|c| quote_ident(ConnectorKind::Mssql, &c.name)).collect();
    if !pk.is_empty() {
        lines.push(format!("    PRIMARY KEY ({})", pk.join(", ")));
    }
    format!("CREATE TABLE {full} (\n{}\n);", lines.join(",\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tiberius::Config;
    use tokio::sync::Mutex;

    #[test]
    fn catalog_queries_run_in_the_schemas_database() {
        assert_eq!(in_db(None, "select 1", 0), "select 1");
        assert_eq!(
            in_db(Some("Sales]DB"), "select N'x' where a = @P1", 1),
            "exec [Sales]]DB].sys.sp_executesql N'select N''x'' where a = @P1', N'@P1 nvarchar(max)', @P1 = @P1"
        );
        let b = each_db(&["a".into(), "b".into()], "select db_name()");
        assert_eq!(b.matches("begin try exec").count(), 2);
        assert!(b.contains("[b].sys.sp_executesql"));
        let s = MssqlSession {
            config: Config::new(),
            use_browser: false,
            conn: Arc::new(Mutex::new(None)),
            state: Default::default(),
            user_rowcount: Default::default(),
            all_dbs: true,
        };
        assert_eq!(s.split("shop.dbo"), (Some("shop"), "dbo"));
        assert_eq!(s.id(Some("shop"), "dbo"), "shop.dbo");
        let two = MssqlSession { all_dbs: false, ..s };
        assert_eq!(two.split("dbo"), (None, "dbo"));
        assert_eq!(two.id(Some("shop"), "dbo"), "dbo");
    }

    #[test]
    fn identity_and_computed_columns_show_in_the_ddl() {
        let col = |name: &str, t: &str, nullable: bool, pk: bool| ColumnInfo {
            name: name.into(),
            data_type: t.into(),
            nullable,
            is_primary_key: pk,
            default: None,
            comment: None,
        };
        let cols = [col("id", "int", false, true), col("total", "decimal(12,2)", true, false), col("name", "nvarchar(40)", false, false)];
        let fills = [Some("IDENTITY(1,1)".to_string()), Some("AS ([qty]*[price]) PERSISTED".to_string()), None];
        let ddl = table_ddl("[dbo].[t]", &cols, &fills);
        assert!(ddl.contains("    [id] int IDENTITY(1,1) NOT NULL,"), "{ddl}");
        assert!(ddl.contains("    [total] AS ([qty]*[price]) PERSISTED,"), "{ddl}");
        assert!(ddl.contains("    [name] nvarchar(40) NOT NULL,"), "{ddl}");
        assert!(COLUMNS_SQL.contains("is_identity") && COLUMNS_SQL.contains("is_computed"));
    }
}
