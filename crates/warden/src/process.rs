//! Agent Subprocess Adapter (ADR-0005): spawns a single agent invocation as
//! a `tokio::process::Command`, cancellable via a `CancellationToken`
//! (code-standards.md: "tokio pour l'annulation propre des sous-process").
//! stdout is captured and handed back as-is — parsing/validating it into
//! [`warden_core::Finding`]s happens at the boundary in `warden-core`, this
//! module never interprets agent output itself.
//!
//! `spawn` and `wait` are split so a caller (the orchestrator) can persist
//! the child's PID to SQLite *before* awaiting completion — that's what
//! makes crash detection meaningful: if the orchestrator itself dies while
//! awaiting, the PID it already wrote is what recovery checks on restart.

use std::path::Path;

use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

use crate::error::ProcessError;

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

/// Spawns `command` with `cwd` pointed at the agent's isolated worktree
/// (code-standards.md: "Agent Subprocess Protocol"). The environment is not
/// inherited from the current process — only `PATH` is passed through, so
/// agents never see credentials sitting in the orchestrator's shell
/// environment (Architecture.md §10, "Isolation environnement des
/// sous-processus").
///
/// Returns the still-running [`Child`] so the caller can read its PID
/// (`child.id()`) and persist it before calling [`wait`].
pub fn spawn(command: &AgentCommand, cwd: &Path) -> Result<Child, ProcessError> {
    let mut cmd = Command::new(&command.program);
    cmd.args(&command.args)
        .current_dir(cwd)
        .env_clear()
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }

    cmd.spawn().map_err(|source| ProcessError::Spawn {
        command: command.program.clone(),
        source,
    })
}

/// Awaits a previously [`spawn`]ed child, cancellable via `cancel`. If
/// `cancel` fires first, the child is killed and
/// [`ProcessError::Cancelled`] is returned.
///
/// Uses `child.wait()` (borrows `&mut self`) rather than
/// `wait_with_output()` (which consumes `self`) so `child` is still
/// available to `kill()` in the cancellation branch of the `select!` below.
pub async fn wait(
    mut child: Child,
    command_name: &str,
    cancel: CancellationToken,
) -> Result<AgentOutcome, ProcessError> {
    use tokio::io::AsyncReadExt;

    tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            let _ = child.kill().await;
            Err(ProcessError::Cancelled { command: command_name.to_string() })
        }
        status_result = child.wait() => {
            let status = status_result.map_err(|source| ProcessError::Wait {
                command: command_name.to_string(),
                source,
            })?;

            // The child has exited by now, so draining its pipes to EOF is
            // bounded and safe (no risk of the child blocking on a full
            // pipe while we're not yet reading).
            let mut stdout_buf = Vec::new();
            if let Some(mut stdout) = child.stdout.take() {
                let _ = stdout.read_to_end(&mut stdout_buf).await;
            }
            let mut stderr_buf = Vec::new();
            if let Some(mut stderr) = child.stderr.take() {
                let _ = stderr.read_to_end(&mut stderr_buf).await;
            }

            Ok(AgentOutcome {
                exit_code: status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&stdout_buf).into_owned(),
                stderr: String::from_utf8_lossy(&stderr_buf).into_owned(),
            })
        }
    }
}

/// Convenience wrapper over [`spawn`] + [`wait`] for callers that don't
/// need the PID before completion (e.g. tests).
pub async fn spawn_and_wait(
    command: &AgentCommand,
    cwd: &Path,
    cancel: CancellationToken,
) -> Result<AgentOutcome, ProcessError> {
    let child = spawn(command, cwd)?;
    wait(child, &command.program, cancel).await
}

/// Checks whether `pid` still refers to a live process, without sending a
/// real signal (`kill(pid, 0)` semantics). Used by crash recovery to decide
/// whether a run left in an intermediate state genuinely still has an agent
/// working on it.
#[cfg(unix)]
pub fn is_process_alive(pid: u32) -> bool {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;

    kill(Pid::from_raw(pid as i32), None).is_ok()
}

#[cfg(not(unix))]
pub fn is_process_alive(_pid: u32) -> bool {
    // Phase 1 targets Unix (see Architecture.md — local, mono-utilisateur
    // tool). A non-Unix liveness check would need a different primitive
    // (e.g. OpenProcess on Windows); until that's implemented, treat any
    // recorded PID as not-alive so crash recovery fails safe towards
    // `Failed` rather than leaving a run stuck as "in progress" forever.
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn captures_stdout_and_exit_code_of_a_successful_command() {
        let dir = TempDir::new().unwrap();
        let cmd = AgentCommand::new("sh", ["-c", "echo hello"]);
        let outcome = spawn_and_wait(&cmd, dir.path(), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout.trim(), "hello");
    }

    #[tokio::test]
    async fn reports_a_non_zero_exit_code_as_a_normal_outcome_not_an_error() {
        let dir = TempDir::new().unwrap();
        let cmd = AgentCommand::new("sh", ["-c", "exit 7"]);
        let outcome = spawn_and_wait(&cmd, dir.path(), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(outcome.exit_code, 7);
    }

    #[tokio::test]
    async fn spawn_exposes_the_pid_before_completion() {
        let dir = TempDir::new().unwrap();
        let cmd = AgentCommand::new("sh", ["-c", "sleep 0.2"]);
        let child = spawn(&cmd, dir.path()).unwrap();
        let pid = child
            .id()
            .expect("pid available for a freshly spawned child");
        assert!(is_process_alive(pid));
        wait(child, "sh", CancellationToken::new()).await.unwrap();
    }

    #[tokio::test]
    async fn cancellation_kills_the_child_and_returns_cancelled_error() {
        let dir = TempDir::new().unwrap();
        let cmd = AgentCommand::new("sh", ["-c", "sleep 30"]);
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        let handle =
            tokio::spawn(async move { spawn_and_wait(&cmd, dir.path(), cancel_clone).await });
        cancel.cancel();

        let result = handle.await.unwrap();
        assert!(matches!(result, Err(ProcessError::Cancelled { .. })));
    }

    #[test]
    fn current_process_is_reported_alive() {
        assert!(is_process_alive(std::process::id()));
    }

    #[test]
    fn a_pid_that_almost_certainly_does_not_exist_is_reported_not_alive() {
        // Real PIDs are far smaller than this on both Linux (< 2^22 by
        // default) and macOS (< 100_000); used purely as a deterministic
        // "not alive" fixture, well within the valid positive pid_t range.
        assert!(!is_process_alive(999_999_999));
    }
}
