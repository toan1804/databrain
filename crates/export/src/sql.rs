//! SQL INSERT export: values are written by `crate::load::literal`, the
//! same rules as job loads (typed date literals where the dialect needs
//! them, NaN/±inf never as quoted text).

#[cfg(test)]
mod tests {
    use crate::test_util::*;
    use crate::*;

    #[test]
    fn sql_and_markdown() {
        let s = run(ExportFormat::SqlInsert);
        assert!(s.starts_with("INSERT INTO \"public\".\"t\" (\"id\", \"name\", \"ok\", \"raw\") VALUES (1, 'a,\"b\"', TRUE, '\\x00ff'::bytea);"), "{s}");
        assert!(s.contains("(NULL, 'it''s|x', NULL, NULL);"));
        let m = run(ExportFormat::Markdown);
        assert!(m.contains("| id | name | ok | raw |\n| ---: | --- | --- | --- |"));
        assert!(m.contains("it's\\|x"));
    }

    /// Dates and timestamps are written the way loads write them: typed
    /// literals for Oracle (text was ORA-01861), text elsewhere; NaN and
    /// ±inf never as quoted text.
    #[test]
    fn sql_insert_dates_and_non_finite_floats() {
        use databrain_connector_core::ConnectorKind as K;
        use databrain_connector_core::arrow::array::{Date32Array, Float64Array, TimestampMicrosecondArray};
        use databrain_connector_core::arrow::datatypes::TimeUnit;
        let schema: SchemaRef = Arc::new(Schema::new(vec![
            Field::new("d", DataType::Date32, true),
            Field::new("ts", DataType::Timestamp(TimeUnit::Microsecond, None), true),
            Field::new("tz", DataType::Timestamp(TimeUnit::Microsecond, Some("+02:00".into())), true),
            Field::new("x", DataType::Float64, true),
        ]));
        let b = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Date32Array::from(vec![20_455])),
                Arc::new(TimestampMicrosecondArray::from(vec![1_767_225_600_000_000])),
                Arc::new(TimestampMicrosecondArray::from(vec![1_767_225_600_000_000]).with_timezone("+02:00")),
                Arc::new(Float64Array::from(vec![f64::NAN])),
            ],
        )
        .unwrap();
        let sql = |k: K| {
            let mut o = ExportOptions::new(ExportFormat::SqlInsert);
            o.table_name = Some("t".into());
            o.dialect = Some(k);
            export_to_string(schema.clone(), [&b], o).unwrap()
        };
        assert_eq!(
            sql(K::Oracle),
            "INSERT INTO \"t\" (\"d\", \"ts\", \"tz\", \"x\") VALUES (DATE '2026-01-02', TIMESTAMP '2026-01-01 00:00:00', TIMESTAMP '2026-01-01 02:00:00 +02:00', NULL);\n"
        );
        assert_eq!(
            sql(K::Mssql),
            "INSERT INTO [t] ([d], [ts], [tz], [x]) VALUES (N'2026-01-02', N'2026-01-01 00:00:00', N'2026-01-01 02:00:00+02:00', NULL);\n"
        );
        assert!(sql(K::Postgres).ends_with("VALUES ('2026-01-02', '2026-01-01 00:00:00', '2026-01-01 02:00:00+02:00', 'NaN'::float8);\n"));
    }
}
