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
//! # Issue #26: [`validate_agent_program`], a belt-and-braces guard on `program`
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

use std::path::Path;

use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;
use warden_core::AgentRole;

use crate::error::ProcessError;
use crate::path_util::canonicalize_best_effort;

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

/// Guards a reviewer/tester `program` against resolving to a path the coder
/// controls (issue #26, belt-and-braces): no adapter shipped today can
/// actually trigger this (see this module's own docs), but nothing stops a
/// future one from naming a script inside the repo under review, and that
/// would defeat the entire point of running the reviewer/tester as an
/// independent gate. Always `Ok(())` for `AgentRole::Coder` -- the coder
/// already runs with full repo access and is the repo's own untrusted role
/// in the first place (`agent_def`'s own module docs), so there is nothing
/// to protect it from here.
///
/// Refuses `program` when it is:
/// - **a relative path** (contains a path separator and is not absolute):
///   resolves against `worktree_path` (the child chdirs there before exec)
///   -- exactly the `./reviewer.sh`-means-the-coder's-own-copy hazard
///   [`spawn`]'s own docs describe.
/// - **an absolute path that resolves inside `worktree_path`, `repo_path`,
///   or `run_worktrees_root`**: the role's own checked-out worktree, the
///   run's base repository, or *any* role's worktree for this run
///   (`<warden_home>/worktrees/<run_id>/`). Issue #26 review, MEDIUM: the
///   original check covered only the checked role's own worktree and the
///   base repo, leaving the *coder's* own worktree
///   (`<run_worktrees_root>/coder`) unchecked -- the most coder-controllable
///   directory on disk, since the coder runs with `Bash` there and writes
///   freely, including files it never commits. `worktree_path` is always a
///   subdirectory of `run_worktrees_root`, so the `run_worktrees_root` check
///   alone already subsumes it; the separate `worktree_path` check is kept
///   only for the more specific error message when the program resolves
///   inside the checked role's *own* worktree specifically.
///
/// A **bare program name with no path separator at all** (`"claude"`,
/// `"echo"`) is always allowed: it resolves via `PATH`
/// (`Command::new`/`execvp` semantics), never against `worktree_path`, so it
/// carries none of the above hazard regardless of what the coder committed.
///
/// `worktree_path`, `repo_path`, and `run_worktrees_root` are all
/// canonicalized before the containment check (walking up to the nearest
/// existing ancestor for a `program` path that doesn't exist on disk -- see
/// `canonicalize_best_effort`), so a `..`-laden or symlink-relative
/// `program` can't slip past a purely lexical comparison. If canonicalizing
/// `program` itself fails for a reason other than "doesn't exist" (e.g. a
/// permissions error walking its ancestors), this fails closed with
/// [`ProcessError::UntrustedAgentProgram`] naming that reason, rather than
/// silently skipping the containment check it could no longer perform
/// (code-standards.md: "no silent fallback").
pub fn validate_agent_program(
    role: AgentRole,
    program: &str,
    worktree_path: &Path,
    repo_path: &Path,
    run_worktrees_root: &Path,
) -> Result<(), ProcessError> {
    if role == AgentRole::Coder {
        return Ok(());
    }

    if !program.contains(std::path::MAIN_SEPARATOR) && !program.contains('/') {
        // No path separator at all (checked for both this platform's own
        // separator and `/`, since a Windows build must still refuse a
        // Unix-style `agents/reviewer.sh` argument) -- a bare name, resolved
        // via `PATH`, never against `worktree_path`.
        return Ok(());
    }

    let candidate = Path::new(program);
    if !candidate.is_absolute() {
        return Err(ProcessError::UntrustedAgentProgram {
            role: role.as_str().to_string(),
            program: program.to_string(),
            reason: format!(
                "relative path -- would resolve against {}, the role's own worktree (a \
                 checkout of the repo the coder can write to)",
                worktree_path.display()
            ),
        });
    }

    let canonical_candidate = canonicalize_best_effort(candidate).map_err(|source| {
        ProcessError::UntrustedAgentProgram {
            role: role.as_str().to_string(),
            program: program.to_string(),
            reason: format!(
                "cannot resolve its real location to verify it is outside the repo under \
                 review: {source}"
            ),
        }
    })?;
    let canonical_worktree = canonicalize_best_effort(worktree_path).map_err(|source| {
        ProcessError::UntrustedAgentProgram {
            role: role.as_str().to_string(),
            program: program.to_string(),
            reason: format!(
                "cannot resolve the role's own worktree ({}) to verify this program is outside \
                 it: {source}",
                worktree_path.display()
            ),
        }
    })?;
    let canonical_repo = canonicalize_best_effort(repo_path).map_err(|source| {
        ProcessError::UntrustedAgentProgram {
            role: role.as_str().to_string(),
            program: program.to_string(),
            reason: format!(
                "cannot resolve the run's base repository ({}) to verify this program is \
                 outside it: {source}",
                repo_path.display()
            ),
        }
    })?;
    let canonical_run_worktrees_root =
        canonicalize_best_effort(run_worktrees_root).map_err(|source| {
            ProcessError::UntrustedAgentProgram {
                role: role.as_str().to_string(),
                program: program.to_string(),
                reason: format!(
                    "cannot resolve this run's own worktrees root ({}) to verify this program is \
                 outside it: {source}",
                    run_worktrees_root.display()
                ),
            }
        })?;

    if canonical_candidate.starts_with(&canonical_worktree) {
        return Err(ProcessError::UntrustedAgentProgram {
            role: role.as_str().to_string(),
            program: program.to_string(),
            reason: format!(
                "resolves inside the role's own worktree ({}) -- a checkout of the repo the \
                 coder can write to",
                worktree_path.display()
            ),
        });
    }
    // Issue #26 review, MEDIUM: catches a program under *another* role's
    // worktree for this same run (most importantly the coder's own,
    // `<run_worktrees_root>/coder` -- the coder writes there freely via
    // `Bash`, including files it never commits) -- the check above only
    // ever covered the checked role's own worktree.
    if canonical_candidate.starts_with(&canonical_run_worktrees_root) {
        return Err(ProcessError::UntrustedAgentProgram {
            role: role.as_str().to_string(),
            program: program.to_string(),
            reason: format!(
                "resolves inside this run's own worktrees ({}) -- e.g. the coder's, which the \
                 coder writes to freely via Bash, including files it never commits",
                run_worktrees_root.display()
            ),
        });
    }
    if canonical_candidate.starts_with(&canonical_repo) {
        return Err(ProcessError::UntrustedAgentProgram {
            role: role.as_str().to_string(),
            program: program.to_string(),
            reason: format!(
                "resolves inside the run's base repository ({}), which the coder can write to \
                 and commit into",
                repo_path.display()
            ),
        });
    }

    Ok(())
}

/// Spawns `command` with `cwd` set (code-standards.md: "Agent Subprocess
/// Protocol"). The environment is not inherited from the current process —
/// `env_clear()` always runs first, only `PATH` forwarded on top
/// (Architecture.md §10, "Isolation environnement des sous-processus").
///
/// Issue #50 review, MEDIUM 3: this and [`wait`] no longer sit on the
/// coder/reviewer/tester invocation path at all — every agent runs through
/// `warden_sandbox::Sandbox` now (`warden_sandbox::LocalSandbox::execute` is
/// the strict-parity port of what used to live here, including its own
/// env-allowlist forwarding and per-line progress callback), routed via
/// `orchestrator::Orchestrator::run_agent`. What remains here is the
/// narrower subset the Evidence Capture Adapter (`evidence.rs`, via
/// [`spawn_and_wait`]) actually needs: no extra env allowlist, no per-line
/// callback — carrying that now-dead functionality forward as a second,
/// separately maintained copy of `LocalSandbox`'s own deadlock-avoidance
/// logic was exactly the drift risk two copies of the same subprocess-drain
/// code creates. `[`validate_agent_program`]` is unaffected by this — it is
/// still the one checkpoint every coder/reviewer/tester spawn goes through,
/// just called from `Orchestrator::run_agent` before the sandbox's own
/// `execute`, not before this function.
///
/// **A relative `command.program` resolves against `cwd`** — the child
/// chdirs before exec. Long-standing behaviour, documented here rather than
/// changed — refusing relative paths is a product decision, and it would
/// break the plain-script case a custom `AgentCommand` might still exist to
/// serve for a non-agent subprocess (evidence capture).
///
/// stdin is piped (ADR-0012, issue #20 Scope B heritage) rather than
/// inherited, so [`wait`]'s optional payload write never leaks the
/// orchestrator's own stdin into the child. A child that never reads stdin
/// at all is *not* unconditionally unaffected: a payload small enough to fit
/// in the OS pipe buffer (typically 64KiB) is written without blocking and
/// simply sits there unread until the child exits, but a larger payload
/// blocks [`wait`]'s write until either the child reads enough to make room
/// or exits and closes its read end (a broken pipe, handled explicitly —
/// see [`wait`]).
///
/// Returns the still-running [`Child`] so the caller can read its PID
/// (`child.id()`) and persist it before calling [`wait`].
pub fn spawn(command: &AgentCommand, cwd: &Path) -> Result<Child, ProcessError> {
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

    cmd.spawn().map_err(|source| ProcessError::Spawn {
        command: command.program.clone(),
        source,
    })
}

/// Awaits a previously [`spawn`]ed child, cancellable via `cancel`.
///
/// If `cancel` fires first, the child is killed and
/// [`ProcessError::Cancelled`] is returned.
///
/// `stdin_payload`, if given, is written to the child's stdin and the write
/// half is then closed (dropped) so the child sees EOF rather than hanging
/// forever waiting for more input — this happens even when `stdin_payload`
/// is `None`, since a piped stdin ([`spawn`]) that's never closed would
/// otherwise hang a child that reads until EOF before proceeding.
///
/// **Deadlock avoidance**: the write, the stdout/stderr draining, and the
/// wait for exit all run *concurrently* (`tokio::join!`), not sequentially.
/// Writing the whole payload before draining anything (or draining only
/// after exit, as this function used to) risks a classic pipe deadlock: a
/// child that interleaves reading stdin with writing enough stdout/stderr to
/// fill the OS pipe buffer (typically 64KiB) before it has consumed all of
/// stdin will block on its own full stdout/stderr pipe; meanwhile we'd be
/// blocked writing to a stdin the child has stopped reading — neither side
/// can make progress. Running all four concurrently means each blocked
/// read/write just yields to the executor, and progress on any one of them
/// unblocks the others.
///
/// **Stdin write failures** (H1, issue #20 review): a broken pipe (the
/// child closed or never read stdin before exiting) is logged at `warn` and
/// treated as a normal, non-fatal outcome — see
/// [`classify_stdin_write_error`]. Any other write error fails this call
/// with [`ProcessError::StdinWrite`] instead of letting the run continue
/// silently: `stdin_payload` is always a single JSON object, so a partial
/// write is unparsable by the child by construction, and there is no
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

/// Spawns `<tui_binary> attach --run-id <run_id> --warden-home <warden_home>`
/// in the foreground (issue #32, `warden run --tui`), attaching it to the run
/// that just started. Unlike [`spawn`], stdio is **inherited** rather than
/// piped -- the whole point is for this child to take over the launch
/// terminal exactly as if the user had typed the `warden-tui attach` command
/// themselves -- and the environment is inherited rather than cleared:
/// `warden-tui` is a trusted first-party binary from this same install, not
/// an agent under the Agent Subprocess Protocol (code-standards.md), so none
/// of that isolation applies to it.
///
/// `warden run --tui` treats this child's exit -- for *any* reason (the user
/// quit with `q`/`Esc`/Ctrl-C, or it was killed/crashed) -- as cancelling the
/// run (issue #32 decision: "la sortie de la TUI annule le run"). Ctrl-C
/// specifically needs `warden_tui`'s own `is_quit` to treat it as a quit key
/// first: raw mode disables the terminal's `SIGINT`-on-Ctrl-C generation
/// entirely (`cfmakeraw` clears `ISIG`), so relying on the signal reaching
/// this process's own group would not work while `warden-tui` holds the tty.
pub fn spawn_tui_attach(
    tui_binary: &Path,
    run_id: &str,
    warden_home: &Path,
) -> Result<Child, ProcessError> {
    Command::new(tui_binary)
        .arg("attach")
        .arg("--run-id")
        .arg(run_id)
        .arg("--warden-home")
        .arg(warden_home)
        .spawn()
        .map_err(|source| ProcessError::Spawn {
            command: tui_binary.display().to_string(),
            source,
        })
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
    use std::path::PathBuf;
    use tempfile::TempDir;

    // -----------------------------------------------------------------
    // `validate_agent_program` (issue #26, belt-and-braces)
    // -----------------------------------------------------------------

    /// A dedicated `<run_worktrees_root>/<role>` layout, mirroring what
    /// `WorktreeManager::create` actually produces
    /// (`<warden_home>/worktrees/<run_id>/<role>`) -- used by every test
    /// below instead of an unrelated bare `TempDir` for `worktree_path`, so
    /// the MEDIUM (issue #26 review) coverage of *other* roles' worktrees
    /// under the same `run_worktrees_root` has something real to check.
    struct WorktreeLayout {
        run_worktrees_root: TempDir,
    }

    impl WorktreeLayout {
        fn new() -> Self {
            Self {
                run_worktrees_root: TempDir::new().unwrap(),
            }
        }

        fn role_worktree(&self, role: &str) -> PathBuf {
            let path = self.run_worktrees_root.path().join(role);
            std::fs::create_dir_all(&path).unwrap();
            path
        }
    }

    #[test]
    fn a_bare_program_name_with_no_separator_is_always_allowed_for_reviewer_and_tester() {
        let layout = WorktreeLayout::new();
        let worktree = layout.role_worktree("reviewer");
        let repo = TempDir::new().unwrap();
        for role in [AgentRole::Reviewer, AgentRole::Tester] {
            assert!(validate_agent_program(
                role,
                "claude",
                &worktree,
                repo.path(),
                layout.run_worktrees_root.path(),
            )
            .is_ok());
        }
    }

    #[test]
    fn a_relative_path_is_refused_for_reviewer_and_tester() {
        let layout = WorktreeLayout::new();
        let worktree = layout.role_worktree("reviewer");
        let repo = TempDir::new().unwrap();
        for role in [AgentRole::Reviewer, AgentRole::Tester] {
            let error = validate_agent_program(
                role,
                "./reviewer.sh",
                &worktree,
                repo.path(),
                layout.run_worktrees_root.path(),
            )
            .unwrap_err();
            assert!(matches!(error, ProcessError::UntrustedAgentProgram { .. }));
            assert!(error.to_string().contains("./reviewer.sh"), "{error}");
        }
    }

    #[test]
    fn an_absolute_path_inside_the_role_worktree_is_refused() {
        let layout = WorktreeLayout::new();
        let worktree = layout.role_worktree("reviewer");
        let repo = TempDir::new().unwrap();
        let program = worktree.join("reviewer.sh");
        std::fs::write(&program, "#!/bin/sh\n").unwrap();

        let error = validate_agent_program(
            AgentRole::Reviewer,
            program.to_str().unwrap(),
            &worktree,
            repo.path(),
            layout.run_worktrees_root.path(),
        )
        .unwrap_err();
        assert!(matches!(error, ProcessError::UntrustedAgentProgram { .. }));
    }

    #[test]
    fn an_absolute_path_inside_the_run_base_repo_is_refused() {
        let layout = WorktreeLayout::new();
        let worktree = layout.role_worktree("tester");
        let repo = TempDir::new().unwrap();
        let program = repo.path().join(".warden/agents/reviewer.sh");
        std::fs::create_dir_all(program.parent().unwrap()).unwrap();
        std::fs::write(&program, "#!/bin/sh\n").unwrap();

        let error = validate_agent_program(
            AgentRole::Tester,
            program.to_str().unwrap(),
            &worktree,
            repo.path(),
            layout.run_worktrees_root.path(),
        )
        .unwrap_err();
        assert!(matches!(error, ProcessError::UntrustedAgentProgram { .. }));
    }

    /// Issue #26 review, MEDIUM: the original guard only checked the
    /// checked role's own worktree and the base repo -- leaving the
    /// *coder's* own worktree, under the same `run_worktrees_root`, entirely
    /// unchecked even though it is the most coder-controllable directory on
    /// disk (the coder runs with `Bash` there and writes freely, including
    /// files it never commits). A reviewer `program` naming a script under
    /// the coder's worktree must now be refused too.
    #[test]
    fn an_absolute_path_inside_the_coders_own_worktree_for_this_run_is_refused() {
        let layout = WorktreeLayout::new();
        let reviewer_worktree = layout.role_worktree("reviewer");
        let coder_worktree = layout.role_worktree("coder");
        let repo = TempDir::new().unwrap();
        let program = coder_worktree.join("tool.sh");
        std::fs::write(&program, "#!/bin/sh\n").unwrap();

        let error = validate_agent_program(
            AgentRole::Reviewer,
            program.to_str().unwrap(),
            &reviewer_worktree,
            repo.path(),
            layout.run_worktrees_root.path(),
        )
        .unwrap_err();
        assert!(matches!(error, ProcessError::UntrustedAgentProgram { .. }));
        assert!(error.to_string().contains("run's own worktrees"), "{error}");
    }

    #[test]
    fn an_absolute_path_outside_the_worktree_the_repo_and_the_run_worktrees_root_is_allowed() {
        let layout = WorktreeLayout::new();
        let worktree = layout.role_worktree("reviewer");
        let repo = TempDir::new().unwrap();
        let elsewhere = TempDir::new().unwrap();
        let program = elsewhere.path().join("some-tool");
        std::fs::write(&program, "#!/bin/sh\n").unwrap();

        assert!(validate_agent_program(
            AgentRole::Reviewer,
            program.to_str().unwrap(),
            &worktree,
            repo.path(),
            layout.run_worktrees_root.path(),
        )
        .is_ok());
    }

    /// The whole point of this guard: it must never apply to the coder,
    /// which already has full repo access and is the repo's own untrusted
    /// role in the first place -- even a program that would be refused for
    /// the reviewer/tester must pass unchanged for the coder.
    #[test]
    fn the_coder_is_never_subject_to_this_guard() {
        let layout = WorktreeLayout::new();
        let worktree = layout.role_worktree("coder");
        let repo = TempDir::new().unwrap();
        let program = repo.path().join(".warden/agents/coder.sh");
        std::fs::create_dir_all(program.parent().unwrap()).unwrap();
        std::fs::write(&program, "#!/bin/sh\n").unwrap();

        assert!(validate_agent_program(
            AgentRole::Coder,
            program.to_str().unwrap(),
            &worktree,
            repo.path(),
            layout.run_worktrees_root.path(),
        )
        .is_ok());
        assert!(validate_agent_program(
            AgentRole::Coder,
            "./coder.sh",
            &worktree,
            repo.path(),
            layout.run_worktrees_root.path(),
        )
        .is_ok());
    }

    /// A `program` that doesn't exist on disk at all must still be checked
    /// against the containment rule (via `canonicalize_best_effort`'s
    /// ancestor walk), not silently allowed just because it can't be
    /// canonicalized outright.
    #[test]
    fn a_nonexistent_absolute_path_inside_the_worktree_is_still_refused() {
        let layout = WorktreeLayout::new();
        let worktree = layout.role_worktree("reviewer");
        let repo = TempDir::new().unwrap();
        let program = worktree.join("does-not-exist-yet.sh");

        let error = validate_agent_program(
            AgentRole::Reviewer,
            program.to_str().unwrap(),
            &worktree,
            repo.path(),
            layout.run_worktrees_root.path(),
        )
        .unwrap_err();
        assert!(matches!(error, ProcessError::UntrustedAgentProgram { .. }));
    }

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

    // Issue #50 review, MEDIUM 3: the `on_stdout_line` callback tests that
    // used to live here (`wait_with_progress_*`) moved to
    // `warden_sandbox::local`'s own test module -- that per-line callback is
    // now dead code on this side (every remaining `wait` caller passes no
    // callback at all; only `warden_sandbox::LocalSandbox::execute` still
    // offers one, to the sandbox seam's own caller). See
    // `warden_sandbox::local::tests::on_stdout_line_skips_blank_lines` and
    // its neighbours for that coverage, unchanged in substance.

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

    /// Issue #32: `spawn_tui_attach` must invoke `<binary> attach --run-id
    /// <id> --warden-home <path>` verbatim. Captures argv to a file instead
    /// of stdout, since [`spawn_tui_attach`]'s whole point is inheriting
    /// stdio (the real `warden-tui` must take over the launch terminal), not
    /// piping it for a test to capture.
    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_tui_attach_passes_the_expected_attach_subcommand_and_flags() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().unwrap();
        let out_file = dir.path().join("captured-args.txt");
        let script_path = dir.path().join("fake-warden-tui");
        std::fs::write(
            &script_path,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{}\"\n",
                out_file.display()
            ),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();

        let warden_home = dir.path().join("home");
        let mut child = spawn_tui_attach(&script_path, "run-123", &warden_home).unwrap();
        let status = child.wait().await.unwrap();
        assert!(status.success());

        let captured = std::fs::read_to_string(&out_file).unwrap();
        assert_eq!(
            captured.lines().collect::<Vec<_>>(),
            vec![
                "attach",
                "--run-id",
                "run-123",
                "--warden-home",
                warden_home.to_str().unwrap(),
            ]
        );
    }

    /// Unlike [`spawn`] (which `env_clear()`s for agent isolation),
    /// `spawn_tui_attach` must inherit the full parent environment --
    /// `warden-tui` is a trusted first-party binary, not an agent under the
    /// Agent Subprocess Protocol. Checked against `PATH`, whatever it
    /// already is in the test process, rather than mutating global process
    /// environment state (which `std::env::set_var` would, unsafely and with
    /// cross-test interference risk under a parallel test runner).
    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_tui_attach_inherits_the_full_parent_environment() {
        use std::os::unix::fs::PermissionsExt;

        let expected_path = std::env::var("PATH").expect("PATH is set in the test process");

        let dir = TempDir::new().unwrap();
        let out_file = dir.path().join("captured-env.txt");
        let script_path = dir.path().join("fake-warden-tui");
        std::fs::write(
            &script_path,
            format!(
                "#!/bin/sh\nprintf '%s' \"$PATH\" > \"{}\"\n",
                out_file.display()
            ),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();

        let mut child =
            spawn_tui_attach(&script_path, "run-123", &dir.path().join("home")).unwrap();
        let status = child.wait().await.unwrap();
        assert!(status.success());

        assert_eq!(std::fs::read_to_string(&out_file).unwrap(), expected_path);
    }

    #[tokio::test]
    async fn spawn_tui_attach_reports_a_typed_error_when_the_binary_does_not_exist() {
        let dir = TempDir::new().unwrap();
        let missing_binary = dir.path().join("does-not-exist");
        let result = spawn_tui_attach(&missing_binary, "run-123", &dir.path().join("home"));
        assert!(matches!(result, Err(ProcessError::Spawn { .. })));
    }
}
