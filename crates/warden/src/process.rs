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
//!
//! ADR-0012 (issue #20 Scope B): `spawn` also pipes stdin, so a caller can
//! feed the agent the run intent / target commit+diff+findings
//! (`warden_core::AgentInputMessage`) over the one channel
//! code-standards.md's Agent Subprocess Protocol already sanctions for this
//! ("Échange JSON en streaming sur stdin/stdout"). [`wait`] writes that
//! payload (if any) and closes the write half concurrently with draining
//! stdout/stderr and awaiting exit — see its own docs for why writing stdin
//! any other way can deadlock.

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
/// inherited from the current process — `env_clear()` always runs first, so
/// agents never see credentials sitting in the orchestrator's shell
/// environment by default (Architecture.md §10, "Isolation environnement des
/// sous-processus"). Convenience wrapper over [`spawn_with_extra_env`] with
/// an empty allowlist — every call site that doesn't need extra env vars
/// (evidence capture, every test in this module) uses this directly.
///
/// **A relative `command.program` resolves against `cwd`**, i.e. against the
/// worktree — the child chdirs before exec, so `./reviewer.sh` means *the
/// repo's own copy of that script at the commit under review*, which the
/// coder can rewrite and commit. Long-standing behaviour, documented here
/// rather than changed — refusing relative paths is a product decision, and
/// it would break the plain-script case a custom `AgentCommand` might still
/// exist to serve outside a built-in `warden::tool_adapter::ToolAdapter`
/// (which, for the adapters Warden ships, always names an absolute-lookup
/// binary like `claude`, resolved via `PATH`, never a path relative to the
/// worktree under review).
///
/// stdin is piped (ADR-0012, issue #20 Scope B) rather than inherited, so
/// the intent/target-commit/diff/findings payload [`wait`] writes never
/// leaks the orchestrator's own stdin into the agent. An agent that never
/// reads stdin at all is *not* unconditionally unaffected: a payload small
/// enough to fit in the OS pipe buffer (typically 64KiB) is written without
/// blocking and simply sits there unread until the agent exits, but a
/// larger payload blocks [`wait`]'s write until either the agent reads
/// enough to make room or exits and closes its read end (a broken pipe,
/// handled explicitly — see [`wait`]).
///
/// Returns the still-running [`Child`] so the caller can read its PID
/// (`child.id()`) and persist it before calling [`wait`].
pub fn spawn(command: &AgentCommand, cwd: &Path) -> Result<Child, ProcessError> {
    spawn_with_extra_env(command, cwd, &[])
}

/// Like [`spawn`], but also forwards each variable named in
/// `extra_env_vars` — if it is actually set in `warden`'s own environment —
/// on top of the always-forwarded `PATH`.
///
/// This is the Architecture.md §10 relaxation issue #24 asks for by name,
/// implementing `warden_core`-agnostic infrastructure for
/// `warden::tool_adapter::ToolAdapter::env_allowlist` (e.g. `claude` needs
/// `HOME` to find its own auth/config): **`env_clear()` still runs first,
/// unconditionally** — this is a small, explicit, per-invocation opt-in
/// allowlist layered back on top, never a switch to inheriting the full
/// environment. A caller that doesn't pass an adapter-provided allowlist
/// (every non-agent subprocess: evidence capture, git plumbing) is
/// unaffected and gets exactly the previous `PATH`-only behaviour via
/// [`spawn`].
pub fn spawn_with_extra_env(
    command: &AgentCommand,
    cwd: &Path,
    extra_env_vars: &[&str],
) -> Result<Child, ProcessError> {
    let mut cmd = Command::new(&command.program);
    cmd.args(&command.args)
        .current_dir(cwd)
        .env_clear()
        .kill_on_drop(true)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }
    for var_name in extra_env_vars {
        if let Ok(value) = std::env::var(var_name) {
            cmd.env(var_name, value);
        }
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
/// `stdin_payload`, if given, is written to the child's stdin and the write
/// half is then closed (dropped) so the agent sees EOF rather than hanging
/// forever waiting for more input — this happens even when `stdin_payload`
/// is `None`, since a piped stdin ([`spawn`]) that's never closed would
/// otherwise hang an agent that reads until EOF before proceeding.
///
/// **Deadlock avoidance**: the write, the stdout/stderr draining, and the
/// wait for exit all run *concurrently* (`tokio::join!`), not sequentially.
/// Writing the whole payload before draining anything (or draining only
/// after exit, as this function used to) risks a classic pipe deadlock: an
/// agent that interleaves reading stdin with writing enough stdout/stderr to
/// fill the OS pipe buffer (typically 64KiB) before it has consumed all of
/// stdin will block on its own full stdout/stderr
/// pipe; meanwhile we'd be blocked writing to a stdin the agent has stopped
/// reading — neither side can make progress. Running all four concurrently
/// means each blocked read/write just yields to the executor, and progress
/// on any one of them unblocks the others.
///
/// **Stdin write failures** (H1, issue #20 review): a broken pipe (the
/// agent closed or never read stdin before exiting) is logged at `warn` and
/// treated as a normal, non-fatal outcome — see
/// [`classify_stdin_write_error`]. Any other write error fails this call
/// with [`ProcessError::StdinWrite`] instead of letting the run continue
/// silently: `stdin_payload` is always a single JSON object, so a partial
/// write is unparsable by the agent by construction, and there is no
/// recovery short of failing the invocation.
///
/// Uses `child.wait()` (borrows `&mut self`) rather than
/// `wait_with_output()` (which consumes `self`) so `child` is still
/// available to `kill()` in the cancellation branch of the `select!` below.
pub async fn wait(
    mut child: Child,
    command_name: &str,
    stdin_payload: Option<String>,
    cancel: CancellationToken,
) -> Result<AgentOutcome, ProcessError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let stdin_handle = child.stdin.take();
    let stdout_handle = child.stdout.take();
    let stderr_handle = child.stderr.take();

    let stdin_task = async move {
        if let Some(mut stdin_handle) = stdin_handle {
            if let Some(payload) = stdin_payload {
                if let Err(error) = stdin_handle.write_all(payload.as_bytes()).await {
                    classify_stdin_write_error(error, command_name)?;
                }
            }
            // Dropping `stdin_handle` here (end of scope) closes the write
            // half, signalling EOF — required even with no payload to
            // write.
        }
        Ok::<(), std::io::Error>(())
    };
    let stdout_task = async move {
        let mut buf = Vec::new();
        if let Some(mut stdout_handle) = stdout_handle {
            if let Err(error) = stdout_handle.read_to_end(&mut buf).await {
                tracing::warn!(command = command_name, %error, "failed to read agent stdout to completion");
            }
        }
        buf
    };
    let stderr_task = async move {
        let mut buf = Vec::new();
        if let Some(mut stderr_handle) = stderr_handle {
            if let Err(error) = stderr_handle.read_to_end(&mut buf).await {
                tracing::warn!(command = command_name, %error, "failed to read agent stderr to completion");
            }
        }
        buf
    };

    tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            let _ = child.kill().await;
            Err(ProcessError::Cancelled { command: command_name.to_string() })
        }
        result = async {
            let (stdin_result, stdout_buf, stderr_buf, status_result) =
                tokio::join!(stdin_task, stdout_task, stderr_task, child.wait());
            let status = status_result.map_err(|source| ProcessError::Wait {
                command: command_name.to_string(),
                source,
            })?;
            // H1: a non-broken-pipe stdin write failure fails the
            // invocation outright rather than silently running the agent
            // with a partial (unparsable) or absent payload — the child has
            // already been awaited above via the same `join!`, so nothing
            // is left running when this returns.
            stdin_result.map_err(|source| ProcessError::StdinWrite {
                command: command_name.to_string(),
                source,
            })?;
            Ok(AgentOutcome {
                exit_code: status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&stdout_buf).into_owned(),
                stderr: String::from_utf8_lossy(&stderr_buf).into_owned(),
            })
        } => result,
    }
}

/// Classifies a stdin write failure (H1, issue #20 review): a broken pipe
/// means the agent closed or never opened its read side of stdin — e.g. it
/// exited before reading everything, or ignores stdin entirely and exits on
/// its own — which is a legitimate agent behaviour, logged at `warn` (never
/// silently dropped) but not fatal to the run. Any other error (disk full on
/// a buffered pipe implementation, permission error, etc.) is fatal: the
/// payload is a single JSON object, so a partial write is unparsable by the
/// agent by construction, and continuing would mean the agent runs with no
/// intent/context at all — exactly the silent fallback code-standards.md
/// forbids.
fn classify_stdin_write_error(
    error: std::io::Error,
    command_name: &str,
) -> Result<(), std::io::Error> {
    if error.kind() == std::io::ErrorKind::BrokenPipe {
        tracing::warn!(
            command = command_name,
            %error,
            "agent closed stdin before the full payload was written; continuing without a \
             guarantee it read the payload"
        );
        Ok(())
    } else {
        Err(error)
    }
}

/// Convenience wrapper over [`spawn`] + [`wait`] for callers that don't
/// need the PID before completion (e.g. tests) or a stdin payload (e.g. the
/// Evidence Capture Adapter's `playwright`/`asciinema` invocations, which
/// aren't agents in the coder/reviewer/tester sense and receive no
/// intent/findings context).
pub async fn spawn_and_wait(
    command: &AgentCommand,
    cwd: &Path,
    cancel: CancellationToken,
) -> Result<AgentOutcome, ProcessError> {
    let child = spawn(command, cwd)?;
    wait(child, &command.program, None, cancel).await
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
/// `expected_start_time` (H1: PID-reuse hardening).
///
/// Deliberately does *not* call [`is_process_alive`] first and then act on
/// that answer: two separate `sysinfo` refreshes (one to check liveness,
/// another later to obtain a handle to kill) would leave a race window
/// between them where the OS could reuse `pid` for an unrelated process,
/// which this function would then signal by mistake. Instead, a *single*
/// refresh produces the exact process handle that is both fingerprint-
/// checked and killed, so there is no gap in which the PID can change
/// identity out from under this call.
///
/// Returns `Ok(())` if the process is already gone, or is no longer the one
/// recorded (fingerprint mismatch) — neither is an error, both just mean
/// there is nothing left to kill. `pid == 0` is always treated as
/// already-gone: see [`is_process_alive`] for why a pid-0 sentinel must
/// never be signalled.
pub fn kill_pid(pid: u32, expected_start_time: i64) -> Result<(), ProcessError> {
    if pid == 0 {
        return Ok(());
    }

    let mut system = sysinfo::System::new();
    system.refresh_processes(
        sysinfo::ProcessesToUpdate::Some(&[sysinfo::Pid::from_u32(pid)]),
        true,
    );

    let Some(process) = system.process(sysinfo::Pid::from_u32(pid)) else {
        // Nothing at this pid right now — already gone, nothing to do.
        return Ok(());
    };

    let actual_start_time = process.start_time() as i64;
    if expected_start_time == UNKNOWN_START_TIME {
        // Degraded case, same as `is_process_alive`: no fingerprint was ever
        // recorded for this row, so PID reuse can't be ruled out. Logged,
        // not refused — a historical row shouldn't be permanently
        // unreclaimable just because it predates start-time tracking.
        tracing::warn!(
            pid,
            "killing a process without a recorded start time; cannot rule out PID reuse"
        );
    } else if actual_start_time != expected_start_time {
        // The PID has been reused by an unrelated process since it was
        // recorded (H1) — on the very same handle we're about to kill, not
        // a separate, earlier check. Never signal it.
        return Ok(());
    }

    if process.kill() {
        Ok(())
    } else {
        Err(ProcessError::KillFailed { pid })
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
        wait(child, "sh", None, CancellationToken::new())
            .await
            .unwrap();
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

    /// ADR-0012 (issue #20 Scope B): a payload written to stdin must reach
    /// the child, and the write half must be closed afterwards so a child
    /// that reads until EOF (`cat` with no arguments) actually sees one and
    /// exits, rather than hanging forever waiting for more input.
    #[tokio::test]
    async fn stdin_payload_is_written_and_closed_so_the_child_sees_it_and_exits() {
        let dir = TempDir::new().unwrap();
        let cmd = AgentCommand::new("cat", Vec::<String>::new());
        let child = spawn(&cmd, dir.path()).unwrap();
        let outcome = wait(
            child,
            "cat",
            Some("hello from warden".to_string()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout, "hello from warden");
    }

    /// ADR-0012 regression test: writing a large stdin payload while the
    /// child also produces enough stdout to fill an OS pipe buffer *before*
    /// it finishes reading stdin must not deadlock. Sequenced deliberately
    /// (write >64KiB of stdout first, only then drain stdin) so a naive
    /// "write the whole payload, then read stdout" implementation would
    /// hang: the child blocks on its own full stdout pipe (nobody's
    /// draining it yet) while we block on the child's full stdin pipe (it
    /// isn't reading yet either). Bounded by a timeout so a regression fails
    /// the test instead of hanging the suite.
    #[tokio::test]
    async fn writing_a_large_stdin_payload_does_not_deadlock_on_large_stdout() {
        let dir = TempDir::new().unwrap();
        // Emits 200_000 bytes of stdout first (well past a typical 64KiB
        // pipe buffer), then only afterwards drains stdin to completion.
        let cmd = AgentCommand::new(
            "sh",
            ["-c", "head -c 200000 /dev/zero; cat > /dev/null; exit 0"],
        );
        let child = spawn(&cmd, dir.path()).unwrap();
        let large_payload = "x".repeat(200_000);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            wait(child, "sh", Some(large_payload), CancellationToken::new()),
        )
        .await
        .expect("wait must not hang when both stdin and stdout exceed the pipe buffer size");

        assert_eq!(result.unwrap().exit_code, 0);
    }

    /// H1 (issue #20 review): an agent that exits immediately without ever
    /// reading stdin at all must not fail the invocation — a broken pipe is
    /// a legitimate outcome (logged, not silently swallowed), not a reason
    /// to fail the run. The payload is deliberately larger than a typical
    /// OS pipe buffer (64KiB) so the write is guaranteed to still be in
    /// progress when the child exits and closes its read end, forcing a
    /// genuine `ErrorKind::BrokenPipe` rather than racing a write that might
    /// complete before the child even schedules to exit.
    #[tokio::test]
    async fn an_agent_that_never_reads_stdin_and_exits_immediately_does_not_fail_the_invocation() {
        let dir = TempDir::new().unwrap();
        let cmd = AgentCommand::new("sh", ["-c", "exit 0"]);
        let child = spawn(&cmd, dir.path()).unwrap();
        let large_payload = "x".repeat(200_000);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            wait(child, "sh", Some(large_payload), CancellationToken::new()),
        )
        .await
        .expect("wait must not hang on a broken pipe");

        let outcome = result
            .expect("a broken pipe from an agent that ignores stdin must not fail the invocation");
        assert_eq!(outcome.exit_code, 0);
    }

    /// H1 unit coverage for [`classify_stdin_write_error`]'s two branches.
    /// The fatal (non-`BrokenPipe`) branch is exercised here rather than
    /// through a real subprocess: deterministically forcing a write error
    /// other than a broken pipe out of a genuine OS pipe isn't practical
    /// (`EPIPE` is by far the dominant real-world case, already covered
    /// end-to-end by `an_agent_that_never_reads_stdin_and_exits_immediately_does_not_fail_the_invocation`
    /// above), so this isolates the classification decision itself.
    #[test]
    fn classify_stdin_write_error_treats_broken_pipe_as_non_fatal_and_anything_else_as_fatal() {
        let broken_pipe = std::io::Error::from(std::io::ErrorKind::BrokenPipe);
        assert!(classify_stdin_write_error(broken_pipe, "agent").is_ok());

        let other = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let result = classify_stdin_write_error(other, "agent");
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }

    // -----------------------------------------------------------------
    // Re-test cycle (issue #20 review fix, fdcaa4e): adversarial stdin
    // write-failure angles beyond the coder's own "never reads at all"
    // case, derived from the task's intent independent of the coder's
    // tests above.
    // -----------------------------------------------------------------

    /// Adversarial angle: an agent that reads only *part* of a large
    /// payload before exiting (not "never reads at all") must still be a
    /// non-fatal, logged outcome -- the broken pipe fires once the agent's
    /// read end closes regardless of how much it already consumed.
    #[tokio::test]
    async fn an_agent_that_reads_only_part_of_the_payload_then_exits_does_not_fail_the_invocation()
    {
        let dir = TempDir::new().unwrap();
        let cmd = AgentCommand::new("sh", ["-c", "head -c 100 > /dev/null; exit 0"]);
        let child = spawn(&cmd, dir.path()).unwrap();
        let large_payload = "x".repeat(200_000);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            wait(child, "sh", Some(large_payload), CancellationToken::new()),
        )
        .await
        .expect("wait must not hang when the agent only partially reads stdin");

        let outcome = result.expect(
            "an agent reading only part of the payload before exiting must not fail the run",
        );
        assert_eq!(outcome.exit_code, 0);
    }

    /// Adversarial angle: an agent that explicitly closes its stdin file
    /// descriptor mid-run (rather than exiting outright) must still see the
    /// write fail as a non-fatal broken pipe -- and `wait` must not hang
    /// waiting for the write to somehow complete once the read side is
    /// gone, even though the process itself keeps running for a while
    /// afterwards.
    #[tokio::test]
    async fn an_agent_that_closes_stdin_mid_write_while_continuing_to_run_does_not_fail_the_invocation(
    ) {
        let dir = TempDir::new().unwrap();
        let cmd = AgentCommand::new("sh", ["-c", "exec 0<&-; sleep 0.3; exit 0"]);
        let child = spawn(&cmd, dir.path()).unwrap();
        let large_payload = "x".repeat(200_000);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            wait(child, "sh", Some(large_payload), CancellationToken::new()),
        )
        .await
        .expect("wait must not hang when the agent closes stdin mid-write and keeps running");

        let outcome = result.expect(
            "an agent that closes stdin mid-write but keeps running must not fail the invocation",
        );
        assert_eq!(outcome.exit_code, 0);
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
