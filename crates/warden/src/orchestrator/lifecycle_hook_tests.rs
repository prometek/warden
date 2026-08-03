//! End-to-end coverage of issue #106: `HookOutcome::Block` must stop the run at every lifecycle
//! point except [`HookPoint::OnRunEnd`] (best-effort teardown), and `HookOutcome::EmitFindings`
//! must actually reboucle the convergence loop, not just be dropped.

use std::process::Command as SyncCommand;

use async_trait::async_trait;
use tempfile::TempDir;

use super::*;
use crate::hook::Hook;

/// Shared with `gate_tail`'s own tests (`super::super::gate_tail::tests`) -- the same fixture repo
/// and blocking-hook double, so both suites exercise the exact same `Block` behavior.
pub(super) fn init_test_repo() -> TempDir {
    let dir = TempDir::new().unwrap();
    for args in [
        vec!["init", "--quiet"],
        vec!["config", "user.email", "test@warden.local"],
        vec!["config", "user.name", "warden-test"],
    ] {
        assert!(SyncCommand::new("git")
            .current_dir(dir.path())
            .args(args)
            .status()
            .unwrap()
            .success());
    }
    std::fs::write(dir.path().join("README.md"), "seed\n").unwrap();
    assert!(SyncCommand::new("git")
        .current_dir(dir.path())
        .args(["add", "."])
        .status()
        .unwrap()
        .success());
    assert!(SyncCommand::new("git")
        .current_dir(dir.path())
        .args(["commit", "--quiet", "-m", "seed"])
        .status()
        .unwrap()
        .success());
    dir
}

/// A [`Hook`] bound to one point that always refuses to let the run proceed.
pub(super) struct BlockingHook(pub(super) HookPoint);

#[async_trait]
impl Hook for BlockingHook {
    fn points(&self) -> &[HookPoint] {
        std::slice::from_ref(&self.0)
    }

    async fn run(&self, _ctx: &HookContext<'_>) -> Result<HookOutcome> {
        Ok(HookOutcome::Block {
            reason: "test hook refuses".to_string(),
        })
    }
}

fn registry_blocking(point: HookPoint) -> HookRegistry {
    let mut registry = HookRegistry::new();
    registry.register(Arc::new(BlockingHook(point)));
    registry
}

/// A [`Hook`] bound to one point that emits a blocking finding, `warden`-sourced so it counts
/// against any step's `on_blocking` edge regardless of which role owns it (see
/// `decide_next_state_for_step`).
struct EmitFindingsHook(HookPoint);

#[async_trait]
impl Hook for EmitFindingsHook {
    fn points(&self) -> &[HookPoint] {
        std::slice::from_ref(&self.0)
    }

    async fn run(&self, _ctx: &HookContext<'_>) -> Result<HookOutcome> {
        Ok(HookOutcome::EmitFindings(vec![Finding {
            source: warden_core::FindingSource::Warden,
            severity: warden_core::Severity::Blocking,
            file: None,
            description: "hook found a problem".to_string(),
            action: None,
        }]))
    }
}

fn registry_emitting(point: HookPoint) -> HookRegistry {
    let mut registry = HookRegistry::new();
    registry.register(Arc::new(EmitFindingsHook(point)));
    registry
}

/// A [`ToolAdapter`] that must never actually be invoked -- every test workflow below has zero
/// `type: agent` steps, so `run_command_step` (not `run_agent_step`) drives the loop.
struct UnusedToolAdapter;

impl ToolAdapter for UnusedToolAdapter {
    fn build_command(&self, _definition: &AgentDefinition) -> Result<AgentCommand> {
        unreachable!("this workflow has no agent-kind step")
    }

    fn env_allowlist(&self) -> &'static [&'static str] {
        &[]
    }

    fn extract_findings(&self, _stdout: &str) -> warden_core::Result<Vec<Finding>> {
        unreachable!("this workflow has no agent-kind step")
    }
}

/// A [`ToolAdapter`] whose one agent step runs a fixed shell script -- deterministic, no LLM,
/// used only by the `on_commit` test (the one point a `type: command` step can never reach, since
/// [`super::agents::Orchestrator::run_step`]'s command path never re-reads `HEAD`).
struct ShellAgentAdapter {
    script: String,
}

impl ToolAdapter for ShellAgentAdapter {
    fn build_command(&self, _definition: &AgentDefinition) -> Result<AgentCommand> {
        Ok(AgentCommand {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), self.script.clone()],
        })
    }

    fn env_allowlist(&self) -> &'static [&'static str] {
        &["HOME"]
    }

    fn extract_findings(&self, _stdout: &str) -> warden_core::Result<Vec<Finding>> {
        Ok(Vec::new())
    }
}

pub(super) fn single_command_step_workflow() -> Workflow {
    Workflow::parse_yaml(
        r#"
name: single
entry: build
steps:
  build:
    type: command
    run: "true"
    on_clean: converged
    on_blocking: build
    on_error: failed
"#,
    )
    .unwrap()
}

fn single_agent_step_workflow() -> Workflow {
    Workflow::parse_yaml(
        r#"
name: single
entry: build
steps:
  build:
    type: agent
    agent: builder
    on_clean: converged
    on_blocking: build
    on_error: failed
"#,
    )
    .unwrap()
}

fn command_workflow_config(repo_path: &Path, warden_home: &Path, max_cycles: u32) -> RunConfig {
    RunConfig {
        repo_path: repo_path.to_path_buf(),
        warden_home: warden_home.to_path_buf(),
        branch: "main".to_string(),
        intent: "test".to_string(),
        max_cycles,
        workflow: single_command_step_workflow(),
        step_agents: Vec::new(),
        repository_agent_definitions: false,
        evidence_tool: None,
        evidence_store_in_repo: false,
        gate: None,
    }
}

async fn run_with_hooks(repo: &TempDir, warden_home: &TempDir, hooks: HookRegistry) -> RunState {
    let pool = db::connect(&warden_home.path().join("state.db"))
        .await
        .unwrap();
    let orchestrator = Orchestrator::new(pool).with_hooks(hooks);
    let config = command_workflow_config(repo.path(), warden_home.path(), 3);
    let (_run_id, final_state) = orchestrator
        .run_convergence_loop(config, UnusedToolAdapter, CancellationToken::new())
        .await
        .unwrap();
    final_state
}

#[tokio::test]
async fn on_run_start_emit_findings_is_recorded_not_merely_counted() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let pool = db::connect(&warden_home.path().join("state.db"))
        .await
        .unwrap();
    let orchestrator =
        Orchestrator::new(pool.clone()).with_hooks(registry_emitting(HookPoint::OnRunStart));
    let config = command_workflow_config(repo.path(), warden_home.path(), 3);

    let (run_id, final_state) = orchestrator
        .run_convergence_loop(config, UnusedToolAdapter, CancellationToken::new())
        .await
        .unwrap();
    // `OnRunStart` has no step to route the finding through -- it does not itself reboucle.
    assert_eq!(final_state, RunState::Converged);

    let events = db::list_events_for_run(&pool, &run_id).await.unwrap();
    let recorded = events.iter().find_map(|entry| match entry.event() {
        Some(warden_core::RunEvent::HookFindingEmitted {
            point,
            source,
            severity,
            description,
            ..
        }) => Some((
            point.clone(),
            source.clone(),
            severity.clone(),
            description.clone(),
        )),
        _ => None,
    });
    assert_eq!(
        recorded,
        Some((
            "on_run_start".to_string(),
            "warden".to_string(),
            "blocking".to_string(),
            "hook found a problem".to_string(),
        )),
        "an EmitFindings hook with no step context must still be materialized (description, \
         severity, source), not just counted"
    );
}

#[tokio::test]
async fn on_run_start_block_fails_the_run_before_any_step() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let final_state = run_with_hooks(
        &repo,
        &warden_home,
        registry_blocking(HookPoint::OnRunStart),
    )
    .await;
    assert_eq!(final_state, RunState::Failed);
}

#[tokio::test]
async fn before_step_block_fails_the_run_before_any_step_runs() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let final_state = run_with_hooks(
        &repo,
        &warden_home,
        registry_blocking(HookPoint::BeforeStep),
    )
    .await;
    assert_eq!(final_state, RunState::Failed);
}

#[tokio::test]
async fn after_step_block_fails_the_run_instead_of_converging() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let final_state =
        run_with_hooks(&repo, &warden_home, registry_blocking(HookPoint::AfterStep)).await;
    assert_eq!(final_state, RunState::Failed);
}

#[tokio::test]
async fn on_converged_block_fails_the_run_instead_of_converging() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let final_state = run_with_hooks(
        &repo,
        &warden_home,
        registry_blocking(HookPoint::OnConverged),
    )
    .await;
    assert_eq!(final_state, RunState::Failed);
}

#[tokio::test]
async fn on_commit_block_fails_the_run_after_a_step_produces_a_new_commit() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let pool = db::connect(&warden_home.path().join("state.db"))
        .await
        .unwrap();
    let orchestrator = Orchestrator::new(pool).with_hooks(registry_blocking(HookPoint::OnCommit));
    let config = RunConfig {
        workflow: single_agent_step_workflow(),
        step_agents: vec![AgentDefinition::new(None, None, None, None, "build something").unwrap()],
        ..command_workflow_config(repo.path(), warden_home.path(), 3)
    };
    let runner = ShellAgentAdapter {
        script: "echo hi > new_file.txt && git add -A && \
                 git -c user.email=t@t -c user.name=t commit -q -m auto"
            .to_string(),
    };

    let (_run_id, final_state) = orchestrator
        .run_convergence_loop(config, runner, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(final_state, RunState::Failed);
}

#[tokio::test]
async fn on_run_start_block_still_publishes_run_finished() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let pool = db::connect(&warden_home.path().join("state.db"))
        .await
        .unwrap();
    let orchestrator =
        Orchestrator::new(pool.clone()).with_hooks(registry_blocking(HookPoint::OnRunStart));
    let config = command_workflow_config(repo.path(), warden_home.path(), 3);

    let (run_id, final_state) = orchestrator
        .run_convergence_loop(config, UnusedToolAdapter, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(final_state, RunState::Failed);

    let events = db::list_events_for_run(&pool, &run_id).await.unwrap();
    let published_run_finished = events.iter().any(|entry| {
        matches!(
            entry.event(),
            Some(warden_core::RunEvent::RunFinished { final_state }) if final_state == "failed"
        )
    });
    assert!(
        published_run_finished,
        "an early hook block must still publish RunEvent::RunFinished -- otherwise a live \
         warden-tui would show the run as still running forever"
    );
}

#[tokio::test]
async fn resume_before_step_block_fails_the_run() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let pool = db::connect(&warden_home.path().join("state.db"))
        .await
        .unwrap();
    let run_id = "resumed-run-1".to_string();
    let workflow = single_command_step_workflow();
    db::insert_run(
        &pool,
        &run_id,
        &repo.path().display().to_string(),
        "main",
        "intent",
        3,
        3,
        workflow.steps.len() as u32,
        3,
    )
    .await
    .unwrap();
    // A quota-suspended run is durably parked in `ResumingQuota` until its resume lease fires --
    // exactly the state `resume_convergence_loop` is invoked against in production.
    db::update_run_state(&pool, &run_id, RunState::ResumingQuota)
        .await
        .unwrap();

    let orchestrator =
        Orchestrator::new(pool.clone()).with_hooks(registry_blocking(HookPoint::BeforeStep));
    let config = command_workflow_config(repo.path(), warden_home.path(), 3);
    let continuation = ConvergenceContinuation::new("deadbeef".to_string(), &workflow);

    let (_run_id, final_state) = orchestrator
        .resume_convergence_loop(
            run_id,
            config,
            &UnusedToolAdapter,
            CancellationToken::new(),
            continuation,
        )
        .await
        .unwrap();

    assert_eq!(
        final_state,
        RunState::Failed,
        "a before_step block on the quota-resume path must fail the run just like the fresh-run path"
    );
}

#[tokio::test]
async fn after_step_emit_findings_reboucles_via_the_step_s_on_blocking_edge() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let pool = db::connect(&warden_home.path().join("state.db"))
        .await
        .unwrap();
    let orchestrator = Orchestrator::new(pool).with_hooks(registry_emitting(HookPoint::AfterStep));
    // `max_cycles: 1` makes the reboucle unambiguous: the step itself is clean (would converge on
    // its own), so only a hook finding actually consumed by the convergence loop can push this
    // straight past its one-cycle budget on the very first attempt.
    let config = command_workflow_config(repo.path(), warden_home.path(), 1);

    let (_run_id, final_state) = orchestrator
        .run_convergence_loop(config, UnusedToolAdapter, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(
        final_state,
        RunState::StepCyclesExceeded(0),
        "a dropped EmitFindings would have converged instead (the step alone raised nothing)"
    );
}
