//! The spawn-to-completion drain shared by every [`crate::Sandbox`] backend
//! that spawns a real child process directly (today: [`crate::LocalSandbox`]
//! and [`crate::DockerSandbox`] -- the latter spawns `docker run` itself as
//! its own direct child, so it needs the exact same stdin/stdout/stderr/wait
//! concurrency, not a second copy of it).
//!
//! Factored out of `LocalSandbox::execute` during #49 review (ADR-0015
//! explicitly flags a second copy of this logic as the mistake to avoid --
//! `warden::process::wait_with_progress`/`spawn_with_extra_env` were deleted
//! for exactly that reason when #50 landed): this is now the *only* place
//! the deadlock-avoidance property below is implemented, for either backend.

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Child;
use tokio_util::sync::CancellationToken;

use crate::error::{Result, SandboxError};
use crate::ExecutionResult;

/// The actual spawn-to-completion drain: stdin write, stdout drain, stderr
/// drain, and the wait for exit all run *concurrently* via `tokio::join!`,
/// never sequentially -- a naive sequential implementation deadlocks the
/// moment both a stdin payload and the child's own stdout exceed the OS pipe
/// buffer (the child blocks writing stdout while this side is still blocked
/// writing stdin). Cancellation is a biased `tokio::select!` arm that kills
/// the child and returns a typed error the moment the token fires, in
/// preference to ever polling the join future.
///
/// `program` is a caller-chosen label for the process actually spawned --
/// [`crate::LocalSandbox`] passes the agent's own `command.program`;
/// [`crate::DockerSandbox`] passes a label naming the `docker` client itself
/// (it is what was actually spawned; see that module's own docs for why a
/// `Wait`/`StdinWrite`/`Cancelled` error surfacing here describes a
/// docker-level failure, not an agent-level one).
pub(crate) async fn drain_and_wait(
    mut child: Child,
    program: String,
    stdin_payload: Option<String>,
    cancel: CancellationToken,
    on_stdout_line: Option<&(dyn Fn(&str) + Send + Sync)>,
) -> Result<ExecutionResult> {
    let stdin_handle = child.stdin.take();
    let stdout_handle = child.stdout.take();
    let stderr_handle = child.stderr.take();

    let stdin_program = program.clone();
    let stdin_task = async move {
        if let Some(mut stdin_handle) = stdin_handle {
            if let Some(payload) = stdin_payload {
                if let Err(error) = stdin_handle.write_all(payload.as_bytes()).await {
                    classify_stdin_write_error(error, &stdin_program)?;
                }
            }
            // Dropping `stdin_handle` here closes the write half, signalling
            // EOF -- required even with no payload to write.
        }
        Ok::<(), std::io::Error>(())
    };
    let stdout_program = program.clone();
    let stdout_task = async move {
        let mut buf = Vec::new();
        if let Some(stdout_handle) = stdout_handle {
            let mut reader = BufReader::new(stdout_handle);
            let mut line = Vec::new();
            loop {
                line.clear();
                match reader.read_until(b'\n', &mut line).await {
                    Ok(0) => break, // EOF
                    Ok(_) => {
                        buf.extend_from_slice(&line);
                        if let Some(callback) = on_stdout_line {
                            let text = String::from_utf8_lossy(&line);
                            let trimmed = text.trim_end_matches(['\n', '\r']);
                            if !trimmed.is_empty() {
                                callback(trimmed);
                            }
                        }
                    }
                    Err(error) => {
                        tracing::warn!(command = %stdout_program, %error, "failed to read agent stdout to completion");
                        break;
                    }
                }
            }
        }
        buf
    };
    let stderr_program = program.clone();
    let stderr_task = async move {
        let mut buf = Vec::new();
        if let Some(mut stderr_handle) = stderr_handle {
            if let Err(error) = stderr_handle.read_to_end(&mut buf).await {
                tracing::warn!(command = %stderr_program, %error, "failed to read agent stderr to completion");
            }
        }
        buf
    };

    tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            let _ = child.kill().await;
            Err(SandboxError::Cancelled { program: program.clone() })
        }
        result = async {
            let (stdin_result, stdout_buf, stderr_buf, status_result) =
                tokio::join!(stdin_task, stdout_task, stderr_task, child.wait());
            let status = status_result.map_err(|source| SandboxError::Wait {
                program: program.clone(),
                source,
            })?;
            stdin_result.map_err(|source| SandboxError::StdinWrite {
                program: program.clone(),
                source,
            })?;
            Ok(ExecutionResult {
                exit_code: status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&stdout_buf).into_owned(),
                stderr: String::from_utf8_lossy(&stderr_buf).into_owned(),
            })
        } => result,
    }
}

/// A broken pipe (the agent closed or never opened its read side of stdin)
/// is a legitimate outcome, logged but not fatal; anything else fails the
/// execution outright, since the payload is a single JSON object and a
/// partial write is unparsable by construction. `program` is logged under
/// the `command` field name -- unchanged from `warden::process`'s own log
/// shape (issue #50 review, LOW 5) so existing log queries built against it
/// still match, and so the warning says *which* process closed stdin early
/// rather than which of several concurrently-running ones.
fn classify_stdin_write_error(
    error: std::io::Error,
    program: &str,
) -> std::result::Result<(), std::io::Error> {
    if error.kind() == std::io::ErrorKind::BrokenPipe {
        tracing::warn!(
            command = program,
            %error,
            "agent closed stdin before the full payload was written; continuing without a \
             guarantee it read the payload"
        );
        Ok(())
    } else {
        Err(error)
    }
}
