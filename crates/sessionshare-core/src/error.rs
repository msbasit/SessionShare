use thiserror::Error;

#[derive(Debug, Error)]
pub enum SessionShareError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("SQLite database error: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Sync error: {0}")]
    Sync(String),

    #[error("Watcher error: {0}")]
    Watch(String),
}

pub type Result<T> = std::result::Result<T, SessionShareError>;
