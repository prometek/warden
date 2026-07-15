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

    /// A reverse-channel CI result message (issue #15/ADR-0011) that isn't
    /// valid JSON, or whose shape doesn't match [`crate::CiResultMessage`] --
    /// untrusted input at the `warden-gated` -> `warden` process boundary,
    /// validated with the same rigor as `parse_post_receive_line` (never
    /// silently ignored).
    #[error("malformed CI result message: {0}")]
    MalformedCiResultMessage(String),

    /// A `--evidence-json` argument (issue #15 review, M2) that isn't valid
    /// JSON, or whose shape doesn't match the expected evidence-row wire
    /// form -- untrusted input at the `warden` -> `warden-gated` process
    /// boundary, never silently ignored.
    #[error("malformed evidence rows: {0}")]
    MalformedEvidenceRows(String),

    /// A stdin JSON payload fed to (or read back from) an agent subprocess
    /// (ADR-0012, issue #20 Scope B) that isn't valid JSON, carries an
    /// unsupported `version`, or violates the role/field invariant
    /// `AgentInputMessage::for_coder`/`for_finding_agent` enforce at
    /// construction (e.g. a `coder` payload missing `intent`) -- validated
    /// with the same rigor as `MalformedCiResultMessage`.
    #[error("malformed agent input: {0}")]
    MalformedAgentInput(String),
}

pub type Result<T> = std::result::Result<T, CoreError>;
