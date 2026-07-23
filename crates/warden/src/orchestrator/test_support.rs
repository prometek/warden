//! Shared test-only fixtures used across `orchestrator`'s submodule test
//! suites: a real throwaway git repo, deterministic coder/reviewer/tester
//! fixtures, and fake `ToolAdapter`s standing in for a real agent CLI.

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

/// A coder that flips `status.txt` between "broken" and "fixed" each
/// time it runs, and a reviewer that raises a blocking finding only
/// while it reads "broken" — this deterministically exercises exactly
/// one reboucle before converging, without depending on a real AI
/// agent (out of scope for Phase 1; see ADR-0005 for the general
/// subprocess contract this stands in for).
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

/// NDJSON wire format (code-standards.md "Agent Subprocess Protocol",
/// M3): one finding object per line, no wrapping `{"findings": [...]}`.
/// "No findings" is simply no stdout at all.
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

/// A test-only wire shape smuggling an `AgentCommand` fixture through an
/// `AgentDefinition`'s `name` field (issue #24: the real schema has no
/// `program`/`args` at all -- those are entirely a `ToolAdapter`'s own
/// business now). JSON round-trips losslessly, unlike a naive
/// space-join/split, so a fixture with a whitespace-/newline-containing
/// arg (every `sh -c "<script>"` fixture in this file) survives intact.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct SmuggledCommand {
    program: String,
    args: Vec<String>,
}

/// Wraps an `AgentCommand` fixture in the markdown definition
/// `RunConfig` now takes (issue #24), with a fixed test system prompt.
/// See [`definition_with_prompt`] for a caller that needs its own.
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

/// The other half of [`definition`]/[`definition_with_prompt`]'s
/// smuggling.
pub(crate) fn decode_smuggled_command(definition: &AgentDefinition) -> AgentCommand {
    let encoded = definition
        .name
        .as_deref()
        .expect("test definitions always smuggle a command via `name`");
    let smuggled: SmuggledCommand = serde_json::from_str(encoded).expect("valid smuggled command");
    AgentCommand::new(smuggled.program, smuggled.args)
}

/// The identity-mapping fake: decodes exactly what [`definition`]
/// encoded and runs it verbatim. Fills the role the removed, real,
/// shipped-in-production `CommandRunner` (ADR-0013's generic
/// any-program/args runner) used to fill in these tests -- issue #24
/// replaces that generic runner with tool-specific adapters
/// (`ClaudeAdapter` in production), so an identity mapping is now only
/// ever a test double, never something Warden ships.
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

/// Issue #33: a `FakeCommandAdapter` that also translates stdout lines
/// into progress, so a test can exercise the full
/// `warden_sandbox::Sandbox::execute`'s `on_stdout_line` callback ->
/// `ToolAdapter::parse_progress_line` -> `Orchestrator::publish_progress_event`
/// -> `EventBus` pipeline (issue #50: the per-line drain this callback
/// used to reach through `process::wait_with_progress` now lives in
/// `warden_sandbox::LocalSandbox`) without needing a real `claude` CLI.
/// Recognizes any line prefixed
/// `PROGRESS: ` (a made-up convention for this fake only -- real
/// progress recognition is `ClaudeAdapter`'s own `stream-json`-specific
/// concern, unrelated to this marker).
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

/// The tool-adapter seam (issue #24): the orchestrator spawns what the
/// *adapter* returns for a definition, not something read straight out
/// of `RunConfig` -- so a fake adapter can serve every role from
/// abstract definitions that name no real binary at all. Records what it
/// was handed, to prove all three roles are resolved through it.
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
        // The mapping a real adapter does: an abstract definition onto
        // whatever CLI actually implements that role.
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

/// An adapter that refuses every definition -- models a definition this
/// build cannot honour (e.g. one written for a tool this binary has no
/// adapter for).
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

/// Looks up a specific cycle's findings by its 1-based `cycle_number` --
/// used where a test needs to compare more than one cycle's findings
/// separately (unlike a single-cycle run, which can just list every
/// finding for its one cycle).
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
