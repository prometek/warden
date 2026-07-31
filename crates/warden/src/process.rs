//! Subprocess Adapter (ADR-0005): spawns one child as a
//! `tokio::process::Command`, cancellable via a `CancellationToken`
//! (code-standards.md: "tokio pour l'annulation propre des sous-process").
//! stdout is captured and handed back as-is — parsing/validating it into
//! [`warden_core::Finding`]s happens at the boundary in `warden-core`, this
//! module never interprets agent output itself.
//!
//! **Issue #50: this is no longer the coder/reviewer/tester invocation
//! path.** Every agent now runs through `warden_sandbox::Sandbox`
//! (`warden_sandbox::LocalSandbox` by default -- a strict-parity port of
//! what [`spawn`]/[`wait`] used to do for that path, including its own
//! per-invocation env-allowlist forwarding and per-line progress callback),
//! wired in by `orchestrator::Orchestrator::run_agent`. [`spawn`]/[`wait`]
//! remain here for the one caller that still needs the general primitive
//! for a non-agent subprocess: the Evidence Capture Adapter (`evidence.rs`,
//! via [`spawn_and_wait`]).
//!
//! `spawn` and `wait` are split so a caller can persist the child's PID to
//! SQLite *before* awaiting completion — that's the same crash-detection
//! shape `warden_sandbox::Execution` gives the agent path (its own
//! `pid`/`wait` split, `warden_sandbox`'s own docs), just for a plain
//! subprocess here instead.
//!
//! [`wait`] writes an optional stdin payload (if any) and closes the write
//! half concurrently with draining stdout/stderr and awaiting exit — see its
//! own docs for why writing stdin any other way can deadlock.
//!
//! # Issue #26: [`validate_agent_program`], a belt-and-braces guard on `program` (and, since #59, `args`)
//!
//! No built-in [`crate::tool_adapter::ToolAdapter`] shipped today ever names
//! a `command.program` that resolves inside the repo under review --
//! `ClaudeAdapter::build_command` always names a bare `claude`, resolved via
//! `PATH` (see [`spawn`]'s own docs on why a relative program is otherwise
//! long-standing, accepted behaviour). But nothing in the type system stops
//! a *future* adapter from doing exactly that, and the entire point of
//! running the reviewer/tester as an independent gate (Architecture.md §1)
//! is that the coder must never control what they execute -- so this is
//! checked once, structurally, at the one call site every coder/reviewer/
//! tester spawn in this codebase goes through
//! (`orchestrator::Orchestrator::run_agent`), rather than trusted to stay
//! true of every adapter forever.
//!
//! **Issue #59** scopes this same reasoning to `args`, deliberately left
//! uncovered by #26: a future adapter naming something like `claude
//! --wrapper ./reviewer.sh` would reintroduce the exact hole #26 closes,
//! since `./reviewer.sh` still resolves against the role's own worktree --
//! `program` being clean says nothing about `args`. [`path_like_candidate`]
//! decides which `args` entries even get the containment check (a
//! conservative separator-based heuristic, not a per-adapter declaration of
//! which args are paths -- that would need a `ToolAdapter` API change this
//! issue deliberately avoids); see its own docs for the exact rule -- as of
//! review round 2, judged from a value's first whitespace-delimited token
//! only, and only when that token carries no shell syntax -- and
//! [`validate_agent_program`]'s own docs for the residual gaps this
//! heuristic does *not* close (a bare-name `args` entry with no separator
//! at all; a path-shaped separator appearing after a value's first token; a
//! path-shaped first token that itself contains shell metacharacters) and
//! the `trusted_arg_values` escape hatch for a caller-vouched non-path
//! value the heuristic would otherwise misjudge.

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
