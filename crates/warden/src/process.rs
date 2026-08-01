//! Subprocess Adapter: spawns one child as a `tokio::process::Command`, cancellable via a
//! `CancellationToken` (code-standards.md: "tokio pour l'annulation propre des sous-process").

use std::path::Path;

use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

use crate::error::ProcessError;
use crate::path_util::canonicalize_best_effort;

mod execution;
mod lifecycle;
mod validation;

pub use execution::{spawn, spawn_and_wait, spawn_tui_attach, wait};
pub use lifecycle::{is_process_alive, kill_pid, process_start_time, UNKNOWN_START_TIME};
pub use validation::validate_agent_program;

#[cfg(test)]
use execution::classify_stdin_write_error;

/// A single agent invocation to run in an isolated worktree.
#[derive(Debug, Clone)]
pub struct AgentCommand {
    pub program: String,
    pub args: Vec<String>,
}

impl AgentCommand {
    pub fn new(
        program: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }
}

/// Outcome of a completed (non-cancelled) agent invocation.
#[derive(Debug)]
pub struct AgentOutcome {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

#[cfg(test)]
mod tests;
