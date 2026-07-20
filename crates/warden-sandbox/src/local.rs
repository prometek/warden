//! [`LocalSandbox`]: strict behavioural parity with the process isolation
//! `warden::process` applied by hand before this issue -- `env_clear()`,
//! `cwd` pointed at the worktree, `kill_on_drop`, stdin/stdout/stderr piped.
//! **No container, on purpose** -- "Local" means exactly what it says: the
//! agent's process runs directly on this host, under this same OS user, the
//! same isolation warden always applied. `DockerSandbox` (#49) is where
//! actual container isolation is added; this type exists so today's default
//! behaviour has a name and a seam to be selected through, not to change
//! what that behaviour is.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Mutex;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Child;
use tokio_util::sync::CancellationToken;

use crate::error::{Result, SandboxError};
use crate::{Command, ExecuteOptions, Execution, ExecutionResult, Sandbox, SandboxId, SandboxSpec};

/// [`Sandbox`] backed by nothing but [`tokio::process::Command`] -- see this
/// module's own docs on why that is a deliberate parity requirement, not a
/// placeholder. Holds an in-memory `id -> cwd` table rather than any real OS
/// resource: [`LocalSandbox::create`]/[`LocalSandbox::destroy`] are pure
/// bookkeeping (no container to create or tear down), but the seam still
/// requires an id to bind a [`Command`] to *which* worktree it runs against,
/// exactly as a container backend will need one to bind a `docker exec` to
/// *which* container.
#[derive(Default)]
pub struct LocalSandbox {
    sandboxes: Mutex<HashMap<SandboxId, PathBuf>>,
}

impl LocalSandbox {
    pub fn new() -> Self {
        Self::default()
    }

    fn cwd_for(&self, id: &SandboxId) -> Result<PathBuf> {
        self.sandboxes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned()
            .ok_or_else(|| SandboxError::UnknownSandbox { id: id.clone() })
    }
}

#[async_trait]
impl Sandbox for LocalSandbox {
    async fn create(&self, spec: SandboxSpec) -> Result<SandboxId> {
        let id = SandboxId::generate();
        self.sandboxes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.clone(), spec.cwd);
        Ok(id)
    }

    async fn execute<'a>(
        &'a self,
        id: &'a SandboxId,
        command: Command,
        options: ExecuteOptions<'a>,
    ) -> Result<Execution<'a>> {
        let cwd = self.cwd_for(id)?;

        // Mirrors `warden::process::spawn_with_extra_env` exactly:
        // `env_clear()` always runs first (agents never inherit the
        // orchestrator's own shell environment), `PATH` is always forwarded
        // on top, and `command.env_allowlist` is resolved from *this*
        // process's own environment one variable at a time -- a missing
        // allowlisted var is logged, not fatal (the tool's own error is
        // downstream, e.g. `claude`'s "Not logged in" if `HOME` never made
        // it through).
        let mut cmd = tokio::process::Command::new(&command.program);
        cmd.args(&command.args)
            .current_dir(&cwd)
            .env_clear()
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Ok(path) = std::env::var("PATH") {
            cmd.env("PATH", path);
        }
        for var_name in &command.env_allowlist {
            match std::env::var(var_name) {
                Ok(value) => {
                    cmd.env(var_name, value);
                }
                Err(_) => {
                    tracing::warn!(
                        var = var_name,
                        program = %command.program,
                        "adapter-requested environment variable is not set in warden's own \
                         process environment; the child will run without it"
                    );
                }
            }
        }

        let child = cmd.spawn().map_err(|source| SandboxError::Spawn {
            program: command.program.clone(),
            source,
        })?;
        let pid = child.id();

        let program = command.program;
        let stdin_payload = command.stdin;
        let cancel = options.cancel;
        let on_stdout_line = options.on_stdout_line;

        Ok(Execution::new(
            pid,
            drain_and_wait(child, program, stdin_payload, cancel, on_stdout_line),
        ))
    }

    async fn destroy(&self, id: SandboxId) -> Result<()> {
        self.sandboxes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
        Ok(())
    }
}

/// The actual spawn-to-completion drain, split out of
/// [`LocalSandbox::execute`] so it can be boxed into the [`Execution`]
/// returned to the caller rather than run inline. Ported from
/// `warden::process::wait_with_progress` verbatim (same deadlock-avoidance
/// property: stdin write, stdout drain, stderr drain, and the wait for exit
/// all run *concurrently* via `tokio::join!`, never sequentially -- see that
/// function's own docs in `warden` for the full reasoning, unchanged here).
async fn drain_and_wait(
    mut child: Child,
    program: String,
    stdin_payload: Option<String>,
    cancel: CancellationToken,
    on_stdout_line: Option<&(dyn Fn(&str) + Send + Sync)>,
) -> Result<ExecutionResult> {
    let stdin_handle = child.stdin.take();
    let stdout_handle = child.stdout.take();
    let stderr_handle = child.stderr.take();

    let stdin_task = async move {
        if let Some(mut stdin_handle) = stdin_handle {
            if let Some(payload) = stdin_payload {
                if let Err(error) = stdin_handle.write_all(payload.as_bytes()).await {
                    classify_stdin_write_error(error)?;
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
                        tracing::warn!(program = %stdout_program, %error, "failed to read agent stdout to completion");
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
                tracing::warn!(program = %stderr_program, %error, "failed to read agent stderr to completion");
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

/// Mirrors `warden::process::classify_stdin_write_error` exactly: a broken
/// pipe (the agent closed or never opened its read side of stdin) is a
/// legitimate outcome, logged but not fatal; anything else fails the
/// execution outright, since the payload is a single JSON object and a
/// partial write is unparsable by construction.
fn classify_stdin_write_error(error: std::io::Error) -> std::result::Result<(), std::io::Error> {
    if error.kind() == std::io::ErrorKind::BrokenPipe {
        tracing::warn!(
            %error,
            "agent closed stdin before the full payload was written; continuing without a \
             guarantee it read the payload"
        );
        Ok(())
    } else {
        Err(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn command(program: &str, args: &[&str]) -> Command {
        Command {
            program: program.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            env_allowlist: Vec::new(),
            stdin: None,
        }
    }

    #[tokio::test]
    async fn create_then_execute_runs_the_command_in_the_bound_cwd() {
        let dir = TempDir::new().unwrap();
        let sandbox = LocalSandbox::new();
        let id = sandbox
            .create(SandboxSpec {
                cwd: dir.path().to_path_buf(),
            })
            .await
            .unwrap();

        let execution = sandbox
            .execute(&id, command("pwd", &[]), ExecuteOptions::default())
            .await
            .unwrap();
        let outcome = execution.wait().await.unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(
            outcome.stdout.trim(),
            dir.path().canonicalize().unwrap().to_str().unwrap()
        );
    }

    #[tokio::test]
    async fn execute_exposes_the_pid_before_completion() {
        let dir = TempDir::new().unwrap();
        let sandbox = LocalSandbox::new();
        let id = sandbox
            .create(SandboxSpec {
                cwd: dir.path().to_path_buf(),
            })
            .await
            .unwrap();

        let execution = sandbox
            .execute(
                &id,
                command("sh", &["-c", "sleep 0.2"]),
                ExecuteOptions::default(),
            )
            .await
            .unwrap();
        assert!(execution.pid.is_some());
        execution.wait().await.unwrap();
    }

    #[tokio::test]
    async fn reports_a_non_zero_exit_code_as_a_normal_outcome_not_an_error() {
        let dir = TempDir::new().unwrap();
        let sandbox = LocalSandbox::new();
        let id = sandbox
            .create(SandboxSpec {
                cwd: dir.path().to_path_buf(),
            })
            .await
            .unwrap();

        let execution = sandbox
            .execute(
                &id,
                command("sh", &["-c", "exit 7"]),
                ExecuteOptions::default(),
            )
            .await
            .unwrap();
        let outcome = execution.wait().await.unwrap();
        assert_eq!(outcome.exit_code, 7);
    }

    #[tokio::test]
    async fn cancellation_kills_the_child_and_returns_cancelled_error() {
        let dir = TempDir::new().unwrap();
        let sandbox = LocalSandbox::new();
        let id = sandbox
            .create(SandboxSpec {
                cwd: dir.path().to_path_buf(),
            })
            .await
            .unwrap();
        let cancel = CancellationToken::new();

        let execution = sandbox
            .execute(
                &id,
                command("sh", &["-c", "sleep 30"]),
                ExecuteOptions {
                    cancel: cancel.clone(),
                    on_stdout_line: None,
                },
            )
            .await
            .unwrap();
        cancel.cancel();
        let result = execution.wait().await;
        assert!(matches!(result, Err(SandboxError::Cancelled { .. })));
    }

    #[tokio::test]
    async fn stdin_payload_is_written_and_closed_so_the_child_sees_it_and_exits() {
        let dir = TempDir::new().unwrap();
        let sandbox = LocalSandbox::new();
        let id = sandbox
            .create(SandboxSpec {
                cwd: dir.path().to_path_buf(),
            })
            .await
            .unwrap();

        let mut cmd = command("cat", &[]);
        cmd.stdin = Some("hello from warden-sandbox".to_string());
        let execution = sandbox
            .execute(&id, cmd, ExecuteOptions::default())
            .await
            .unwrap();
        let outcome = execution.wait().await.unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout, "hello from warden-sandbox");
    }

    /// Same regression scenario as
    /// `warden::process::writing_a_large_stdin_payload_does_not_deadlock_on_large_stdout`
    /// -- a large stdin payload and a large stdout write, deliberately
    /// sequenced so a naive "write the whole payload, then read stdout"
    /// implementation would hang.
    #[tokio::test]
    async fn writing_a_large_stdin_payload_does_not_deadlock_on_large_stdout() {
        let dir = TempDir::new().unwrap();
        let sandbox = LocalSandbox::new();
        let id = sandbox
            .create(SandboxSpec {
                cwd: dir.path().to_path_buf(),
            })
            .await
            .unwrap();

        let mut cmd = command(
            "sh",
            &["-c", "head -c 200000 /dev/zero; cat > /dev/null; exit 0"],
        );
        cmd.stdin = Some("x".repeat(200_000));

        let execution = sandbox
            .execute(&id, cmd, ExecuteOptions::default())
            .await
            .unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), execution.wait())
            .await
            .expect("execution must not hang when both stdin and stdout exceed the pipe buffer");

        assert_eq!(result.unwrap().exit_code, 0);
    }

    #[tokio::test]
    async fn on_stdout_line_is_invoked_once_per_line_as_it_arrives() {
        let dir = TempDir::new().unwrap();
        let sandbox = LocalSandbox::new();
        let id = sandbox
            .create(SandboxSpec {
                cwd: dir.path().to_path_buf(),
            })
            .await
            .unwrap();

        let seen = std::sync::Mutex::new(Vec::new());
        let on_line = |line: &str| seen.lock().unwrap().push(line.to_string());

        let execution = sandbox
            .execute(
                &id,
                command("sh", &["-c", "echo one; echo two"]),
                ExecuteOptions {
                    cancel: CancellationToken::new(),
                    on_stdout_line: Some(&on_line),
                },
            )
            .await
            .unwrap();
        execution.wait().await.unwrap();

        assert_eq!(seen.into_inner().unwrap(), vec!["one", "two"]);
    }

    #[tokio::test]
    async fn an_agent_that_never_reads_stdin_and_exits_immediately_does_not_fail_the_execution() {
        let dir = TempDir::new().unwrap();
        let sandbox = LocalSandbox::new();
        let id = sandbox
            .create(SandboxSpec {
                cwd: dir.path().to_path_buf(),
            })
            .await
            .unwrap();

        let mut cmd = command("sh", &["-c", "exit 0"]);
        cmd.stdin = Some("x".repeat(200_000));

        let execution = sandbox
            .execute(&id, cmd, ExecuteOptions::default())
            .await
            .unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), execution.wait())
            .await
            .expect("execution must not hang on a broken pipe");

        let outcome = result.expect("a broken pipe from an agent that ignores stdin must not fail");
        assert_eq!(outcome.exit_code, 0);
    }

    #[tokio::test]
    async fn spawn_of_a_nonexistent_program_reports_a_typed_error() {
        let dir = TempDir::new().unwrap();
        let sandbox = LocalSandbox::new();
        let id = sandbox
            .create(SandboxSpec {
                cwd: dir.path().to_path_buf(),
            })
            .await
            .unwrap();

        let result = sandbox
            .execute(
                &id,
                command("this-program-does-not-exist-anywhere", &[]),
                ExecuteOptions::default(),
            )
            .await;
        assert!(matches!(result, Err(SandboxError::Spawn { .. })));
    }

    /// Uses `CARGO_MANIFEST_DIR` -- reliably set by `cargo test` in this
    /// process's own environment, read-only here -- rather than
    /// `std::env::set_var`: mutating global process environment state would
    /// be both `unsafe` and carry cross-test interference risk under a
    /// parallel test runner (same reasoning
    /// `warden::process`'s own `spawn_tui_attach_inherits_the_full_parent_environment`
    /// test documents for the identical trade-off).
    #[tokio::test]
    async fn env_clear_means_an_unallowlisted_variable_never_reaches_the_child() {
        assert!(
            std::env::var("CARGO_MANIFEST_DIR").is_ok(),
            "precondition: cargo test sets CARGO_MANIFEST_DIR"
        );
        let dir = TempDir::new().unwrap();
        let sandbox = LocalSandbox::new();
        let id = sandbox
            .create(SandboxSpec {
                cwd: dir.path().to_path_buf(),
            })
            .await
            .unwrap();

        let execution = sandbox
            .execute(
                &id,
                command("sh", &["-c", "echo \"[$CARGO_MANIFEST_DIR]\""]),
                ExecuteOptions::default(),
            )
            .await
            .unwrap();
        let outcome = execution.wait().await.unwrap();

        assert_eq!(outcome.stdout.trim(), "[]");
    }

    #[tokio::test]
    async fn env_allowlist_forwards_only_the_named_variables() {
        let expected = std::env::var("CARGO_MANIFEST_DIR")
            .expect("precondition: cargo test sets CARGO_MANIFEST_DIR");
        let dir = TempDir::new().unwrap();
        let sandbox = LocalSandbox::new();
        let id = sandbox
            .create(SandboxSpec {
                cwd: dir.path().to_path_buf(),
            })
            .await
            .unwrap();

        let mut cmd = command("sh", &["-c", "echo \"[$CARGO_MANIFEST_DIR]\""]);
        cmd.env_allowlist = vec!["CARGO_MANIFEST_DIR".to_string()];

        let execution = sandbox
            .execute(&id, cmd, ExecuteOptions::default())
            .await
            .unwrap();
        let outcome = execution.wait().await.unwrap();

        assert_eq!(outcome.stdout.trim(), format!("[{expected}]"));
    }

    #[tokio::test]
    async fn execute_with_an_unknown_sandbox_id_reports_a_typed_error() {
        let sandbox = LocalSandbox::new();
        let bogus_id = SandboxId::generate();

        let result = sandbox
            .execute(&bogus_id, command("true", &[]), ExecuteOptions::default())
            .await;
        assert!(matches!(result, Err(SandboxError::UnknownSandbox { .. })));
    }

    #[tokio::test]
    async fn destroy_is_idempotent_for_an_id_that_was_never_created() {
        let sandbox = LocalSandbox::new();
        assert!(sandbox.destroy(SandboxId::generate()).await.is_ok());
    }

    #[tokio::test]
    async fn destroy_then_execute_reports_unknown_sandbox() {
        let dir = TempDir::new().unwrap();
        let sandbox = LocalSandbox::new();
        let id = sandbox
            .create(SandboxSpec {
                cwd: dir.path().to_path_buf(),
            })
            .await
            .unwrap();
        sandbox.destroy(id.clone()).await.unwrap();

        let result = sandbox
            .execute(&id, command("true", &[]), ExecuteOptions::default())
            .await;
        assert!(matches!(result, Err(SandboxError::UnknownSandbox { .. })));
    }
}
