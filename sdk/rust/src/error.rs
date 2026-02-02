//! Error types for the AgentFS SDK.

use thiserror::Error;

/// The main error type for the AgentFS SDK.
#[derive(Debug, Error)]
pub enum Error {
    /// Database error from turso
    #[error("database error: {0}")]
    Database(#[from] turso::Error),

    /// IO error
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialization/deserialization error
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// System time error
    #[error("time error: {0}")]
    Time(#[from] std::time::SystemTimeError),

    /// Filesystem-specific error with errno semantics
    #[error(transparent)]
    Fs(#[from] crate::filesystem::FsError),

    /// Invalid agent ID format
    #[error("invalid agent ID '{0}': agent IDs must contain only alphanumeric characters, hyphens, and underscores")]
    InvalidAgentId(String),

    /// Agent not found
    #[error("agent '{id}' not found at '{path}'")]
    AgentNotFound { id: String, path: String },

    /// Invalid path encoding
    #[error("path '{0}' is not valid UTF-8")]
    InvalidUtf8Path(String),

    /// Base directory does not exist
    #[error("base directory does not exist: {0}")]
    BaseDirectoryNotFound(String),

    /// Path is not a directory
    #[error("path is not a directory: {0}")]
    NotADirectory(String),

    /// Tool call not found
    #[error("tool call not found")]
    ToolCallNotFound,

    /// Sync not enabled for this database
    #[error("sync is not enabled for this database")]
    SyncNotEnabled,

    /// Connection pool timeout - no connections available
    #[error("connection pool timeout: no connections available")]
    ConnectionPoolTimeout,

    /// Encryption not supported for this configuration
    #[error("encryption not supported: {0}")]
    EncryptionNotSupported(String),

    /// Invalid encryption key
    #[error("invalid encryption key: {0}")]
    InvalidEncryptionKey(String),

    /// Internal error (for unexpected conditions)
    #[error("{0}")]
    Internal(String),

    /// Schema version mismatch - database schema version doesn't match expected version
    #[error("schema version mismatch: database is version {found}, expected {expected}")]
    SchemaVersionMismatch { found: String, expected: String },
}

/// Result type alias using the SDK Error type.
pub type Result<T> = std::result::Result<T, Error>;
