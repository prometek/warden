//! [`LocalSandbox`]: strict behavioural parity with the process isolation `warden::process` applied
//! by hand before this issue.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::drain::drain_and_wait;
use crate::error::{Result, SandboxError};
use crate::{Command, ExecuteOptions, Execution, Sandbox, SandboxId, SandboxSpec};

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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

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
    async fn dropping_the_execution_mid_flight_kills_the_child_via_kill_on_drop_not_the_cancel_path(
    ) {
        let dir = TempDir::new().unwrap();
        let sandbox = LocalSandbox::new();
        let id = sandbox
            .create(SandboxSpec {
                cwd: dir.path().to_path_buf(),
            })
            .await
            .unwrap();
        let marker = dir.path().join("still-alive-after-sleep");

        let execution = sandbox
            .execute(
                &id,
                command(
                    "sh",
                    &["-c", &format!("sleep 1; touch {}", marker.display())],
                ),
                ExecuteOptions::default(),
            )
            .await
            .unwrap();

        let timed_out =
            tokio::time::timeout(std::time::Duration::from_millis(200), execution.wait())
                .await
                .is_err();
        assert!(
            timed_out,
            "the timeout must fire while the child is still sleeping, for this test to mean \
             anything"
        );

        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        assert!(
            !marker.exists(),
            "kill_on_drop must terminate the child when the Execution future is dropped \
             mid-flight, independent of the explicit cancel-token path"
        );
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
    async fn on_stdout_line_skips_blank_lines() {
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
                command("sh", &["-c", "printf 'a\\n\\nb\\n'"]),
                ExecuteOptions {
                    cancel: CancellationToken::new(),
                    on_stdout_line: Some(&on_line),
                },
            )
            .await
            .unwrap();
        execution.wait().await.unwrap();

        assert_eq!(seen.into_inner().unwrap(), vec!["a", "b"]);
    }

    #[tokio::test]
    async fn on_stdout_line_is_invoked_for_a_final_line_with_no_trailing_newline() {
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
                command("sh", &["-c", "printf 'no newline at the end'"]),
                ExecuteOptions {
                    cancel: CancellationToken::new(),
                    on_stdout_line: Some(&on_line),
                },
            )
            .await
            .unwrap();
        execution.wait().await.unwrap();

        assert_eq!(seen.into_inner().unwrap(), vec!["no newline at the end"]);
    }

    #[tokio::test]
    async fn does_not_deadlock_on_large_newline_free_stdout_with_a_callback_attached() {
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
        let callback_invocations = std::sync::atomic::AtomicUsize::new(0);
        let on_line = |_line: &str| {
            callback_invocations.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        };

        let execution = sandbox
            .execute(
                &id,
                cmd,
                ExecuteOptions {
                    cancel: CancellationToken::new(),
                    on_stdout_line: Some(&on_line),
                },
            )
            .await
            .unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), execution.wait())
            .await
            .expect("execution must not hang when stdout has no newlines at all");

        assert_eq!(result.unwrap().exit_code, 0);
        assert_eq!(
            callback_invocations.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the whole newline-free chunk must be delivered as exactly one line, at EOF"
        );
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
