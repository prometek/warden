//! Pure orchestration logic for Warden.

mod agent_def;
mod agent_wire;
mod ci_channel;
mod convergence;
mod error;
mod event;
mod evidence;
mod evidence_wire;
mod hook;
mod pr_body;
mod rate_limit;
mod socket;
mod state;
mod token_usage;
mod workflow;

pub use agent_def::{parse_agent_definition, AgentDefinition};
pub use agent_wire::{
    build_step_input_json, parse_agent_input_message, AgentInputMessage, AGENT_INPUT_VERSION,
    DIFF_TRUNCATED_MARKER,
};
pub use ci_channel::{parse_ci_result_message, CiResultMessage, CiWatchOutcome};
pub use convergence::{
    decide_next_state_after_ci, decide_next_state_for_step, parse_findings, state_for_target,
    validate_finding_sources_for_role, CiOutcome, Finding, FindingSource, Severity,
};
pub use error::{CoreError, Result};
pub use event::{
    EventKind, RunEvent, RunEventHistoryEntry, RunEventRecord, UndecodableEvent, UndecodableReason,
};
pub use evidence::{
    detect_project_type, select_evidence_tool, EvidenceTool, EvidenceType, ProjectMarkers,
    ProjectType,
};
pub use evidence_wire::{parse_evidence_rows, serialize_evidence_rows};
pub use hook::{HookContext, HookOutcome, HookPoint};
pub use pr_body::{format_evidence_section, EvidenceRow};
pub use rate_limit::{RateLimitState, RateLimitStatus, RateLimitWindow};
pub use socket::{resolve_ci_result_socket_path, resolve_socket_path, MAX_SOCKET_PATH_LEN};
pub use state::RunState;
pub use token_usage::TokenUsage;
pub use workflow::{
    Role, StepKind, StepOutcome, StepTarget, StepTransitions, Workflow, WorkflowStep,
};
