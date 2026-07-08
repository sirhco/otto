//! Error type for config parsing / loading.

use std::path::PathBuf;

/// Errors raised while reading, parsing, or loading configuration.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Failed to read a config file from disk.
    #[error("failed to read config file {path}: {source}")]
    Io {
        /// The file that could not be read.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// JSONC parse failure (comments / trailing-comma tolerant parse).
    #[error("failed to parse JSONC: {0}")]
    Jsonc(String),

    /// The parsed JSON did not deserialize into [`crate::Config`].
    #[error("failed to deserialize config: {0}")]
    Deserialize(#[from] serde_json::Error),
}

/// Convenience alias for results in this crate.
pub type Result<T> = std::result::Result<T, Error>;
