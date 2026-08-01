//! Error types for the `warden-tui` binary/library.

use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum TuiError {
    /// `warden-tui` must never create the database itself -- only `warden` does, via its
    /// migrations.
    #[error("database not found at {0} -- warden-tui never creates it, only warden does")]
    DatabaseNotFound(PathBuf),

    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error(transparent)]
    Core(#[from] warden_core::CoreError),

    #[error("run {run_id} not found")]
    RunNotFound { run_id: String },

    #[error("row column `{column}` = {value} does not fit in the expected numeric type")]
    InvalidStoredValue { column: &'static str, value: i64 },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("failed to decode image {path}: {source}")]
    ImageDecode {
        path: PathBuf,
        #[source]
        source: image::ImageError,
    },

    #[error("failed to prepare image {path} for the terminal: {source}")]
    ImageProtocol {
        path: PathBuf,
        #[source]
        source: ratatui_image::errors::Errors,
    },

    /// Video frame extraction (`ffmpeg`) and asciinema sub-terminal playback are deliberately out
    /// of scope for this pass.
    #[error("{feature} is not yet implemented (deferred: {reason})")]
    NotYetImplemented {
        feature: &'static str,
        reason: &'static str,
    },
}

pub type Result<T> = std::result::Result<T, TuiError>;
