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

    /// A markdown agent definition (issue #24, Claude Code's own
    /// `.claude/agents/*.md` schema) whose frontmatter fence is
    /// missing/unterminated, whose frontmatter isn't valid YAML, carries an
    /// unknown key (`deny_unknown_fields`), carries a blank-but-present
    /// optional field, or whose system-prompt body is blank -- untrusted
    /// input read off disk at the CLI boundary, validated with the same
    /// rigor as [`Self::MalformedAgentInput`].
    #[error("malformed agent definition: {0}")]
    MalformedAgentDefinition(String),

    /// Issue #73: a `.warden/workflow.yaml` that isn't valid YAML, or whose
    /// shape violates [`crate::workflow::Workflow`]'s own invariants (empty
    /// `steps`, a blank `role`/`agent`, a duplicate `role`, an unknown
    /// `gate`, or a first step that isn't a plain pass-through) -- validated
    /// at the boundary with the same rigor as every other user-supplied file
    /// this crate parses. The message names *what* is wrong; the caller
    /// (`warden::agent_def`, which reads the file) is responsible for naming
    /// *which file*.
    #[error("invalid workflow: {0}")]
    InvalidWorkflow(String),
}

pub type Result<T> = std::result::Result<T, CoreError>;
