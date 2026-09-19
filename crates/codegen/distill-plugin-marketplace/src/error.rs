use thiserror::Error;

#[derive(Debug, Error)]
pub enum MarketplaceError {
    #[error("IO error at {path}: {source}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("JSON error: {0}")]
    Json(String),
    #[error("Git error: {0}")]
    Git(String),
    #[error("{0}")]
    Other(String),
}
