use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Child;
use tokio_util::sync::CancellationToken;

use crate::error::{Result, SandboxError};
use crate::ExecutionResult;

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
