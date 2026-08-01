use super::*;

/// Spawns `command` with `cwd` set (code-standards.md: "Agent Subprocess Protocol").
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

/// Spawns `<tui_binary> attach --run-id <run_id> --warden-home <warden_home>` in the foreground,
/// attaching it to the run that just started.
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

pub async fn spawn_and_wait(
    command: &AgentCommand,
    cwd: &Path,
    cancel: CancellationToken,
) -> Result<AgentOutcome, ProcessError> {
    let child = spawn(command, cwd)?;
    wait(child, &command.program, None, cancel).await
}
