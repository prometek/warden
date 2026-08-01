//! The convergence loop: producer -> [gated step]* -> reboucle if findings.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use sqlx::SqlitePool;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use warden_core::{
    decide_next_state_after_ci, decide_next_state_for_step, AgentDefinition, AgentRole, CiOutcome,
    CiResultMessage, Finding, HookContext, HookOutcome, HookPoint, Role, RunEvent, RunState,
    Workflow, WorkflowStep, DIFF_TRUNCATED_MARKER,
};
use warden_sandbox::{LocalSandbox, Sandbox};

use crate::agent_def;
use crate::ci_channel::CiResultListener;
use crate::db;
use crate::error::{ProcessError, Result, WardenError, WorktreeError};
use crate::event_bus::EventBus;
use crate::evidence::{self, EvidenceCaptureContext};
use crate::gate_trigger::{GateChild, GateTrigger, RunTailTrigger};
use crate::git_util::NO_HOST_HOOKS;
use crate::hook::HookRegistry;
use crate::policy_gate::{PolicyGate, PolicyOutcome};
use crate::process::{self, AgentCommand, AgentOutcome};
use crate::tool_adapter::ToolAdapter;
use crate::worktree::{self, WorktreeManager};

mod agent_run;
mod agents;
mod config;
mod continuation;
mod convergence;
mod diff;
mod evidence_capture;
mod gate_tail;
mod recovery;
mod tampering;
#[cfg(test)]
pub(crate) mod test_support;

pub use config::{
    ApprovalConfig, GateConfig, RunConfig, RunExecutionContext, SandboxConfig,
    UntrustedRepoAgentDefinition,
};
pub use recovery::{recover_crashed_runs, resume_awaiting_ci_runs, resume_quota_suspended_runs};

use tampering::AgentDefinitionSnapshot;

/// One step's markdown definition, already mapped onto the command to spawn for it: what to run,
/// and what to tell it it is.
struct ResolvedAgent {
    command: AgentCommand,
    /// `AgentDefinition::system_prompt`, cloned once per run.
    system_prompt: String,
    trusted_arg_values: Vec<String>,
}

fn trusted_arg_values_for_step(
    step: &WorkflowStep,
    definition: &AgentDefinition,
    untrusted_repo_agent_definitions: &[UntrustedRepoAgentDefinition],
) -> Vec<String> {
    let role = match step.role.as_str() {
        "reviewer" => AgentRole::Reviewer,
        "tester" => AgentRole::Tester,
        _ => return Vec::new(),
    };
    let sourced_from_the_repo_under_review = untrusted_repo_agent_definitions
        .iter()
        .any(|entry| entry.role == role);
    if sourced_from_the_repo_under_review {
        return Vec::new();
    }
    definition.model.iter().cloned().collect()
}

struct ResolvedAgents {
    steps: Vec<Option<ResolvedAgent>>,
    /// This run's `--tool` adapter's own env allowlist, resolved once here since it's a property of
    /// the tool, not of any one role -- `--tool` is global for a run, so every step shares it.
    env_allowlist: &'static [&'static str],
}

impl ResolvedAgents {
    /// Maps every `type: agent` step's definition up-front, before the loop spawns anything.
    fn resolve<R: ToolAdapter>(runner: &R, config: &RunConfig) -> Result<Self> {
        let expected_agent_steps = config
            .workflow
            .steps
            .iter()
            .filter(|step| step.kind == warden_core::StepKind::Agent)
            .count();
        if config.step_agents.len() != expected_agent_steps {
            return Err(WardenError::MismatchedStepAgentCount {
                agent_steps: expected_agent_steps,
                step_agents: config.step_agents.len(),
            });
        }
        let resolve_one =
            |step: &WorkflowStep, definition: &AgentDefinition| -> Result<ResolvedAgent> {
                Ok(ResolvedAgent {
                    command: runner.build_command(definition)?,
                    system_prompt: definition.system_prompt.clone(),
                    trusted_arg_values: trusted_arg_values_for_step(
                        step,
                        definition,
                        &config.untrusted_repo_agent_definitions,
                    ),
                })
            };
        let mut definitions = config.step_agents.iter();
        let mut steps = Vec::with_capacity(config.workflow.steps.len());
        for step in &config.workflow.steps {
            match step.kind {
                warden_core::StepKind::Agent => {
                    let definition = definitions
                        .next()
                        .expect("length checked against expected_agent_steps above");
                    steps.push(Some(resolve_one(step, definition)?));
                }
                warden_core::StepKind::Hook => steps.push(None),
            }
        }
        Ok(Self {
            steps,
            env_allowlist: runner.env_allowlist(),
        })
    }
}

/// Parameters for a single producer invocation (`workflow.steps[0]` -- the coder in the built-in
/// default workflow).
struct ProducerInvocation<'a> {
    run_id: &'a str,
    cycle_id: &'a str,
    cycle_number: u32,
    config: &'a RunConfig,
    /// The producer step's own open role (`workflow.steps[0].role`).
    role: &'a Role,
    /// This run's producer command + system prompt.
    agent: &'a ResolvedAgent,
    /// This run's `--tool` adapter's env allowlist -- `ResolvedAgents::env_allowlist`.
    env_allowlist: &'static [&'static str],
    worktree_manager: &'a WorktreeManager,
    base_commit: &'a str,
    run_agent_definition_snapshot: &'a AgentDefinitionSnapshot,
    /// Findings the producer must fix during this cycle.
    prior_findings: &'a [Finding],
    cancel: CancellationToken,
}

struct EvidenceCapture<'a> {
    run_id: &'a str,
    cycle_id: &'a str,
    cycle_number: u32,
    config: &'a RunConfig,
    /// The command the tester step was invoked with, mapped from its definition by this run's
    /// `ToolAdapter` — what `asciinema rec` records as the session.
    tester_command: &'a AgentCommand,
    tester_worktree_path: &'a Path,
    cancel: CancellationToken,
}

struct GatedStepInvocation<'a> {
    run_id: &'a str,
    cycle_id: &'a str,
    cycle_number: u32,
    /// This step's 0-based index in `config.workflow.steps` -- never `0` (the producer's own
    /// index).
    step_index: u32,
    role: &'a Role,
    /// which mechanism this step runs through -- [`Orchestrator::run_gated_step`]'s own dispatch.
    kind: warden_core::StepKind,
    /// This step's command + system prompt.
    agent: Option<&'a ResolvedAgent>,
    /// the shell command a `type: hook` step runs.
    run: Option<&'a str>,
    /// This run's `--tool` adapter's env allowlist -- `ResolvedAgents::env_allowlist`.
    env_allowlist: &'static [&'static str],
    worktree_manager: &'a WorktreeManager,
    commit: &'a str,
    /// The diff this cycle's producer introduced against the cycle's starting commit -- fed to the
    /// agent as `AgentInputMessage::diff`, unless `scope` narrows it to a correctif.
    diff: &'a str,
    /// Findings that triggered this cycle -- fed to the agent as `AgentInputMessage::findings`.
    prior_findings: &'a [Finding],
    /// `ReviewScope::Full` for every step except one following its own first full pass over a run's
    /// body of work; `ReviewScope::Correctif` then -- see [`warden_core::ReviewScope`].
    scope: warden_core::ReviewScope,
    /// This step's own declared [`warden_core::WorkflowStep::captures_evidence`] -- whether a clean
    /// run of *this* step triggers evidence capture.
    captures_evidence: bool,
    /// Consulted only when `captures_evidence` is set (evidence capture's own config:
    /// `evidence_tool`/`evidence_store_in_repo`/`warden_home`).
    config: &'a RunConfig,
    cancel: CancellationToken,
}

/// Outcome of a single producer invocation within a cycle: the commit it produced, and the diff
/// introduced against the cycle's starting commit.
#[derive(Debug, Clone)]
struct ProducerCycleResult {
    commit: String,
    diff: String,
    definition_tampering_finding: Option<Finding>,
}

/// All mutable convergence-loop state that must survive a quota suspension.
#[derive(Debug, Clone)]
struct ConvergenceContinuation {
    /// Fixed commit against which agent-definition tampering is checked for the lifetime of the
    /// run.
    run_base_commit_sha: String,
    /// Commit the next producer cycle starts from, or the commit every remaining gated step in the
    /// active cycle inspects.
    base_commit: String,
    cycle_number: u32,
    review_cycle_number: u32,
    test_cycle_number: u32,
    extra_step_cycle_number: u32,
    pending_ci_findings: Vec<Finding>,
    previous_cycle_id: Option<String>,
    step_last_reviewed_commit: Vec<Option<String>>,
    own_step_cycle_numbers: Vec<u32>,
    active_cycle: Option<ActiveCycleContinuation>,
}

impl ConvergenceContinuation {
    fn new(run_base_commit_sha: String, total_steps: usize) -> Self {
        Self {
            base_commit: run_base_commit_sha.clone(),
            run_base_commit_sha,
            cycle_number: 1,
            review_cycle_number: 0,
            test_cycle_number: 0,
            extra_step_cycle_number: 0,
            pending_ci_findings: Vec::new(),
            previous_cycle_id: None,
            step_last_reviewed_commit: vec![None; total_steps],
            own_step_cycle_numbers: vec![0; total_steps],
            active_cycle: None,
        }
    }

    fn next_run_state(&self) -> RunState {
        match self.active_cycle.as_ref().map(|cycle| &cycle.phase) {
            Some(ActiveCyclePhase::Gated {
                next_step_index, ..
            }) => RunState::RunningStep(*next_step_index),
            Some(ActiveCyclePhase::Producer) | None => RunState::CoderRunning,
        }
    }
}

/// The cycle row already exists when a process suspends during an invocation or between two steps.
#[derive(Debug, Clone)]
struct ActiveCycleContinuation {
    cycle_id: String,
    prior_findings: Vec<Finding>,
    producer_base_commit: String,
    phase: ActiveCyclePhase,
}

#[derive(Debug, Clone)]
enum ActiveCyclePhase {
    /// The producer invocation did not complete successfully and must be retried from the same
    /// cycle boundary.
    Producer,
    /// The producer completed and `next_step_index` is the first gated step that has not completed
    /// in this cycle.
    Gated {
        producer_result: ProducerCycleResult,
        findings: Vec<Finding>,
        next_step_index: u32,
        entered_extra_budget_this_cycle: bool,
    },
}

/// The run this [`Orchestrator`] instance is currently driving, and the [`EventBus`] its events are
/// published on.
struct RunContext {
    run_id: String,
    event_bus: EventBus,
}

#[derive(Debug)]
enum PostConvergenceOutcome {
    Terminal(RunState),
    Reboucle { findings: Vec<Finding> },
}

/// Drives the convergence loop against a persisted [`SqlitePool`].
pub struct Orchestrator {
    pool: SqlitePool,
    /// `None` until [`Orchestrator::run_convergence_loop`] starts a run.
    run_context: tokio::sync::OnceCell<RunContext>,
    /// invoked synchronously with the freshly generated run id, at the exact same point
    /// `RunEvent::RunStarted` is published.
    on_run_started: Option<RunStartedCallback>,
    sandbox: Arc<dyn Sandbox>,
    /// the lifecycle-hook registry dispatched at every relevant state transition (see
    /// [`Orchestrator::transition`]).
    hooks: HookRegistry,
    policy_gate: Arc<PolicyGate>,
    /// Exact startup choices required for durable quota resumption.
    run_execution_context: Option<RunExecutionContext>,
    quota_anticipation_threshold: f64,
}

type RunStartedCallback = Box<dyn Fn(&str) + Send + Sync>;

impl Orchestrator {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            run_context: tokio::sync::OnceCell::new(),
            on_run_started: None,
            sandbox: Arc::new(LocalSandbox::new()),
            hooks: HookRegistry::new(),
            policy_gate: Arc::new(PolicyGate::empty()),
            run_execution_context: None,
            quota_anticipation_threshold: 0.90,
        }
    }

    pub fn on_run_started(mut self, callback: impl Fn(&str) + Send + Sync + 'static) -> Self {
        self.on_run_started = Some(Box::new(callback));
        self
    }

    /// Selects a [`Sandbox`] backend other than the [`LocalSandbox`] default.
    pub fn with_sandbox(mut self, sandbox: Arc<dyn Sandbox>) -> Self {
        self.sandbox = sandbox;
        self
    }

    /// Installs the lifecycle-hook [`HookRegistry`] dispatched at each relevant transition.
    pub fn with_hooks(mut self, hooks: HookRegistry) -> Self {
        self.hooks = hooks;
        self
    }

    /// Installs the [`PolicyGate`] consulted before this run's `git_push` action.
    pub fn with_policy_gate(mut self, policy_gate: Arc<PolicyGate>) -> Self {
        self.policy_gate = policy_gate;
        self
    }

    /// Installs the durable counterpart of this orchestrator's adapter, sandbox, hooks, policy, and
    /// approval-channel choices.
    pub fn with_run_execution_context(mut self, context: RunExecutionContext) -> Self {
        self.run_execution_context = Some(context);
        self
    }

    /// Sets the fraction of consumed CLI quota at which the next workflow step is withheld.
    pub fn with_quota_anticipation_threshold(mut self, threshold: f64) -> Self {
        debug_assert!((0.0..=1.0).contains(&threshold));
        self.quota_anticipation_threshold = threshold;
        self
    }

    async fn suspend_for_quota_if(
        &self,
        run_id: &str,
        predicate: impl FnOnce(&warden_core::RateLimitStatus) -> bool,
    ) -> Result<bool> {
        let Some(status) = db::get_run_rate_limit_status(&self.pool, run_id).await? else {
            return Ok(false);
        };
        if !predicate(&status) {
            return Ok(false);
        }
        Ok(true)
    }

    async fn suspend_for_anticipated_quota(&self, run_id: &str) -> Result<bool> {
        self.suspend_for_quota_if(run_id, |status| {
            !status.is_using_overage && status.utilization >= self.quota_anticipation_threshold
        })
        .await
    }

    async fn suspend_for_exhausted_quota(&self, run_id: &str) -> Result<bool> {
        self.suspend_for_quota_if(run_id, |status| {
            !status.is_using_overage
                && (status.utilization >= 1.0
                    || matches!(status.status, warden_core::RateLimitState::Other(ref value)
                        if matches!(value.as_str(), "blocked" | "exhausted" | "rate_limited")))
        })
        .await
    }

    async fn publish_event(&self, event: RunEvent) -> Result<()> {
        let Some(context) = self.run_context.get() else {
            return Ok(());
        };

        let id = Uuid::new_v4().to_string();
        let created_at = Utc::now().to_rfc3339();
        db::insert_event(&self.pool, &id, &context.run_id, &event, &created_at).await?;
        context.event_bus.publish(&warden_core::RunEventRecord {
            id,
            run_id: context.run_id.clone(),
            event,
            created_at,
        });
        Ok(())
    }

    fn publish_progress_event(&self, role_name: &str, detail: String) {
        let Some(context) = self.run_context.get() else {
            return;
        };

        context.event_bus.publish(&warden_core::RunEventRecord {
            id: Uuid::new_v4().to_string(),
            run_id: context.run_id.clone(),
            event: RunEvent::AgentProgress {
                role: role_name.to_string(),
                detail,
            },
            created_at: Utc::now().to_rfc3339(),
        });
    }

    async fn transition(&self, run_id: &str, to: RunState) -> Result<()> {
        let run =
            db::get_run(&self.pool, run_id)
                .await?
                .ok_or_else(|| WardenError::RunNotFound {
                    run_id: run_id.to_string(),
                })?;
        run.state.validate_transition(to, run.total_steps)?;
        db::update_run_state(&self.pool, run_id, to).await?;

        if !self.hooks.is_empty() {
            if let Some(point) = HookPoint::on_entering(to) {
                let ctx = HookContext {
                    point,
                    run_id,
                    state: to,
                    repo_path: Path::new(&run.repo_path),
                    cycle: None,
                    worktree: None,
                    commit: None,
                    diff: None,
                };
                match self.hooks.run_hooks(point, &ctx).await? {
                    HookOutcome::Continue => {}
                    other => tracing::warn!(
                        run_id,
                        point = point.as_str(),
                        ?other,
                        "lifecycle hook returned a non-Continue outcome; consuming it \
                             (Block / EmitFindings) is not wired yet (issue #51)"
                    ),
                }
            }
        }
        Ok(())
    }

    /// Dispatches the **run-level** lifecycle hooks ([`HookPoint::OnRunStart`] /
    /// [`HookPoint::OnRunEnd`]) and returns their aggregated outcome.
    pub(super) async fn dispatch_run_hooks(
        &self,
        run_id: &str,
        repo_path: &Path,
        state: RunState,
        point: HookPoint,
    ) -> Result<HookOutcome> {
        if self.hooks.is_empty() {
            return Ok(HookOutcome::Continue);
        }
        let ctx = HookContext {
            point,
            run_id,
            state,
            repo_path,
            cycle: None,
            worktree: None,
            commit: None,
            diff: None,
        };
        self.hooks.run_hooks(point, &ctx).await
    }

    /// Fires [`HookPoint::OnRunEnd`] teardown, best-effort.
    pub(super) async fn run_teardown(&self, run_id: &str, repo_path: &Path, final_state: RunState) {
        match self
            .dispatch_run_hooks(run_id, repo_path, final_state, HookPoint::OnRunEnd)
            .await
        {
            Ok(HookOutcome::Continue) => {}
            Ok(other) => tracing::warn!(
                run_id,
                ?other,
                "on_run_end teardown hook returned a non-Continue outcome; ignoring \
                 (the run is already over, teardown is best-effort)"
            ),
            Err(err) => tracing::warn!(
                run_id,
                error = %err,
                "on_run_end teardown hook failed to run; ignoring (teardown must not \
                 mask the run's final state)"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::test_support::*;
    use warden_core::Gate;

    fn config_with(
        workflow: warden_core::Workflow,
        step_agents: Vec<AgentDefinition>,
    ) -> RunConfig {
        RunConfig {
            repo_path: PathBuf::from("/nonexistent/repo"),
            warden_home: PathBuf::from("/nonexistent/warden-home"),
            branch: "main".to_string(),
            intent: "issue #79 review: ResolvedAgents::resolve coverage".to_string(),
            max_review_cycles: 3,
            max_test_cycles: 3,
            workflow,
            max_extra_step_cycles: 5,
            step_agents,
            evidence_tool: None,
            evidence_store_in_repo: false,
            gate: None,
            untrusted_repo_agent_definitions: Vec::new(),
        }
    }

    fn workflow_with_two_hook_steps() -> warden_core::Workflow {
        warden_core::Workflow::parse_yaml(
            r#"
name: x
steps:
  - role: coder
    agent: coder
  - role: one
    type: hook
    run: "true"
    gate: loop-until-clean
  - role: two
    type: hook
    run: "true"
    gate: loop-until-clean
"#,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn mismatched_step_agent_count_counts_only_agent_kind_steps() {
        let workflow = workflow_with_two_hook_steps();

        let too_many = config_with(
            workflow.clone(),
            vec![
                definition(always_passing_tester()),
                definition(always_passing_tester()),
            ],
        );
        let error = match ResolvedAgents::resolve(&FakeCommandAdapter, &too_many) {
            Err(error) => error,
            Ok(_) => panic!("expected a mismatched-count error"),
        };
        assert!(
            matches!(
                error,
                WardenError::MismatchedStepAgentCount {
                    agent_steps: 1,
                    step_agents: 2,
                }
            ),
            "expected agent_steps: 1 (only the producer is type: agent), step_agents: 2: {error:?}"
        );

        let too_few = config_with(workflow, Vec::new());
        let error = match ResolvedAgents::resolve(&FakeCommandAdapter, &too_few) {
            Err(error) => error,
            Ok(_) => panic!("expected a mismatched-count error"),
        };
        assert!(
            matches!(
                error,
                WardenError::MismatchedStepAgentCount {
                    agent_steps: 1,
                    step_agents: 0,
                }
            ),
            "expected agent_steps: 1, step_agents: 0: {error:?}"
        );
    }

    #[tokio::test]
    async fn resolved_agents_are_some_only_at_a_type_agent_steps_own_index() {
        let workflow = workflow_with_two_hook_steps();
        let config = config_with(workflow, vec![definition(always_passing_tester())]);

        let resolved = ResolvedAgents::resolve(&FakeCommandAdapter, &config).unwrap();

        assert_eq!(resolved.steps.len(), 3);
        assert!(
            resolved.steps[0].is_some(),
            "steps[0] (\"coder\", type: agent) must resolve to Some"
        );
        assert!(
            resolved.steps[1].is_none(),
            "steps[1] (\"one\", type: hook) must resolve to None"
        );
        assert!(
            resolved.steps[2].is_none(),
            "steps[2] (\"two\", type: hook) must resolve to None"
        );
    }

    fn step(role_name: &str) -> WorkflowStep {
        WorkflowStep {
            role: Role::new(role_name).unwrap(),
            kind: warden_core::StepKind::Agent,
            agent: Some(role_name.to_string()),
            run: None,
            gate: Gate::PassThrough,
            budget: None,
            captures_evidence: false,
        }
    }

    fn definition_with_model(model: &str) -> AgentDefinition {
        AgentDefinition::new(None, None, None, Some(model.to_string()), "be an agent").unwrap()
    }

    #[test]
    fn a_reviewer_or_tester_step_sourced_from_trusted_config_vouches_for_its_model() {
        for role_name in ["reviewer", "tester"] {
            let definition = definition_with_model("anthropic/claude-3-opus");
            let trusted = trusted_arg_values_for_step(&step(role_name), &definition, &[]);
            assert_eq!(trusted, vec!["anthropic/claude-3-opus".to_string()]);
        }
    }

    #[test]
    fn a_reviewer_step_sourced_from_the_repo_under_review_never_vouches_for_anything() {
        let definition = definition_with_model("anthropic/claude-3-opus");
        let untrusted = vec![UntrustedRepoAgentDefinition {
            role: AgentRole::Reviewer,
            path: PathBuf::from(".warden/agents/reviewer.md"),
            canonical_path: PathBuf::from("/repo/.warden/agents/reviewer.md"),
        }];

        let trusted = trusted_arg_values_for_step(&step("reviewer"), &definition, &untrusted);
        assert!(trusted.is_empty());

        let tester_trusted = trusted_arg_values_for_step(&step("tester"), &definition, &untrusted);
        assert_eq!(tester_trusted, vec!["anthropic/claude-3-opus".to_string()]);
    }

    #[test]
    fn a_custom_step_never_vouches_for_anything() {
        let definition = definition_with_model("anthropic/claude-3-opus");
        let trusted = trusted_arg_values_for_step(&step("techlead"), &definition, &[]);
        assert!(trusted.is_empty());
    }

    #[test]
    fn the_producer_step_never_vouches_for_anything() {
        let definition = definition_with_model("anthropic/claude-3-opus");
        let trusted = trusted_arg_values_for_step(&step("coder"), &definition, &[]);
        assert!(trusted.is_empty());
    }

    #[test]
    fn a_trusted_step_with_no_model_set_vouches_for_nothing() {
        let definition = AgentDefinition::new(None, None, None, None, "be an agent").unwrap();
        let trusted = trusted_arg_values_for_step(&step("reviewer"), &definition, &[]);
        assert!(trusted.is_empty());
    }
}
