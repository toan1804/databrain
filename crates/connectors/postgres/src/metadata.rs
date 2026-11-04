use databrain_connector_core::{ColumnInfo, DbObject, ForeignKey, ObjectKind, Result};

use crate::error::map_err;
use crate::session::PgSession;

impl PgSession {
    pub(crate) async fn list_objects_filtered(&self, schema: &str, names: Option<&[String]>) -> Result<Vec<DbObject>> {
        if self.redshift {
            return self.rs_list_objects(schema, names).await;
        }
        let names: Option<Vec<String>> = names.map(<[String]>::to_vec);
        let rows = self
            .client()
            .query(
                "select c.relname::text, c.relkind::text, \
                        obj_description(c.oid, 'pg_class'), c.reltuples::float8 \
                 from pg_catalog.pg_class c \
                 join pg_catalog.pg_namespace n on n.oid = c.relnamespace \
                 where n.nspname = $1 and c.relkind in ('r','p','v','m','f','S') and not c.relispartition \
                   and ($2::text[] is null or c.relname = any($2)) \
                 order by c.relkind in ('v','m'), c.relkind = 'S', c.relname",
                &[&schema, &names],
            )
            .await
            .map_err(|e| map_err(&e))?;
        let mut out: Vec<DbObject> = rows
            .iter()
            .map(|r| {
                let kind = match r.get::<_, String>(1).as_str() {
                    "v" => ObjectKind::View,
                    "m" => ObjectKind::MaterializedView,
                    "f" => ObjectKind::ForeignTable,
                    "S" => ObjectKind::Sequence,
                    _ => ObjectKind::Table,
                };
                let est: Option<f64> = r.get(3);
                let est = est.filter(|_| kind != ObjectKind::Sequence);
                DbObject { schema: schema.to_string(), name: r.get(0), kind, comment: r.get(2), row_estimate: est.filter(|e| *e >= 0.0).map(|e| e as i64) }
            })
            .collect();

        let funcs = self
            .client()
            .query(
                "select p.proname::text, p.prokind::text \
                 from pg_catalog.pg_proc p join pg_catalog.pg_namespace n on n.oid = p.pronamespace \
                 where n.nspname = $1 and p.prokind in ('f','p') \
                 and n.nspname not in ('pg_catalog','information_schema') \
                 and ($2::text[] is null or p.proname = any($2)) \
                 group by 1, 2 order by 1",
                &[&schema, &names],
            )
            .await
            .map_err(|e| map_err(&e))?;
        out.extend(funcs.iter().map(|r| DbObject {
            schema: schema.to_string(),
            name: r.get(0),
            kind: if r.get::<_, String>(1) == "p" { ObjectKind::Procedure } else { ObjectKind::Function },
            comment: None,
            row_estimate: None,
        }));
        Ok(out)
    }

    /// Three catalog queries for the whole batch of schemas (objects,
    /// columns, foreign keys), instead of three per schema.
    pub(crate) async fn bulk_metadata_batched(&self, schemas: &[String]) -> Result<Vec<databrain_connector_core::SchemaMetadata>> {
        let names: Vec<String> = schemas.to_vec();
        let objects = self
            .client()
            .query(
                "select n.nspname::text, c.relname::text, c.relkind::text, d.description, c.reltuples::float8 \
                 from pg_catalog.pg_class c \
                 join pg_catalog.pg_namespace n on n.oid = c.relnamespace \
                 left join pg_catalog.pg_description d on d.objoid = c.oid and d.classoid = 'pg_catalog.pg_class'::regclass and d.objsubid = 0 \
                 where n.nspname::text = any($1::text[]) and c.relkind in ('r','p','v','m','f') and not c.relispartition \
                 order by 1, 2",
                &[&names],
            )
            .await
            .map_err(|e| map_err(&e))?
            .iter()
            .map(|r| {
                let est: Option<f64> = r.get(4);
                DbObject {
                    schema: r.get(0),
                    name: r.get(1),
                    kind: match r.get::<_, String>(2).as_str() {
                        "v" => ObjectKind::View,
                        "m" => ObjectKind::MaterializedView,
                        "f" => ObjectKind::ForeignTable,
                        _ => ObjectKind::Table,
                    },
                    comment: r.get(3),
                    row_estimate: est.filter(|e| *e >= 0.0).map(|e| e as i64),
                }
            })
            .collect();
        let columns = self
            .client()
            .query(
                "select n.nspname::text, c.relname::text, a.attname::text, pg_catalog.format_type(a.atttypid, a.atttypmod), \
                        not a.attnotnull, coalesce(a.attnum = any(i.indkey), false), d.description \
                 from pg_catalog.pg_attribute a \
                 join pg_catalog.pg_class c on c.oid = a.attrelid \
                 join pg_catalog.pg_namespace n on n.oid = c.relnamespace \
                 left join pg_catalog.pg_index i on i.indrelid = a.attrelid and i.indisprimary \
                 left join pg_catalog.pg_description d on d.objoid = a.attrelid and d.classoid = 'pg_catalog.pg_class'::regclass and d.objsubid = a.attnum \
                 where n.nspname::text = any($1::text[]) and c.relkind in ('r','p','v','m','f') and not c.relispartition \
                   and a.attnum > 0 and not a.attisdropped \
                 order by 1, 2, a.attnum",
                &[&names],
            )
            .await
            .map_err(|e| map_err(&e))?
            .iter()
            .map(|r| {
                (
                    r.get::<_, String>(0),
                    r.get::<_, String>(1),
                    ColumnInfo { name: r.get(2), data_type: r.get(3), nullable: r.get(4), is_primary_key: r.get(5), default: None, comment: r.get(6) },
                )
            })
            .collect();
        let fks = self.foreign_keys_in(&names, None).await?;
        Ok(databrain_connector_core::assemble_metadata(schemas, objects, columns, fks))
    }

    /// Foreign keys of the tables of several schemas: (schema, table, key).
    pub(crate) async fn foreign_keys_in(&self, schemas: &[String], table: Option<&str>) -> Result<Vec<(String, String, ForeignKey)>> {
        let rows = self
            .client()
            .query(
                "select n.nspname::text, c.relname::text, \
                        array(select a.attname::text from unnest(k.conkey) with ordinality u(n, o) \
                              join pg_catalog.pg_attribute a on a.attrelid = k.conrelid and a.attnum = u.n order by u.o), \
                        rn.nspname::text, rc.relname::text, \
                        array(select a.attname::text from unnest(k.confkey) with ordinality u(n, o) \
                              join pg_catalog.pg_attribute a on a.attrelid = k.confrelid and a.attnum = u.n order by u.o) \
                 from pg_catalog.pg_constraint k \
                 join pg_catalog.pg_class c on c.oid = k.conrelid \
                 join pg_catalog.pg_namespace n on n.oid = c.relnamespace \
                 join pg_catalog.pg_class rc on rc.oid = k.confrelid \
                 join pg_catalog.pg_namespace rn on rn.oid = rc.relnamespace \
                 where k.contype = 'f' and n.nspname::text = any($1::text[]) and ($2::text is null or c.relname = $2)",
                &[&schemas, &table],
            )
            .await
            .map_err(|e| map_err(&e))?;
        Ok(rows
            .iter()
            .map(|r| {
                (
                    r.get::<_, String>(0),
                    r.get::<_, String>(1),
                    ForeignKey { columns: r.get(2), ref_schema: r.get(3), ref_table: r.get(4), ref_columns: r.get(5) },
                )
            })
            .collect())
    }

    /// Foreign keys of one table (or all tables of the schema).
    pub(crate) async fn foreign_keys(&self, schema: &str, table: Option<&str>) -> Result<Vec<(String, ForeignKey)>> {
        let rows = self
            .client()
            .query(
                "select c.relname::text, \
                        array(select a.attname::text from unnest(k.conkey) with ordinality u(n, o) \
                              join pg_catalog.pg_attribute a on a.attrelid = k.conrelid and a.attnum = u.n order by u.o), \
                        rn.nspname::text, rc.relname::text, \
                        array(select a.attname::text from unnest(k.confkey) with ordinality u(n, o) \
                              join pg_catalog.pg_attribute a on a.attrelid = k.confrelid and a.attnum = u.n order by u.o) \
                 from pg_catalog.pg_constraint k \
                 join pg_catalog.pg_class c on c.oid = k.conrelid \
                 join pg_catalog.pg_namespace n on n.oid = c.relnamespace \
                 join pg_catalog.pg_class rc on rc.oid = k.confrelid \
                 join pg_catalog.pg_namespace rn on rn.oid = rc.relnamespace \
                 where k.contype = 'f' and n.nspname = $1 and ($2::text is null or c.relname = $2)",
                &[&schema, &table],
            )
            .await
            .map_err(|e| map_err(&e))?;
        Ok(rows
            .iter()
            .map(|r| (r.get::<_, String>(0), ForeignKey { columns: r.get(1), ref_schema: r.get(2), ref_table: r.get(3), ref_columns: r.get(4) }))
            .collect())
    }
}
