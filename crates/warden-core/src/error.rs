//! Error types for `warden-core`.

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

    #[error("malformed CI result message: {0}")]
    MalformedCiResultMessage(String),

    /// A `--evidence-json` argument that isn't valid JSON, or whose shape doesn't match the
    /// expected evidence-row wire form.
    #[error("malformed evidence rows: {0}")]
    MalformedEvidenceRows(String),

    #[error("malformed agent input: {0}")]
    MalformedAgentInput(String),

    #[error("malformed agent definition: {0}")]
    MalformedAgentDefinition(String),

    #[error("invalid workflow: {0}")]
    InvalidWorkflow(String),
}

pub type Result<T> = std::result::Result<T, CoreError>;
