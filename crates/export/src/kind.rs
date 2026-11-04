use databrain_connector_core::arrow::datatypes::DataType;
use databrain_result_store::display::is_decimal_text;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Number,
    Bool,
    Binary,
    /// Postgres array (`{1,2}` text): a JSON array, elements typed as `elem`.
    Array {
        elem: ArrayElem,
    },
    Other,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArrayElem {
    Number,
    Bool,
    Text,
}

pub(crate) fn kind_of(f: &databrain_connector_core::arrow::datatypes::Field) -> Kind {
    match f.data_type() {
        DataType::Boolean => Kind::Bool,
        t if t.is_numeric() => Kind::Number,
        DataType::Binary | DataType::LargeBinary | DataType::FixedSizeBinary(_) => Kind::Binary,
        _ if is_decimal_text(f) => Kind::Number,
        DataType::Utf8 | DataType::LargeUtf8 => match f.metadata().get(databrain_connector_core::value::META_DB_TYPE).and_then(|t| t.strip_suffix("[]")) {
            Some(base) => Kind::Array {
                elem: match base.split('(').next().unwrap_or("").trim() {
                    "smallint" | "integer" | "bigint" | "real" | "double precision" | "oid" => ArrayElem::Number,
                    b if databrain_result_store::display::is_decimal_type(b) => ArrayElem::Number,
                    "boolean" => ArrayElem::Bool,
                    _ => ArrayElem::Text,
                },
            },
            None => Kind::Other,
        },
        _ => Kind::Other,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn decimal_types_of_every_database_stay_numbers() {
        for t in ["numeric(12,2)", "decimal", "NUMBER(10,2)", "NUMBER", "BIGNUMERIC", "money", "DECIMAL(38,10)"] {
            assert!(databrain_result_store::display::is_decimal_type(t), "{t}");
        }
        for t in ["varchar(10)", "numbers", "text[]"] {
            assert!(!databrain_result_store::display::is_decimal_type(t), "{t}");
        }
    }
}
