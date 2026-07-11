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

/// Sentinel meaning "no process start time was recorded for this row" —
/// used for historical rows written before start-time tracking existed.
/// `0` is never a real Unix start time in practice (that would be 1970).
pub const UNKNOWN_START_TIME: i64 = 0;

/// Returns the OS-reported start time (seconds since the Unix epoch) of
/// `pid`, or `None` if no such process exists right now.
///
/// This is what lets [`is_process_alive`] tell a still-running process
/// apart from an unrelated process that happens to have reused the same PID
/// after a reboot: PIDs are a small, wrapping namespace recycled by the OS,
/// so a bare "does this PID exist" check is not sufficient for correctness
/// over the lifetime of a persisted run (H1 / crash recovery, Architecture
/// §9). A process's start time is immutable for its whole lifetime, so
/// re-reading it later and comparing against a value captured right after
/// spawn reliably detects PID reuse.
pub fn process_start_time(pid: u32) -> Option<i64> {
    if pid == 0 {
        return None;
    }
    // A single-PID refresh (not `ProcessesToUpdate::All`) keeps this cheap
    // enough to call synchronously from an async context — it's invoked at
    // most once per agent invocation, never in a hot loop.
    let mut system = sysinfo::System::new();
    system.refresh_processes(
        sysinfo::ProcessesToUpdate::Some(&[sysinfo::Pid::from_u32(pid)]),
        true,
    );
    system
        .process(sysinfo::Pid::from_u32(pid))
        .map(|process| process.start_time() as i64)
}

/// Checks whether `pid` still refers to the *same* process that was
/// recorded with `expected_start_time` (seconds since epoch), not merely
/// whether some process with that PID currently exists.
///
/// `pid == 0` is always reported not-alive: POSIX `kill(0, ...)` signals
/// the caller's entire process group rather than a single process, so a
/// naive `kill(pid, None)` liveness check against a pid-0 sentinel always
/// (mis)reports "alive" regardless of whether an agent is actually running
/// — this was H1, a real bug in the previous implementation.
///
/// If `expected_start_time` is [`UNKNOWN_START_TIME`] (no start time was
/// ever recorded for this row), falls back to a plain existence check —
/// strictly less safe against PID reuse, and logged as such.
pub fn is_process_alive(pid: u32, expected_start_time: i64) -> bool {
    if pid == 0 {
        return false;
    }

    let Some(actual_start_time) = process_start_time(pid) else {
        return false;
    };

    if expected_start_time == UNKNOWN_START_TIME {
        tracing::warn!(
            pid,
            "checking process liveness without a recorded start time; cannot rule out PID reuse"
        );
        return true;
    }

    actual_start_time == expected_start_time
}

/// Terminates `pid`, but only if it is still the exact process recorded at
/// `expected_start_time` (H1: PID-reuse hardening) — re-checked here,
/// immediately before signalling, to shrink the race window between a
/// caller's earlier [`is_process_alive`] check and the kill itself. Used by
/// crash recovery to clean up orphaned agent processes left running after
/// the orchestrator that owned them died without a chance to run
/// `kill_on_drop` (Architecture.md §9, Disaster Recovery).
///
/// Returns `Ok(())` if the process is already gone, or was never the one
/// recorded (fingerprint mismatch) — neither is an error, both just mean
/// there is nothing left to kill. `pid == 0` is always treated as
/// already-gone: see [`is_process_alive`] for why a pid-0 sentinel must
/// never be signalled.
pub fn kill_pid(pid: u32, expected_start_time: i64) -> Result<(), ProcessError> {
    if !is_process_alive(pid, expected_start_time) {
        return Ok(());
    }

    let mut system = sysinfo::System::new();
    system.refresh_processes(
        sysinfo::ProcessesToUpdate::Some(&[sysinfo::Pid::from_u32(pid)]),
        true,
    );

    match system.process(sysinfo::Pid::from_u32(pid)) {
        Some(process) => {
            if process.kill() {
                Ok(())
            } else {
                Err(ProcessError::KillFailed { pid })
            }
        }
        // Disappeared between the liveness check above and this refresh —
        // already gone, nothing to do.
        None => Ok(()),
    }
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
        let start_time = process_start_time(pid).expect("start time available for a live process");
        assert!(is_process_alive(pid, start_time));
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
        let pid = std::process::id();
        let start_time =
            process_start_time(pid).expect("start time available for the current process");
        assert!(is_process_alive(pid, start_time));
    }

    #[test]
    fn a_pid_that_almost_certainly_does_not_exist_is_reported_not_alive() {
        // Real PIDs are far smaller than this on both Linux (< 2^22 by
        // default) and macOS (< 100_000); used purely as a deterministic
        // "not alive" fixture, well within the valid positive pid_t range.
        assert!(!is_process_alive(999_999_999, UNKNOWN_START_TIME));
    }

    #[test]
    fn a_wrong_start_time_is_reported_not_alive_even_though_the_pid_exists() {
        // The core PID-reuse defence (H1): a PID that genuinely exists
        // right now must still be reported not-alive if the start time we
        // recorded for it doesn't match the process currently holding that
        // PID — that mismatch is exactly what happens when the original
        // process died and the OS handed its PID to something else later.
        let pid = std::process::id();
        let real_start_time = process_start_time(pid).unwrap();
        let bogus_start_time = real_start_time + 1_000_000;
        assert!(!is_process_alive(pid, bogus_start_time));
    }

    #[test]
    fn no_recorded_start_time_falls_back_to_plain_existence_check() {
        // Historical/degraded case: UNKNOWN_START_TIME means we never
        // captured a fingerprint for this row, so we can't rule out PID
        // reuse — but we also shouldn't refuse to ever recover such rows,
        // so we fall back to "does a process with this PID exist at all".
        let pid = std::process::id();
        assert!(is_process_alive(pid, UNKNOWN_START_TIME));
    }

    /// Regression test for H1: POSIX `kill(pid=0, ...)` signals every
    /// process in the caller's own process group, so a naive liveness check
    /// against a pid-0 sentinel always misreported "alive" regardless of
    /// whether pid 0 referred to a real agent — silently defeating the
    /// crash-detection acceptance criterion in issue #1. `pid == 0` is now
    /// an explicit sentinel that is never alive, and
    /// `orchestrator::run_agent` no longer persists pid 0 at all (a missing
    /// `Child::id()` is a typed `ProcessError::MissingPid`, not a silent
    /// fallback to 0).
    #[test]
    fn pid_zero_is_never_reported_alive() {
        assert!(!is_process_alive(0, UNKNOWN_START_TIME));
        assert!(!is_process_alive(0, 12345));
    }

    #[tokio::test]
    async fn kill_pid_terminates_a_live_process_with_a_matching_fingerprint() {
        let dir = TempDir::new().unwrap();
        let cmd = AgentCommand::new("sh", ["-c", "sleep 30"]);
        let mut child = spawn(&cmd, dir.path()).unwrap();
        let pid = child.id().unwrap();
        let start_time = process_start_time(pid).unwrap();

        kill_pid(pid, start_time).unwrap();

        // `wait()` blocks until the OS has reaped it — proves the signal
        // actually landed, not just that `kill_pid` returned `Ok`.
        let status = child.wait().await.unwrap();
        assert!(!status.success());
        assert!(!is_process_alive(pid, start_time));
    }

    #[tokio::test]
    async fn kill_pid_is_a_noop_when_the_fingerprint_no_longer_matches() {
        // H1 regression: a live process that genuinely exists at `pid` must
        // never be signalled if its recorded start time doesn't match —
        // that mismatch is exactly the PID-reuse case this guards against.
        let dir = TempDir::new().unwrap();
        let cmd = AgentCommand::new("sh", ["-c", "sleep 30"]);
        let mut child = spawn(&cmd, dir.path()).unwrap();
        let pid = child.id().unwrap();
        let real_start_time = process_start_time(pid).unwrap();
        let bogus_start_time = real_start_time + 1_000_000;

        kill_pid(pid, bogus_start_time).unwrap();

        // Still alive: the mismatched fingerprint must have stopped
        // `kill_pid` from touching it.
        assert!(is_process_alive(pid, real_start_time));
        child.kill().await.unwrap();
    }

    #[test]
    fn kill_pid_on_pid_zero_is_a_noop_not_a_signal_to_the_process_group() {
        assert!(kill_pid(0, UNKNOWN_START_TIME).is_ok());
    }

    #[test]
    fn kill_pid_on_an_already_dead_pid_is_a_noop() {
        assert!(kill_pid(999_999_999, UNKNOWN_START_TIME).is_ok());
    }
}
