//! The convergence loop: producer -> [gated step]* -> reboucle if findings.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use sqlx::SqlitePool;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use warden_core::{
    decide_next_state_after_ci, decide_next_state_for_step, AgentDefinition, CiOutcome,
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
use crate::progress_writer::ProgressWriter;
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
#[cfg(test)]
mod lifecycle_hook_tests;
#[cfg(test)]
mod progress_tests;
mod recovery;
mod tampering;

pub use config::{ApprovalConfig, GateConfig, RunConfig, RunExecutionContext, SandboxConfig};
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

fn trusted_arg_values_for_step(_step: &WorkflowStep, _definition: &AgentDefinition) -> Vec<String> {
    Vec::new()
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
                    trusted_arg_values: trusted_arg_values_for_step(step, definition),
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
                warden_core::StepKind::Command => steps.push(None),
            }
        }
        Ok(Self {
            steps,
            env_allowlist: runner.env_allowlist(),
        })
    }
}

struct StepInvocation<'a> {
    run_id: &'a str,
    cycle_id: &'a str,
    cycle_number: u32,
    step_index: u32,
    config: &'a RunConfig,
    role: &'a Role,
    kind: warden_core::StepKind,
    agent: Option<&'a ResolvedAgent>,
    run: Option<&'a str>,
    env_allowlist: &'static [&'static str],
    worktree_manager: &'a WorktreeManager,
    commit: &'a str,
    run_base_commit: &'a str,
    run_agent_definition_snapshot: Option<&'a AgentDefinitionSnapshot>,
    prior_findings: &'a [Finding],
    cancel: CancellationToken,
}

struct EvidenceCapture<'a> {
    run_id: &'a str,
    cycle_id: &'a str,
    cycle_number: u32,
    config: &'a RunConfig,
    command: &'a AgentCommand,
    worktree_path: &'a Path,
    cancel: CancellationToken,
}

#[derive(Debug, Clone)]
struct StepResult {
    commit: String,
    findings: Vec<Finding>,
    outcome: warden_core::StepOutcome,
}

/// All mutable convergence-loop state that must survive a quota suspension.
#[derive(Debug, Clone)]
struct ConvergenceContinuation {
    run_base_commit_sha: String,
    base_commit: String,
    cycle_number: u32,
    step_cycle_numbers: Vec<u32>,
    pending_ci_findings: Vec<Finding>,
    previous_cycle_id: Option<String>,
    next_step_index: u32,
}

impl ConvergenceContinuation {
    fn new(run_base_commit_sha: String, workflow: &Workflow) -> Self {
        Self {
            base_commit: run_base_commit_sha.clone(),
            run_base_commit_sha,
            cycle_number: 1,
            step_cycle_numbers: vec![0; workflow.steps.len()],
            pending_ci_findings: Vec::new(),
            previous_cycle_id: None,
            next_step_index: workflow.entry(),
        }
    }

    fn next_run_state(&self) -> RunState {
        RunState::RunningStep(self.next_step_index)
    }
}

/// The run this [`Orchestrator`] instance is currently driving, and the [`EventBus`] its events are
/// published on.
struct RunContext {
    run_id: String,
    event_bus: EventBus,
    /// Persists `AgentProgress` off the synchronous publication path (issue #108) -- see
    /// [`Orchestrator::publish_progress_event`].
    progress_writer: ProgressWriter,
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

    /// Publishes one `AgentProgress` event, then hands the very same record to the run's
    /// [`ProgressWriter`] for persistence (issue #108).
    ///
    /// Synchronous and infallible on purpose: its only caller is the `on_stdout_line` callback
    /// `warden_sandbox` imposes the signature of, which can neither `await` nor fail. Publication
    /// comes first and is **unchanged** -- a live subscriber sees every progress event, whatever
    /// the writer then does with it (queue it, drop it over the per-step cap, fail to write it).
    fn publish_progress_event(&self, role_name: &str, detail: String) {
        let Some(context) = self.run_context.get() else {
            return;
        };

        let record = warden_core::RunEventRecord {
            id: Uuid::new_v4().to_string(),
            run_id: context.run_id.clone(),
            event: RunEvent::AgentProgress {
                role: role_name.to_string(),
                detail,
            },
            created_at: Utc::now().to_rfc3339(),
        };
        context.event_bus.publish(&record);
        // Moved, not cloned: publication is done with it, and this runs once per agent turn.
        context.progress_writer.record(record);
    }

    /// Opens a fresh persisted-progress budget for one agent invocation (see
    /// [`crate::progress_writer::MAX_PERSISTED_PROGRESS_EVENTS_PER_INVOCATION`]).
    fn begin_progress_invocation(&self) {
        if let Some(context) = self.run_context.get() {
            context.progress_writer.begin_invocation();
        }
    }

    /// Waits until every progress event queued so far is written. Called at the end of an agent
    /// invocation, **before** `AgentFinished` is persisted, so a replay reads an invocation's
    /// progress where it happened rather than after the step that produced it had already ended.
    async fn flush_progress(&self) {
        if let Some(context) = self.run_context.get() {
            context.progress_writer.flush().await;
        }
    }

    /// Writes `to` to the run's persisted state (write-ahead of intent), then dispatches whichever
    /// lifecycle hook fires [`HookPoint::on_entering(to)`], if any, and returns its aggregated
    /// outcome uninterpreted -- **the caller decides** what a `Block` or `EmitFindings` means at
    /// this particular transition (fail the run, reboucle, or merge into a step's findings), since
    /// that depends on which state is being entered and what context (step index, cycle) the
    /// caller has in hand. The point that fired, when one did, travels bundled with the outcome
    /// ([`TransitionEffect`]) so no caller ever re-derives `HookPoint::on_entering(to)` itself.
    async fn transition(&self, run_id: &str, to: RunState) -> Result<TransitionEffect> {
        let run =
            db::get_run(&self.pool, run_id)
                .await?
                .ok_or_else(|| WardenError::RunNotFound {
                    run_id: run_id.to_string(),
                })?;
        run.state.validate_transition(to, run.total_steps)?;
        db::update_run_state(&self.pool, run_id, to).await?;

        if self.hooks.is_empty() {
            return Ok(TransitionEffect::Continue);
        }
        let Some(point) = HookPoint::on_entering(to) else {
            return Ok(TransitionEffect::Continue);
        };
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
        Ok(match self.hooks.run_hooks(point, &ctx).await? {
            HookOutcome::Continue => TransitionEffect::Continue,
            HookOutcome::Block { reason } => TransitionEffect::Blocked { point, reason },
            HookOutcome::EmitFindings(findings) => {
                TransitionEffect::FindingsEmitted { point, findings }
            }
        })
    }

    /// The common tail of any lifecycle-hook [`HookOutcome::Block`]: logs `reason` and forces
    /// `run_id` into [`RunState::Failed`] -- `Block` is a barrier at every lifecycle point except
    /// [`HookPoint::OnRunEnd`] (best-effort teardown, see [`Orchestrator::run_teardown`]).
    ///
    /// If forcing `Failed` itself fails (a DB error), the run is left stranded in whatever state
    /// the earlier write-ahead put it in (e.g. `Pushed`, which -- unlike `RunningStep`/`AwaitingCi`
    /// -- `RunState::is_intermediate` does not cover, so crash recovery will never pick it back up).
    /// That residual gap is called out here at `error!` rather than silently bubbling as just
    /// another `Err` -- the caller's own error propagation still surfaces it to the operator, this
    /// only makes the stranding itself unambiguous in the logs.
    async fn fail_run_on_block(&self, run_id: &str, point: HookPoint, reason: &str) -> Result<()> {
        tracing::warn!(
            run_id,
            point = point.as_str(),
            reason,
            "lifecycle hook blocked the run"
        );
        if let Err(error) = self.transition(run_id, RunState::Failed).await {
            tracing::error!(
                run_id,
                %error,
                "failed to force the run to Failed after a hook block; it is stranded in \
                 whatever state the write-ahead left it in"
            );
            return Err(error);
        }
        Ok(())
    }

    /// Transitions into `to` and, if the lifecycle hook firing on entering it blocks, forces the
    /// run to [`RunState::Failed`] and returns it for the caller to unwind to its own single
    /// teardown/`RunFinished` tail rather than tearing down here. `Ok(None)` means the caller should
    /// proceed: the hook was `Continue`, or it emitted findings that are recorded (see
    /// [`Orchestrator::record_unrouted_findings`]) but have no workflow step to route them through
    /// at this point.
    async fn transition_or_block(&self, run_id: &str, to: RunState) -> Result<Option<RunState>> {
        match self.transition(run_id, to).await? {
            TransitionEffect::Continue => Ok(None),
            TransitionEffect::Blocked { point, reason } => {
                self.fail_run_on_block(run_id, point, &reason).await?;
                Ok(Some(RunState::Failed))
            }
            TransitionEffect::FindingsEmitted { point, findings } => {
                self.record_unrouted_findings(point, &findings).await?;
                Ok(None)
            }
        }
    }

    /// Materializes findings a lifecycle hook emitted at `point` -- unlike [`HookPoint::AfterStep`]/
    /// [`HookPoint::OnCommit`] (folded into a step's own findings in the convergence loop, see
    /// `driver.rs`), `point` carries no workflow step whose `on_clean`/`on_blocking` edges these
    /// could be routed through. **Recorded, not routed**: published as
    /// [`RunEvent::HookFindingEmitted`] (queryable via `events`, replayed by `warden-tui`), never
    /// silently dropped, but this alone never reboucles the run.
    async fn record_unrouted_findings(&self, point: HookPoint, findings: &[Finding]) -> Result<()> {
        for finding in findings {
            self.publish_event(RunEvent::HookFindingEmitted {
                point: point.as_str().to_string(),
                source: finding.source.as_str().to_string(),
                severity: finding.severity.as_str().to_string(),
                file: finding.file.clone(),
                description: finding.description.clone(),
                action: finding.action.clone(),
            })
            .await?;
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

/// One [`Orchestrator::transition`] dispatch's result: the [`HookPoint`] that fired travels bundled
/// with its outcome, so callers never re-derive `HookPoint::on_entering(to)` and assert it `Some`.
/// `#[must_use]` on purpose: dropping a transition's effect on the floor is exactly the bug #106
/// fixed. A caller that genuinely has nothing to enforce -- because the target state maps to no
/// hook point -- must say so explicitly rather than by omission.
#[must_use]
enum TransitionEffect {
    /// No hook returned anything but `Continue` -- either none fired, or every one that did was a
    /// no-op.
    Continue,
    Blocked {
        point: HookPoint,
        reason: String,
    },
    FindingsEmitted {
        point: HookPoint,
        findings: Vec<Finding>,
    },
}
