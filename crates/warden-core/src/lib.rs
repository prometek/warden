//! Pure orchestration logic for Warden.
//!
//! This crate contains no I/O: no filesystem, no subprocess, no database. It
//! is the state machine and convergence rules that decide *what* should
//! happen next; the `warden` binary crate decides *how* to make it happen.

mod convergence;
mod error;
mod state;

pub use convergence::{decide_next_state, parse_findings, Finding, FindingSource, Severity};
pub use error::{CoreError, Result};
pub use state::{AgentRole, RunState};
