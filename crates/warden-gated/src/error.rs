//! Error types for the `warden-gated` binary/library.

use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum GatedError {
    /// `warden-gated` must never create the database itself -- only
    /// `warden` does, via its migrations (ADR-0006: the gate is a read-only
    /// consumer). A missing file here means a misconfigured path or a
    /// `warden` that has never run, not something to paper over.
    #[error("database not found at {0} -- warden-gated never creates it, only warden does")]
    DatabaseNotFound(PathBuf),

    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error(transparent)]
    Core(#[from] warden_core::CoreError),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// A `post-receive` payload that isn't exactly `<old-sha> <new-sha>
    /// <ref-name>`, or whose ref doesn't match the gate's naming convention
    /// -- untrusted input at the process boundary, never silently dropped.
    #[error("malformed post-receive notification line: {0:?}")]
    MalformedPushNotification(String),

    #[error("git push to origin failed (exit {exit_code:?}): {stderr}\ncommand: {command}")]
    PushFailed {
        command: String,
        exit_code: Option<i32>,
        stderr: String,
    },

    #[error("git command `{command}` failed (exit {exit_code:?}): {stderr}")]
    GitCommandFailed {
        command: String,
        exit_code: Option<i32>,
        stderr: String,
    },
}

pub type Result<T> = std::result::Result<T, GatedError>;
