#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("arrow: {0}")]
    Arrow(#[from] databrain_connector_core::arrow::error::ArrowError),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("parquet: {0}")]
    Parquet(#[from] parquet::errors::ParquetError),
    #[error("xlsx: {0}")]
    Xlsx(#[from] rust_xlsxwriter::XlsxError),
    #[error("{0}")]
    Unsupported(String),
}

pub type Result<T> = std::result::Result<T, ExportError>;
