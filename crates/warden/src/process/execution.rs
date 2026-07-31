use super::*;

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
pub(super) fn classify_stdin_write_error(
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
