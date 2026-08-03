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

/// The fixture repo's `HEAD`. A quota continuation must be restored against a commit that really
/// exists: seeded with a placeholder SHA instead, the first step's worktree creation fails, the
/// step reports `StepOutcome::Error` and the run reaches `Failed` through `on_error` -- which would
/// make any "the hook block failed the run" assertion on the resume path pass for the wrong reason.
pub(super) fn head_commit(repo: &TempDir) -> String {
    let output = SyncCommand::new("git")
        .current_dir(repo.path())
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_string()
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
    // A real commit, so this run would otherwise converge -- see `head_commit`.
    let continuation = ConvergenceContinuation::new(head_commit(&repo), &workflow);

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

// ---------------------------------------------------------------------------
// Independent verification pass on issue #106.
//
// Everything below covers an acceptance criterion the suite above does not
// discriminate on: the `BeforeStep` entry path that had no test (the ordinary
// post-step loop iteration), the three step-less `EmitFindings` recording
// points other than `OnRunStart`, `OnCommit`'s routing, `OnRunEnd`'s
// best-effort contract, the teardown/`RunFinished` tail's exactly-once
// property, and the aggregation/error edges.
// ---------------------------------------------------------------------------

use std::sync::atomic::{AtomicUsize, Ordering};

/// Blocks only from its `block_from_call`-th invocation onwards -- lets a test single out *which*
/// dispatch of a repeated point (e.g. `BeforeStep`, which fires once per loop iteration) is
/// actually being enforced.
struct BlockFromNthCallHook {
    point: HookPoint,
    block_from_call: usize,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Hook for BlockFromNthCallHook {
    fn points(&self) -> &[HookPoint] {
        std::slice::from_ref(&self.point)
    }

    async fn run(&self, _ctx: &HookContext<'_>) -> Result<HookOutcome> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if call >= self.block_from_call {
            Ok(HookOutcome::Block {
                reason: format!("test hook refuses at call {call}"),
            })
        } else {
            Ok(HookOutcome::Continue)
        }
    }
}

/// A `Continue` hook that only counts its dispatches -- for the exactly-once teardown assertions.
struct CountingContinueHook {
    points: Vec<HookPoint>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Hook for CountingContinueHook {
    fn points(&self) -> &[HookPoint] {
        &self.points
    }

    async fn run(&self, _ctx: &HookContext<'_>) -> Result<HookOutcome> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(HookOutcome::Continue)
    }
}

/// Emits one fully-populated finding (`file` *and* `action` set, unlike [`EmitFindingsHook`]) so a
/// test can assert every field survives the round-trip into the `events` table.
struct DetailedEmitFindingsHook {
    point: HookPoint,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Hook for DetailedEmitFindingsHook {
    fn points(&self) -> &[HookPoint] {
        std::slice::from_ref(&self.point)
    }

    async fn run(&self, _ctx: &HookContext<'_>) -> Result<HookOutcome> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(HookOutcome::EmitFindings(vec![Finding {
            source: warden_core::FindingSource::Warden,
            severity: warden_core::Severity::Blocking,
            file: Some("src/secrets.rs".to_string()),
            description: "AWS key in diff".to_string(),
            action: Some("rotate the key".to_string()),
        }]))
    }
}

/// A hook that fails outright rather than returning any [`HookOutcome`].
struct ErroringHook(HookPoint);

#[async_trait]
impl Hook for ErroringHook {
    fn points(&self) -> &[HookPoint] {
        std::slice::from_ref(&self.0)
    }

    async fn run(&self, _ctx: &HookContext<'_>) -> Result<HookOutcome> {
        Err(WardenError::HookConfig {
            path: PathBuf::from("/tmp/hooks.toml"),
            reason: "the hook itself failed".to_string(),
        })
    }
}

/// A single `type: command` step whose command always fails -- one blocking finding per cycle, so
/// the loop reboucles onto itself through `on_blocking` and `BeforeStep` fires a second time.
fn failing_command_step_workflow() -> Workflow {
    Workflow::parse_yaml(
        r#"
name: single
entry: build
steps:
  build:
    type: command
    run: "false"
    on_clean: converged
    on_blocking: build
    on_error: failed
"#,
    )
    .unwrap()
}

async fn events_for(pool: &SqlitePool, run_id: &str) -> Vec<RunEvent> {
    db::list_events_for_run(pool, run_id)
        .await
        .unwrap()
        .iter()
        .filter_map(|entry| entry.event().cloned())
        .collect()
}

fn hook_findings_at(events: &[RunEvent], wanted_point: &str) -> Vec<RunEvent> {
    events
        .iter()
        .filter(|event| {
            matches!(event, RunEvent::HookFindingEmitted { point, .. } if point == wanted_point)
        })
        .cloned()
        .collect()
}

fn count_run_finished(events: &[RunEvent]) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, RunEvent::RunFinished { .. }))
        .count()
}

fn count_cycles_started(events: &[RunEvent]) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, RunEvent::CycleStarted { .. }))
        .count()
}

fn count_workflow_resolved(events: &[RunEvent]) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, RunEvent::WorkflowResolved { .. }))
        .count()
}

/// The fourth `BeforeStep` entry path: the ordinary post-step loop iteration. The three others
/// (fresh entry, quota resume, CI reboucle) each have a test; this one blocks only on the *second*
/// dispatch, so an implementation that enforced `BeforeStep` on entry alone would sail past it.
#[tokio::test]
async fn before_step_block_on_a_later_loop_iteration_fails_the_run() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let pool = db::connect(&warden_home.path().join("state.db"))
        .await
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = HookRegistry::new();
    registry.register(Arc::new(BlockFromNthCallHook {
        point: HookPoint::BeforeStep,
        block_from_call: 2,
        calls: calls.clone(),
    }));
    let orchestrator = Orchestrator::new(pool.clone()).with_hooks(registry);
    let config = RunConfig {
        workflow: failing_command_step_workflow(),
        ..command_workflow_config(repo.path(), warden_home.path(), 3)
    };

    let (run_id, final_state) = orchestrator
        .run_convergence_loop(config, UnusedToolAdapter, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(
        final_state,
        RunState::Failed,
        "the in-loop before_step dispatch must be a barrier too, not just the entry one"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "the block must be the second before_step dispatch (the reboucle), not the first"
    );
    let events = events_for(&pool, &run_id).await;
    assert_eq!(
        count_cycles_started(&events),
        1,
        "exactly one cycle must have run before the reboucle's before_step blocked it"
    );
    assert_eq!(
        db::get_run(&pool, &run_id).await.unwrap().unwrap().state,
        RunState::Failed
    );
}

/// The `OnCommit` half of `EmitFindings` routing: `AfterStep` has its own test, but `OnCommit` is a
/// distinct dispatch that could have been left out of the aggregation.
#[tokio::test]
async fn on_commit_emit_findings_reboucles_via_the_step_s_on_blocking_edge() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let pool = db::connect(&warden_home.path().join("state.db"))
        .await
        .unwrap();
    let orchestrator = Orchestrator::new(pool).with_hooks(registry_emitting(HookPoint::OnCommit));
    // As in the `AfterStep` twin: `max_cycles: 1` plus a clean step means only a routed hook
    // finding can push the run past its budget on the first pass.
    let config = RunConfig {
        workflow: single_agent_step_workflow(),
        step_agents: vec![AgentDefinition::new(None, None, None, None, "build something").unwrap()],
        ..command_workflow_config(repo.path(), warden_home.path(), 1)
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

    assert_eq!(
        final_state,
        RunState::StepCyclesExceeded(0),
        "a dropped on_commit EmitFindings would have converged instead"
    );
}

/// `BeforeStep` findings: recorded with every field intact, and deliberately *not* routed.
#[tokio::test]
async fn before_step_emit_findings_is_recorded_with_every_field_and_never_reboucles() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let pool = db::connect(&warden_home.path().join("state.db"))
        .await
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = HookRegistry::new();
    registry.register(Arc::new(DetailedEmitFindingsHook {
        point: HookPoint::BeforeStep,
        calls: calls.clone(),
    }));
    let orchestrator = Orchestrator::new(pool.clone()).with_hooks(registry);
    // `max_cycles: 1`: had these findings been routed into the step's decision, the clean step
    // would have blown its one-cycle budget instead of converging.
    let config = command_workflow_config(repo.path(), warden_home.path(), 1);

    let (run_id, final_state) = orchestrator
        .run_convergence_loop(config, UnusedToolAdapter, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(
        final_state,
        RunState::Converged,
        "before_step findings are recorded, not routed -- they must not force a reboucle"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let events = events_for(&pool, &run_id).await;
    assert_eq!(
        hook_findings_at(&events, "before_step"),
        vec![RunEvent::HookFindingEmitted {
            point: "before_step".to_string(),
            source: "warden".to_string(),
            severity: "blocking".to_string(),
            file: Some("src/secrets.rs".to_string()),
            description: "AWS key in diff".to_string(),
            action: Some("rotate the key".to_string()),
        }],
        "the persisted event must carry severity, description, file and source verbatim"
    );
}

/// `OnConverged` findings: recorded, and the run still converges.
#[tokio::test]
async fn on_converged_emit_findings_is_recorded_and_the_run_still_converges() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let pool = db::connect(&warden_home.path().join("state.db"))
        .await
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = HookRegistry::new();
    registry.register(Arc::new(DetailedEmitFindingsHook {
        point: HookPoint::OnConverged,
        calls: calls.clone(),
    }));
    let orchestrator = Orchestrator::new(pool.clone()).with_hooks(registry);
    let config = command_workflow_config(repo.path(), warden_home.path(), 1);

    let (run_id, final_state) = orchestrator
        .run_convergence_loop(config, UnusedToolAdapter, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(final_state, RunState::Converged);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let events = events_for(&pool, &run_id).await;
    assert_eq!(
        hook_findings_at(&events, "on_converged").len(),
        1,
        "an on_converged EmitFindings must land in the events table, not just a log line"
    );
}

/// `OnRunEnd` is best-effort teardown: a `Block` there must not rewrite the run's already-decided
/// final state -- not in the returned value, not in the database, not in `RunFinished`.
#[tokio::test]
async fn on_run_end_block_does_not_change_the_final_state() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let pool = db::connect(&warden_home.path().join("state.db"))
        .await
        .unwrap();
    let orchestrator =
        Orchestrator::new(pool.clone()).with_hooks(registry_blocking(HookPoint::OnRunEnd));
    let config = command_workflow_config(repo.path(), warden_home.path(), 3);

    let (run_id, final_state) = orchestrator
        .run_convergence_loop(config, UnusedToolAdapter, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(
        final_state,
        RunState::Converged,
        "on_run_end is teardown: the run is already over, a Block there decides nothing"
    );
    assert_eq!(
        db::get_run(&pool, &run_id).await.unwrap().unwrap().state,
        RunState::Converged
    );
    let events = events_for(&pool, &run_id).await;
    assert!(events.iter().any(|event| matches!(
        event,
        RunEvent::RunFinished { final_state } if final_state == "converged"
    )));
}

/// Fresh-entry early exit: an `OnRunStart` block must still fire `OnRunEnd` teardown, exactly once,
/// on its way out -- and publish exactly one `RunFinished`.
#[tokio::test]
async fn an_on_run_start_block_still_fires_on_run_end_teardown_once() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let pool = db::connect(&warden_home.path().join("state.db"))
        .await
        .unwrap();
    let teardowns = Arc::new(AtomicUsize::new(0));
    let mut registry = HookRegistry::new();
    registry.register(Arc::new(BlockingHook(HookPoint::OnRunStart)));
    registry.register(Arc::new(CountingContinueHook {
        points: vec![HookPoint::OnRunEnd],
        calls: teardowns.clone(),
    }));
    let orchestrator = Orchestrator::new(pool.clone()).with_hooks(registry);
    let config = command_workflow_config(repo.path(), warden_home.path(), 3);

    let (run_id, final_state) = orchestrator
        .run_convergence_loop(config, UnusedToolAdapter, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(final_state, RunState::Failed);
    assert_eq!(
        teardowns.load(Ordering::SeqCst),
        1,
        "an early block must still run teardown, and exactly once"
    );
    let events = events_for(&pool, &run_id).await;
    assert_eq!(count_run_finished(&events), 1);
}

/// Quota-resume early exit: same guarantees on the `resume_convergence_loop` entry, whose block
/// happens before the loop is ever entered.
#[tokio::test]
async fn a_resume_path_block_still_publishes_run_finished_and_fires_teardown_once() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let pool = db::connect(&warden_home.path().join("state.db"))
        .await
        .unwrap();
    let run_id = "resumed-run-2".to_string();
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
    db::update_run_state(&pool, &run_id, RunState::ResumingQuota)
        .await
        .unwrap();

    let teardowns = Arc::new(AtomicUsize::new(0));
    let mut registry = HookRegistry::new();
    registry.register(Arc::new(BlockingHook(HookPoint::BeforeStep)));
    registry.register(Arc::new(CountingContinueHook {
        points: vec![HookPoint::OnRunEnd],
        calls: teardowns.clone(),
    }));
    let orchestrator = Orchestrator::new(pool.clone()).with_hooks(registry);
    let config = command_workflow_config(repo.path(), warden_home.path(), 3);
    let continuation = ConvergenceContinuation::new(head_commit(&repo), &workflow);

    let (run_id, final_state) = orchestrator
        .resume_convergence_loop(
            run_id,
            config,
            &UnusedToolAdapter,
            CancellationToken::new(),
            continuation,
        )
        .await
        .unwrap();

    assert_eq!(final_state, RunState::Failed);
    assert_eq!(teardowns.load(Ordering::SeqCst), 1);
    let events = events_for(&pool, &run_id).await;
    assert_eq!(
        count_run_finished(&events),
        1,
        "an attached warden-tui keys is_finished() off RunFinished; the resume early-exit must \
         publish it exactly once"
    );
}

/// The no-hook baseline: the `'run:` labeled-block rewrite must not have changed the ordinary happy
/// path at all.
#[tokio::test]
async fn a_run_with_no_hooks_at_all_still_converges_with_one_run_finished() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let pool = db::connect(&warden_home.path().join("state.db"))
        .await
        .unwrap();
    let orchestrator = Orchestrator::new(pool.clone());
    let config = command_workflow_config(repo.path(), warden_home.path(), 3);

    let (run_id, final_state) = orchestrator
        .run_convergence_loop(config, UnusedToolAdapter, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(final_state, RunState::Converged);
    assert_eq!(
        db::get_run(&pool, &run_id).await.unwrap().unwrap().state,
        RunState::Converged
    );
    let events = events_for(&pool, &run_id).await;
    assert_eq!(count_run_finished(&events), 1, "no double RunFinished");
    assert_eq!(count_cycles_started(&events), 1);
    assert!(
        hook_findings_at(&events, "before_step").is_empty(),
        "no hooks means no hook-finding events at all"
    );
}

/// The all-`Continue` baseline: every point registered, nothing refused -- the run must converge
/// exactly as with no hooks, with a single teardown and a single `RunFinished`.
#[tokio::test]
async fn a_run_whose_hooks_all_continue_converges_with_one_teardown_and_one_run_finished() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let pool = db::connect(&warden_home.path().join("state.db"))
        .await
        .unwrap();
    let per_point: Vec<(HookPoint, Arc<AtomicUsize>)> = HookPoint::ALL
        .iter()
        .map(|point| (*point, Arc::new(AtomicUsize::new(0))))
        .collect();
    let mut registry = HookRegistry::new();
    for (point, calls) in &per_point {
        registry.register(Arc::new(CountingContinueHook {
            points: vec![*point],
            calls: calls.clone(),
        }));
    }
    let orchestrator = Orchestrator::new(pool.clone()).with_hooks(registry);
    let config = command_workflow_config(repo.path(), warden_home.path(), 3);

    let (run_id, final_state) = orchestrator
        .run_convergence_loop(config, UnusedToolAdapter, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(final_state, RunState::Converged);
    let count = |wanted: HookPoint| {
        per_point
            .iter()
            .find(|(point, _)| *point == wanted)
            .map(|(_, calls)| calls.load(Ordering::SeqCst))
            .unwrap()
    };
    assert_eq!(count(HookPoint::OnRunStart), 1);
    assert_eq!(count(HookPoint::BeforeStep), 1);
    assert_eq!(count(HookPoint::AfterStep), 1);
    assert_eq!(count(HookPoint::OnConverged), 1);
    assert_eq!(
        count(HookPoint::OnRunEnd),
        1,
        "teardown fires once and only once"
    );
    assert_eq!(
        count(HookPoint::OnCommit),
        0,
        "a `type: command` step never moves HEAD"
    );
    assert_eq!(
        count(HookPoint::BeforePush),
        0,
        "no gate is configured, so nothing is ever pushed"
    );
    let events = events_for(&pool, &run_id).await;
    assert_eq!(count_run_finished(&events), 1);
}

/// Aggregation contract, `Block` first: the first `Block` short-circuits, so a later hook on the
/// same point never even runs.
#[tokio::test]
async fn a_block_short_circuits_a_later_emit_findings_hook_on_the_same_point() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let pool = db::connect(&warden_home.path().join("state.db"))
        .await
        .unwrap();
    let emitted = Arc::new(AtomicUsize::new(0));
    let mut registry = HookRegistry::new();
    registry.register(Arc::new(BlockingHook(HookPoint::AfterStep)));
    registry.register(Arc::new(DetailedEmitFindingsHook {
        point: HookPoint::AfterStep,
        calls: emitted.clone(),
    }));
    let orchestrator = Orchestrator::new(pool).with_hooks(registry);
    let config = command_workflow_config(repo.path(), warden_home.path(), 3);

    let (_run_id, final_state) = orchestrator
        .run_convergence_loop(config, UnusedToolAdapter, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(final_state, RunState::Failed);
    assert_eq!(
        emitted.load(Ordering::SeqCst),
        0,
        "the hook registered after the blocker must never run"
    );
}

/// Aggregation contract, `EmitFindings` first: a later `Block` still wins over the reboucle the
/// earlier findings would have caused.
#[tokio::test]
async fn a_block_after_an_emit_findings_hook_still_fails_the_run() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let pool = db::connect(&warden_home.path().join("state.db"))
        .await
        .unwrap();
    let emitted = Arc::new(AtomicUsize::new(0));
    let mut registry = HookRegistry::new();
    registry.register(Arc::new(DetailedEmitFindingsHook {
        point: HookPoint::AfterStep,
        calls: emitted.clone(),
    }));
    registry.register(Arc::new(BlockingHook(HookPoint::AfterStep)));
    let orchestrator = Orchestrator::new(pool).with_hooks(registry);
    let config = command_workflow_config(repo.path(), warden_home.path(), 3);

    let (_run_id, final_state) = orchestrator
        .run_convergence_loop(config, UnusedToolAdapter, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(
        final_state,
        RunState::Failed,
        "a Block anywhere on the point beats the findings emitted before it"
    );
    assert_eq!(emitted.load(Ordering::SeqCst), 1);
}

/// A hook that *fails* is not a hook that blocks: the error propagates as an `Err` out of the run
/// rather than being folded into a final state.
#[tokio::test]
async fn a_hook_that_errors_aborts_the_run_with_an_error() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let pool = db::connect(&warden_home.path().join("state.db"))
        .await
        .unwrap();
    let teardowns = Arc::new(AtomicUsize::new(0));
    let mut registry = HookRegistry::new();
    registry.register(Arc::new(ErroringHook(HookPoint::BeforeStep)));
    registry.register(Arc::new(CountingContinueHook {
        points: vec![HookPoint::OnRunEnd],
        calls: teardowns.clone(),
    }));
    let orchestrator = Orchestrator::new(pool.clone()).with_hooks(registry);
    let config = command_workflow_config(repo.path(), warden_home.path(), 3);

    let error = orchestrator
        .run_convergence_loop(config, UnusedToolAdapter, CancellationToken::new())
        .await
        .expect_err("a failing hook must surface as an error, not a silent Continue");

    assert!(
        matches!(error, WardenError::HookConfig { .. }),
        "the hook's own error must reach the caller verbatim: {error}"
    );

    // Documented gap, not a #106 regression: `?` inside `run_convergence_continuation`'s `'run:`
    // block returns straight out of the function, so the single teardown/`RunFinished` tail below
    // it is skipped. A `Block` unwinds through that tail (see the tests above); an `Err` does not,
    // which leaves the run's persisted state untouched and an attached `warden-tui` showing it as
    // still running. Asserted so a future fix has to come here and flip these deliberately.
    let events = events_for(&pool, &run_id_of_only_run(&pool).await).await;
    assert_eq!(
        count_run_finished(&events),
        0,
        "current behavior: a hook error skips the RunFinished tail"
    );
    assert_eq!(
        teardowns.load(Ordering::SeqCst),
        0,
        "current behavior: a hook error skips on_run_end teardown"
    );
    assert_eq!(
        db::get_run(&pool, &run_id_of_only_run(&pool).await)
            .await
            .unwrap()
            .unwrap()
            .state,
        RunState::RunningStep(0),
        "current behavior: the run is left in the write-ahead state, to be swept up later by \
         crash recovery rather than resolved here"
    );
}

/// The one run in a freshly-created pool (these tests never create a second).
async fn run_id_of_only_run(pool: &SqlitePool) -> String {
    let rows: Vec<(String,)> = sqlx::query_as("SELECT id FROM runs")
        .fetch_all(pool)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    rows[0].0.clone()
}

/// Issue #107's headline guarantee: a fresh run publishes `WorkflowResolved` exactly once, right
/// after `RunStarted` and before the first step transition (`CycleStarted`). Pins the exact
/// ordering so moving the `publish_event` call in `driver.rs` -- e.g. into the `restored` branch,
/// or below `transition_or_block` -- fails loudly here instead of shipping silently.
#[tokio::test]
async fn workflow_resolved_is_published_exactly_once_right_after_run_started() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let pool = db::connect(&warden_home.path().join("state.db"))
        .await
        .unwrap();
    let orchestrator = Orchestrator::new(pool.clone());
    let config = command_workflow_config(repo.path(), warden_home.path(), 3);

    let (run_id, final_state) = orchestrator
        .run_convergence_loop(config, UnusedToolAdapter, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(final_state, RunState::Converged);

    let events = events_for(&pool, &run_id).await;
    assert_eq!(
        count_workflow_resolved(&events),
        1,
        "WorkflowResolved must be published exactly once per run: {events:?}"
    );
    assert!(
        matches!(events[0], RunEvent::RunStarted { .. }),
        "expected RunStarted first: {events:?}"
    );
    assert!(
        matches!(events[1], RunEvent::WorkflowResolved { .. }),
        "WorkflowResolved must immediately follow RunStarted, before the first step transition: \
         {events:?}"
    );

    let RunEvent::WorkflowResolved { name, entry, steps } = &events[1] else {
        unreachable!("just matched above");
    };
    assert_eq!(name, "single");
    assert_eq!(*entry, 0);
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].id, "build");
}

/// The resume path (crash/quota recovery) must not re-publish `WorkflowResolved` -- it was already
/// published once, on the run's original start.
#[tokio::test]
async fn resuming_a_run_does_not_republish_workflow_resolved() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let pool = db::connect(&warden_home.path().join("state.db"))
        .await
        .unwrap();
    let run_id = "resumed-run-workflow-resolved".to_string();
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
    db::update_run_state(&pool, &run_id, RunState::ResumingQuota)
        .await
        .unwrap();
    // The original start's own `WorkflowResolved`, already persisted before the crash/suspension
    // being resumed here.
    db::insert_event(
        &pool,
        &Uuid::new_v4().to_string(),
        &run_id,
        &RunEvent::WorkflowResolved {
            name: workflow.name.clone(),
            entry: workflow.entry(),
            steps: Vec::new(),
        },
        &chrono::Utc::now().to_rfc3339(),
    )
    .await
    .unwrap();

    let orchestrator = Orchestrator::new(pool.clone());
    let config = command_workflow_config(repo.path(), warden_home.path(), 3);
    let continuation = ConvergenceContinuation::new(head_commit(&repo), &workflow);

    let (run_id, final_state) = orchestrator
        .resume_convergence_loop(
            run_id,
            config,
            &UnusedToolAdapter,
            CancellationToken::new(),
            continuation,
        )
        .await
        .unwrap();

    assert_eq!(final_state, RunState::Converged);
    let events = events_for(&pool, &run_id).await;
    assert_eq!(
        count_workflow_resolved(&events),
        1,
        "resuming a run must not publish a second WorkflowResolved: {events:?}"
    );
}
