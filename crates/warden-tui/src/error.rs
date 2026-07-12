//! Error types for the `warden-tui` binary/library.

use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum TuiError {
    /// `warden-tui` must never create the database itself -- only `warden`
    /// does, via its migrations (mirrors `warden_gated::GatedError`, same
    /// "read-only consumer" boundary, ADR-0008). A missing file means a
    /// misconfigured path or a `warden` that has never run, never a case to
    /// paper over by creating an empty one.
    #[error("database not found at {0} -- warden-tui never creates it, only warden does")]
    DatabaseNotFound(PathBuf),

    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error(transparent)]
    Core(#[from] warden_core::CoreError),

    #[error("event payload (de)serialization failed: {0}")]
    EventPayload(#[from] serde_json::Error),

    /// Mirrors `warden::WardenError::EventKindMismatch` -- a row/wire
    /// message whose declared kind disagrees with its own payload is never
    /// silently trusted (code-standards.md: "toute ligne relue est
    /// reparsée en type Rust fort").
    #[error(
        "event {id} has event_type {event_type:?} but its payload's own kind is {payload_kind:?}"
    )]
    EventKindMismatch {
        id: String,
        event_type: String,
        payload_kind: &'static str,
    },

    #[error("run {run_id} not found")]
    RunNotFound { run_id: String },

    #[error("row column `{column}` = {value} does not fit in the expected numeric type")]
    InvalidStoredValue { column: &'static str, value: i64 },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// The connected terminal does not support any of the inline graphics
    /// protocols ADR-0010 covers (Kitty, iTerm2, Sixel) -- surfaced as a
    /// typed condition the caller decides how to react to (fall back to an
    /// external viewer), never a panic or a garbled render attempt.
    #[error("terminal does not support an inline graphics protocol (Kitty/iTerm2/Sixel)")]
    NoInlineGraphicsSupport,

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

    /// Video frame extraction (`ffmpeg`) and asciinema sub-terminal playback
    /// (ADR-0010) are deliberately out of scope for this pass: there is no
    /// `EVIDENCE` producer yet (Phase 7, issue #7, is not implemented on
    /// this branch), so there is no real evidence data to exercise either
    /// path against. Modeled as an explicit, typed "not yet implemented"
    /// rather than a half-working attempt.
    #[error("{feature} is not yet implemented (deferred: {reason})")]
    NotYetImplemented {
        feature: &'static str,
        reason: &'static str,
    },
}

pub type Result<T> = std::result::Result<T, TuiError>;
