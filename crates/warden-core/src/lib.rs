//! Pure orchestration logic for Warden.
//!
//! This crate contains no I/O: no filesystem, no subprocess, no database. It
//! is the state machine and convergence rules that decide *what* should
//! happen next; the `warden` binary crate decides *how* to make it happen.
//! The one narrow exception is [`resolve_socket_path`], which reads
//! `std::env::temp_dir()` (an environment/OS constant lookup, not a
//! filesystem access) -- see its module docs for why it lives here rather
//! than duplicated per-crate like the rest of the I/O layer.

mod agent_def;
mod agent_wire;
mod ci_channel;
mod convergence;
mod error;
mod event;
mod evidence;
mod evidence_wire;
mod pr_body;
mod socket;
mod state;

pub use agent_def::{parse_agent_definition, AgentDefinition, RunnerKind, FRONTMATTER_FENCE};
pub use agent_wire::{
    parse_agent_input_message, AgentInputMessage, AGENT_INPUT_VERSION, DIFF_TRUNCATED_MARKER,
};
pub use ci_channel::{parse_ci_result_message, CiResultMessage, CiWatchOutcome};
pub use convergence::{
    decide_next_state, decide_next_state_after_ci, parse_findings, CiOutcome, Finding,
    FindingSource, Severity,
};
pub use error::{CoreError, Result};
pub use event::{EventKind, RunEvent, RunEventRecord};
pub use evidence::{
    detect_project_type, select_evidence_tool, EvidenceTool, EvidenceType, ProjectMarkers,
    ProjectType,
};
pub use evidence_wire::{parse_evidence_rows, serialize_evidence_rows};
pub use pr_body::{format_evidence_section, EvidenceRow};
pub use socket::{resolve_ci_result_socket_path, resolve_socket_path, MAX_SOCKET_PATH_LEN};
pub use state::{AgentRole, RunState};
