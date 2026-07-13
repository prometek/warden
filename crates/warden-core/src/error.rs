//! Error types for `warden-core`.
//!
//! Everything here is a pure, in-memory error: no I/O failure ever
//! originates in this crate.

use thiserror::Error;

use crate::state::RunState;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CoreError {
    #[error("invalid run state transition: {from:?} -> {to:?}")]
    InvalidTransition { from: RunState, to: RunState },

    #[error("unknown run state: {0:?}")]
    UnknownState(String),

    #[error("unknown agent role: {0:?}")]
    UnknownRole(String),

    #[error("unknown finding source: {0:?}")]
    UnknownFindingSource(String),

    #[error("unknown finding severity: {0:?}")]
    UnknownSeverity(String),

    #[error("malformed agent output: {0}")]
    MalformedAgentOutput(String),

    #[error("unknown event kind: {0:?}")]
    UnknownEventKind(String),
    #[error("unknown evidence type: {0:?}")]
    UnknownEvidenceType(String),

    #[error("unknown evidence tool: {0:?}")]
    UnknownEvidenceTool(String),
}

pub type Result<T> = std::result::Result<T, CoreError>;
