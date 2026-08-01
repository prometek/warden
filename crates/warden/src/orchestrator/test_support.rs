#![cfg(test)]

use super::*;
use std::process::Command as SyncCommand;
use tempfile::TempDir;

pub(crate) fn init_test_repo() -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    let run = |args: &[&str]| {
        let status = SyncCommand::new("git")
            .current_dir(dir.path())
            .args(args)
            .status()
            .expect("spawn git");
        assert!(status.success(), "git {args:?} failed");
    };
    run(&["init", "--quiet"]);
    run(&["config", "user.email", "test@warden.local"]);
    run(&["config", "user.name", "warden-test"]);
    std::fs::write(dir.path().join("README.md"), "warden test repo\n").unwrap();
    run(&["add", "."]);
    run(&["commit", "--quiet", "-m", "initial commit"]);
    dir
}

pub(crate) fn flip_status_coder() -> AgentCommand {
    AgentCommand::new(
        "sh",
        [
            "-c",
            r#"
                if [ -f status.txt ] && [ "$(cat status.txt)" = "broken" ]; then
                    echo fixed > status.txt
                else
                    echo broken > status.txt
                fi
                git add status.txt
                git -c user.email=test@warden.local -c user.name=warden-test commit -q -m "coder cycle"
                "#,
        ],
    )
}

/// NDJSON wire format (code-standards.md "Agent Subprocess Protocol", M3): one finding object per
/// line, no wrapping `{"findings": [...]}`.
pub(crate) fn status_gated_reviewer() -> AgentCommand {
    AgentCommand::new(
        "sh",
        [
            "-c",
            r#"
                if [ -f status.txt ] && [ "$(cat status.txt)" = "broken" ]; then
                    echo '{"source":"reviewer","severity":"blocking","description":"status is broken"}'
                fi
                "#,
        ],
    )
}

pub(crate) fn always_passing_tester() -> AgentCommand {
    AgentCommand::new("sh", ["-c", "true"])
}

/// A test-only wire shape smuggling an `AgentCommand` fixture through an `AgentDefinition`'s `name`
/// field.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct SmuggledCommand {
    program: String,
    args: Vec<String>,
}

/// Wraps an `AgentCommand` fixture in the markdown definition `RunConfig` now takes, with a fixed
/// test system prompt.
pub(crate) fn definition(command: AgentCommand) -> AgentDefinition {
    definition_with_prompt(command, "test agent system prompt")
}

pub(crate) fn definition_with_prompt(command: AgentCommand, prompt: &str) -> AgentDefinition {
    let encoded = serde_json::to_string(&SmuggledCommand {
        program: command.program,
        args: command.args,
    })
    .unwrap();
    AgentDefinition::new(Some(encoded), None, None, None, prompt).unwrap()
}

/// The other half of [`definition`]/[`definition_with_prompt`]'s smuggling.
pub(crate) fn decode_smuggled_command(definition: &AgentDefinition) -> AgentCommand {
    let encoded = definition
        .name
        .as_deref()
        .expect("test definitions always smuggle a command via `name`");
    let smuggled: SmuggledCommand = serde_json::from_str(encoded).expect("valid smuggled command");
    AgentCommand::new(smuggled.program, smuggled.args)
}

/// The identity-mapping fake: decodes exactly what [`definition`] encoded and runs it verbatim.
pub(crate) struct FakeCommandAdapter;

impl ToolAdapter for FakeCommandAdapter {
    fn build_command(&self, definition: &AgentDefinition) -> Result<AgentCommand> {
        Ok(decode_smuggled_command(definition))
    }

    fn env_allowlist(&self) -> &'static [&'static str] {
        &[]
    }

    fn extract_findings(&self, stdout: &str) -> warden_core::Result<Vec<Finding>> {
        warden_core::parse_findings(stdout)
    }

    fn default_prompt(&self, _role: AgentRole) -> &'static str {
        "unused: every test using this adapter provides an explicit definition"
    }

    fn default_tools(&self, _role: AgentRole) -> Option<&'static str> {
        None
    }
}

pub(crate) struct ProgressReportingAdapter;

impl ToolAdapter for ProgressReportingAdapter {
    fn build_command(&self, definition: &AgentDefinition) -> Result<AgentCommand> {
        Ok(decode_smuggled_command(definition))
    }

    fn env_allowlist(&self) -> &'static [&'static str] {
        &[]
    }

    fn extract_findings(&self, stdout: &str) -> warden_core::Result<Vec<Finding>> {
        warden_core::parse_findings(stdout)
    }

    fn default_prompt(&self, _role: AgentRole) -> &'static str {
        "unused: every test using this adapter provides an explicit definition"
    }

    fn default_tools(&self, _role: AgentRole) -> Option<&'static str> {
        None
    }

    fn parse_progress_line(&self, line: &str) -> Option<String> {
        line.strip_prefix("PROGRESS: ").map(str::to_string)
    }
}

/// The tool-adapter seam: the orchestrator spawns what the *adapter* returns for a definition, not
/// something read straight out of `RunConfig`.
pub(crate) struct FakeRunner {
    resolved_programs: std::sync::Mutex<Vec<String>>,
}

impl FakeRunner {
    pub(crate) fn new() -> Self {
        Self {
            resolved_programs: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl ToolAdapter for FakeRunner {
    fn build_command(&self, definition: &AgentDefinition) -> Result<AgentCommand> {
        let program = decode_smuggled_command(definition).program;
        self.resolved_programs.lock().unwrap().push(program.clone());
        Ok(match program.as_str() {
            "the-coder" => AgentCommand::new(
                "sh",
                [
                    "-c",
                    r#"
                        echo done > work.txt
                        git add work.txt
                        git -c user.email=test@warden.local -c user.name=warden-test commit -q -m "fake coder"
                        "#,
                ],
            ),
            _ => AgentCommand::new("sh", ["-c", "true"]),
        })
    }

    fn env_allowlist(&self) -> &'static [&'static str] {
        &[]
    }

    fn extract_findings(&self, stdout: &str) -> warden_core::Result<Vec<Finding>> {
        warden_core::parse_findings(stdout)
    }

    fn default_prompt(&self, _role: AgentRole) -> &'static str {
        "unused: every test using this adapter provides an explicit definition"
    }

    fn default_tools(&self, _role: AgentRole) -> Option<&'static str> {
        None
    }
}

pub(crate) struct FailingRunner;

impl ToolAdapter for FailingRunner {
    fn build_command(&self, _definition: &AgentDefinition) -> Result<AgentCommand> {
        Err(WardenError::Core(
            warden_core::CoreError::MalformedAgentDefinition(
                "no adapter available for this definition".to_string(),
            ),
        ))
    }

    fn env_allowlist(&self) -> &'static [&'static str] {
        &[]
    }

    fn extract_findings(&self, _stdout: &str) -> warden_core::Result<Vec<Finding>> {
        unreachable!("build_command always fails first")
    }

    fn default_prompt(&self, _role: AgentRole) -> &'static str {
        unreachable!("build_command always fails first")
    }

    fn default_tools(&self, _role: AgentRole) -> Option<&'static str> {
        unreachable!("build_command always fails first")
    }
}

pub(crate) async fn count_runs(pool: &SqlitePool) -> i64 {
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM runs")
        .fetch_one(pool)
        .await
        .unwrap();
    count
}

/// Looks up a specific cycle's findings by its 1-based `cycle_number`.
pub(crate) async fn findings_for_cycle_number(
    pool: &SqlitePool,
    run_id: &str,
    cycle_number: i64,
) -> Vec<Finding> {
    let (cycle_id,): (String,) =
        sqlx::query_as("SELECT id FROM cycles WHERE run_id = ? AND cycle_number = ?")
            .bind(run_id)
            .bind(cycle_number)
            .fetch_one(pool)
            .await
            .unwrap();
    db::list_findings_for_cycle(pool, &cycle_id).await.unwrap()
}

/// Implements [`warden_sandbox::Sandbox`] from scratch -- own bookkeeping, own process spawn, own
/// [`warden_sandbox::Execution`] -- using nothing but this crate's public API.
pub(crate) struct RecordingSandbox {
    calls: std::sync::Mutex<Vec<&'static str>>,
    cwds: std::sync::Mutex<std::collections::HashMap<warden_sandbox::SandboxId, PathBuf>>,
    fail_execute: bool,
}

impl RecordingSandbox {
    pub(crate) fn new(fail_execute: bool) -> Self {
        Self {
            calls: std::sync::Mutex::new(Vec::new()),
            cwds: std::sync::Mutex::new(std::collections::HashMap::new()),
            fail_execute,
        }
    }

    pub(crate) fn calls(&self) -> Vec<&'static str> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl warden_sandbox::Sandbox for RecordingSandbox {
    async fn create(
        &self,
        spec: warden_sandbox::SandboxSpec,
    ) -> warden_sandbox::Result<warden_sandbox::SandboxId> {
        self.calls.lock().unwrap().push("create");
        let id = warden_sandbox::SandboxId::new(uuid::Uuid::new_v4().to_string());
        self.cwds.lock().unwrap().insert(id.clone(), spec.cwd);
        Ok(id)
    }

    async fn execute<'a>(
        &'a self,
        id: &'a warden_sandbox::SandboxId,
        command: warden_sandbox::Command,
        options: warden_sandbox::ExecuteOptions<'a>,
    ) -> warden_sandbox::Result<warden_sandbox::Execution<'a>> {
        self.calls.lock().unwrap().push("execute");
        if self.fail_execute {
            return Err(warden_sandbox::SandboxError::Spawn {
                program: "recording-sandbox-fixture".to_string(),
                source: std::io::Error::from(std::io::ErrorKind::NotFound),
            });
        }
        let cwd = self
            .cwds
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .expect("test fixture: execute always called with an id create just returned");

        let mut spawn = tokio::process::Command::new(&command.program);
        spawn
            .args(&command.args)
            .current_dir(&cwd)
            .kill_on_drop(true)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = spawn
            .spawn()
            .map_err(|source| warden_sandbox::SandboxError::Spawn {
                program: command.program.clone(),
                source,
            })?;
        let pid = child.id();

        let program = command.program;
        let stdin_payload = command.stdin;
        let cancel = options.cancel;

        Ok(warden_sandbox::Execution::new(pid, async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            let mut stdin_handle = child.stdin.take();
            let mut stdout_handle = child.stdout.take();
            let mut stderr_handle = child.stderr.take();

            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    let _ = child.kill().await;
                    Err(warden_sandbox::SandboxError::Cancelled { program })
                }
                result = async {
                    let stdin_task = async {
                        if let Some(mut handle) = stdin_handle.take() {
                            if let Some(payload) = stdin_payload {
                                // A broken pipe here is not a failure -- it means the child exited
                                // without reading its payload, which the fake `claude` scripts
                                // these tests use do routinely.
                                if let Err(error) =
                                    handle.write_all(payload.as_bytes()).await
                                {
                                    if error.kind() != std::io::ErrorKind::BrokenPipe {
                                        return Err(error);
                                    }
                                }
                            }
                        }
                        Ok::<(), std::io::Error>(())
                    };
                    let stdout_task = async {
                        let mut buf = Vec::new();
                        if let Some(mut handle) = stdout_handle.take() {
                            handle.read_to_end(&mut buf).await?;
                        }
                        Ok::<Vec<u8>, std::io::Error>(buf)
                    };
                    let stderr_task = async {
                        let mut buf = Vec::new();
                        if let Some(mut handle) = stderr_handle.take() {
                            handle.read_to_end(&mut buf).await?;
                        }
                        Ok::<Vec<u8>, std::io::Error>(buf)
                    };
                    let (stdin_result, stdout_result, stderr_result, status_result) =
                        tokio::join!(stdin_task, stdout_task, stderr_task, child.wait());
                    let status = status_result.map_err(|source| warden_sandbox::SandboxError::Wait {
                        program: program.clone(),
                        source,
                    })?;
                    stdin_result.map_err(|source| warden_sandbox::SandboxError::StdinWrite {
                        program: program.clone(),
                        source,
                    })?;
                    let stdout_buf = stdout_result.map_err(|source| warden_sandbox::SandboxError::Wait {
                        program: program.clone(),
                        source,
                    })?;
                    let stderr_buf = stderr_result.map_err(|source| warden_sandbox::SandboxError::Wait {
                        program: program.clone(),
                        source,
                    })?;
                    Ok(warden_sandbox::ExecutionResult {
                        exit_code: status.code().unwrap_or(-1),
                        stdout: String::from_utf8_lossy(&stdout_buf).into_owned(),
                        stderr: String::from_utf8_lossy(&stderr_buf).into_owned(),
                    })
                } => result,
            }
        }))
    }

    async fn destroy(&self, id: warden_sandbox::SandboxId) -> warden_sandbox::Result<()> {
        self.calls.lock().unwrap().push("destroy");
        self.cwds.lock().unwrap().remove(&id);
        Ok(())
    }
}
