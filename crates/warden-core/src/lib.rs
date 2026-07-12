//! Pure orchestration logic for Warden.
//!
//! This crate contains no I/O: no filesystem, no subprocess, no database. It
//! is the state machine and convergence rules that decide *what* should
//! happen next; the `warden` binary crate decides *how* to make it happen.

mod convergence;
mod error;
mod evidence;
mod state;

pub use convergence::{
    decide_next_state, decide_next_state_after_ci, parse_findings, CiOutcome, Finding,
    FindingSource, Severity,
};
pub use error::{CoreError, Result};
pub use evidence::{
    detect_project_type, select_evidence_tool, EvidenceTool, EvidenceType, ProjectMarkers,
    ProjectType,
};
pub use state::{AgentRole, RunState};
