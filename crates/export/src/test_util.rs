//! Fixtures shared by the exporters' tests.

pub(crate) use std::sync::Arc;

pub(crate) use databrain_connector_core::arrow::array::{BinaryArray, BooleanArray, Int64Array, RecordBatch, StringArray};
pub(crate) use databrain_connector_core::arrow::datatypes::{DataType, Field, Schema, SchemaRef};

use crate::*;

pub(crate) fn batch() -> RecordBatch {
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, true),
        Field::new("name", DataType::Utf8, true),
        Field::new("ok", DataType::Boolean, true),
        Field::new("raw", DataType::Binary, true),
    ]));
    RecordBatch::try_new(
        schema,
        vec![
            Arc::new(Int64Array::from(vec![Some(1), None])),
            Arc::new(StringArray::from(vec![Some("a,\"b\""), Some("it's|x")])),
            Arc::new(BooleanArray::from(vec![Some(true), None])),
            Arc::new(BinaryArray::from(vec![Some(&b"\x00\xff"[..]), None])),
        ],
    )
    .unwrap()
}

pub(crate) fn run(format: ExportFormat) -> String {
    let b = batch();
    let mut o = ExportOptions::new(format);
    o.table_name = Some("public.t".into());
    export_to_string(b.schema(), [&b], o).unwrap()
}
