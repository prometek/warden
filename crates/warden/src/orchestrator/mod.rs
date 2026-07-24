//! The convergence loop: producer -> [gated step]* -> reboucle if findings
//! (Architecture.md §5.1, ADR-0014; generalized from a hardcoded
//! coder -> gate review -> gate test pipeline by issue #73's
//! trio-unification follow-up).
//!
//! **One uniform step-execution path (issue #73).** `workflow.steps[0]` --
//! the producer, the coder in the built-in default workflow -- is the one
//! step ever invoked via [`Orchestrator::run_producer`]; every step after it
//! is a **gated** step invoked via [`Orchestrator::run_gated_step`], the
//! exact same body for the built-in reviewer/tester and any custom role
//! (e.g. `techlead`) alike. No step is special-cased by role name in either
//! function -- `warden_core::workflow::Role` (an open, workflow-declared
//! string) is the only thing that identifies a step; the closed `AgentRole`
//! enum still exists, but only as a convenience for resolving the built-in
//! default workflow's own agent definitions (`main.rs`), never as a branch
//! point in this loop.
//!
//! Gated steps never run in parallel (ADR-0003 amendment): each one only
//! starts once the previous one in the sequence came back clean this cycle.
//! The first gated step (`workflow.steps[1]`) gets one additional,
//! positional (not role-name) mechanic: its very first pass over a run's
//! body of work is full (the whole diff); every re-review that follows a
//! producer correction is scoped to just that correctif plus the findings
//! that motivated it (`warden_core::ReviewScope`) -- decision #37 Q2's
//! scoped-re-review optimization was always about *whichever step reviews
//! first*, not a role literally named "reviewer". A later gated step's
//! blocking finding reboucles to the producer exactly like an earlier one
//! does, going through that same scoped re-review gate before it is ever
//! handed the correctif's commit again.
//!
//! Per-step budgets: `max_review_cycles`/`max_test_cycles` (config.rs) back
//! `workflow.steps[1]`/`steps[2]` specifically (positional, decision #37 Q1
//! -- a blocking finding is charged to whichever step raised it); any step
//! beyond those two shares `max_extra_step_cycles`, one shared budget for
//! the whole remaining chain. [`warden_core::decide_next_state_for_step`]
//! decides the next [`RunState`] for any step, at any position, uniformly.
//! Each step gets its own worktree synced onto the producer's commit (see
//! [`crate::worktree::WorktreeManager::create`]), keyed by role, and its own
//! `agent_processes`/`cycle_token_usage` row -- generalized to every step,
//! built-in or custom (issue #73's trio-unification follow-up: no step
//! leaks a worktree or process on crash anymore, see `recovery`). Every
//! [`RunState`] transition is written to SQLite *before* the action it
//! authorizes (ADR-0004).
//!
//! Every significant transition is also published as a [`RunEvent`] --
//! persisted to `events` and broadcast live on the run's [`EventBus`] -- so
//! a `warden-tui` can observe the run without polling SQLite (ADR-0008,
//! issue #8). A running agent's own declarative progress
//! (`RunEvent::AgentProgress`) is broadcast on the same [`EventBus`] but
//! deliberately **not** persisted to `events` -- a late `warden-tui` attach
//! never replays it (ADR-0008 amendment, issue #33).
//!
//! # Module layout
//!
//! This module is a thin facade: the [`Orchestrator`] type and its shared
//! internal data types live here; behaviour is split by responsibility into
//! submodules --
//! - `config`: `RunConfig`/`GateConfig`/`UntrustedRepoAgentDefinition`.
//! - `convergence`: the main loop (`Orchestrator::run_convergence_loop`).
//! - `gate_tail`: the post-`Converged` push/PR/CI tail (ADR-0011).
//! - `agents`: workflow step invocation (`run_producer`/`run_gated_step`).
//! - `agent_run`: the sandboxed subprocess seam (`run_agent`, `SandboxGuard`).
//! - `evidence_capture`: evidence capture/commit around a cycle (ADR-0009).
//! - `tampering`: cross-run agent-definition-poisoning detection (issue #30) --
//!   still scoped to the built-in trio's own `.warden/agents/` convention,
//!   a distinct, pre-existing security feature this issue does not extend.
//! - `diff`: bounded diff/HEAD-commit reads.
//! - `recovery`: crash recovery (`recover_crashed_runs`/`resume_awaiting_ci_runs`).

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
    DIFF_TRUNCATED_MARKER,
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
use crate::process::{self, AgentCommand, AgentOutcome};
use crate::tool_adapter::ToolAdapter;
use crate::worktree::{self, WorktreeManager};

mod agent_run;
mod agents;
mod config;
mod convergence;
mod diff;
mod evidence_capture;
mod gate_tail;
mod recovery;
mod tampering;
#[cfg(test)]
pub(crate) mod test_support;

pub use config::{GateConfig, RunConfig, UntrustedRepoAgentDefinition};
pub use recovery::{recover_crashed_runs, resume_awaiting_ci_runs};

use tampering::AgentDefinitionSnapshot;

/// One step's markdown definition (issue #24), already mapped onto the
/// command to spawn for it: what to run, and what to tell it it is.
///
/// Resolved once per run rather than per invocation — a definition is static
/// for a run's whole lifetime and a [`ToolAdapter`] is a pure mapping, so
/// re-running it per cycle would produce the identical command.
struct ResolvedAgent {
    command: AgentCommand,
    /// `AgentDefinition::system_prompt`, cloned once per run. Owned rather
    /// than borrowed from `RunConfig` purely to keep a lifetime out of every
    /// signature it's threaded through.
    system_prompt: String,
}

/// Issue #73 (trio-unification follow-up): every workflow step's resolved
/// agent, in `config.workflow.steps` order -- `steps[0]` is always the
/// producer's (the coder, in the built-in default workflow). No role is
/// privileged here: the built-in coder/reviewer/tester and any custom step
/// are resolved and stored identically.
struct ResolvedAgents {
    steps: Vec<ResolvedAgent>,
    /// This run's `--tool` adapter's own env allowlist (issue #24), resolved
    /// once here since it's a property of the tool, not of any one role --
    /// `--tool` is global for a run (issue #24, "Sélection d'outil par
    /// rôle... hors scope"), so every step shares it.
    env_allowlist: &'static [&'static str],
}

impl ResolvedAgents {
    /// Maps every step's definition up-front, before the loop spawns
    /// anything: a definition the adapter cannot honour must fail the run at
    /// its start, not several cycles in when that step first happens to run.
    fn resolve<R: ToolAdapter>(runner: &R, config: &RunConfig) -> Result<Self> {
        let resolve_one = |definition: &AgentDefinition| -> Result<ResolvedAgent> {
            Ok(ResolvedAgent {
                command: runner.build_command(definition)?,
                system_prompt: definition.system_prompt.clone(),
            })
        };
        let steps = config
            .step_agents
            .iter()
            .map(resolve_one)
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            steps,
            env_allowlist: runner.env_allowlist(),
        })
    }
}

/// Parameters for a single producer invocation (`workflow.steps[0]` -- the
/// coder in the built-in default workflow). Grouped into a struct (rather
/// than passed positionally) purely to keep `run_producer`'s signature
/// readable — it has no behaviour of its own.
struct ProducerInvocation<'a> {
    run_id: &'a str,
    cycle_id: &'a str,
    cycle_number: u32,
    config: &'a RunConfig,
    /// The producer step's own open role (`workflow.steps[0].role`) --
    /// issue #73's trio-unification follow-up: no longer hardcoded to
    /// `"coder"`, since a workflow's first step may be named anything.
    role: &'a Role,
    /// This run's producer command + system prompt (issue #24).
    agent: &'a ResolvedAgent,
    /// This run's `--tool` adapter's env allowlist (issue #24) --
    /// `ResolvedAgents::env_allowlist`.
    env_allowlist: &'static [&'static str],
    worktree_manager: &'a WorktreeManager,
    base_commit: &'a str,
    /// Issue #30: the run-start snapshot of the built-in trio's raw,
    /// unparsed definition bytes (resolved once in `run_convergence_loop`,
    /// before cycle 1's producer ever runs) -- what
    /// `agent_definition_tampering_finding` compares this cycle's own
    /// re-resolution against (a throwaway checkout of this cycle's
    /// resulting commit, issue #30 review, HIGH -- see that function's own
    /// docs), every cycle, regardless of which cycle actually introduced a
    /// divergence. See `run_convergence_loop`'s own comment on
    /// `run_agent_definition_snapshot` for why this must be the run's fixed
    /// start, not something recomputed per cycle.
    run_agent_definition_snapshot: &'a AgentDefinitionSnapshot,
    /// A2 (ADR-0013, issue #22): the findings that triggered this cycle —
    /// what the producer is being asked to fix, fed to it as
    /// `AgentInputMessage::findings`. The very same list every gated step of
    /// this cycle is told triggered it (`select_prior_findings`), including
    /// CI findings on a post-convergence reboucle (ADR-0011). Empty on a
    /// run's first cycle.
    prior_findings: &'a [Finding],
    cancel: CancellationToken,
}

/// Parameters for one cycle's evidence capture (ADR-0009). Grouped into a
/// struct (rather than passed positionally) purely to keep
/// `capture_evidence_for_cycle`/`try_capture_evidence_for_cycle`'s
/// signatures readable — the same convention as [`ProducerInvocation`] /
/// [`GatedStepInvocation`]; it has no behaviour of its own.
struct EvidenceCapture<'a> {
    run_id: &'a str,
    cycle_id: &'a str,
    cycle_number: u32,
    config: &'a RunConfig,
    /// The command the tester step was invoked with, mapped from its
    /// definition by this run's `ToolAdapter` (issue #24) — what
    /// `asciinema rec` records as the session. Passed explicitly because
    /// `RunConfig` holds definitions rather than commands: only the adapter
    /// can map one to the other.
    tester_command: &'a AgentCommand,
    tester_worktree_path: &'a Path,
    cancel: CancellationToken,
}

/// Parameters for a single **gated** workflow step invocation (issue #73,
/// trio-unification follow-up) -- any step but the producer
/// (`workflow.steps[0]`), whether that's the built-in reviewer/tester or a
/// custom role like `techlead`. One uniform shape for every such step: no
/// role is special-cased here.
struct GatedStepInvocation<'a> {
    run_id: &'a str,
    cycle_id: &'a str,
    cycle_number: u32,
    /// This step's 0-based index in `config.workflow.steps` -- never `0`
    /// (the producer's own index). Used for worktree/bookkeeping labels and
    /// error messages; the actual gating decision is made by the caller
    /// (`run_convergence_loop`, via `decide_next_state_for_step`), not here.
    step_index: u32,
    role: &'a Role,
    /// This step's command + system prompt (issue #24).
    agent: &'a ResolvedAgent,
    /// This run's `--tool` adapter's env allowlist (issue #24) --
    /// `ResolvedAgents::env_allowlist`.
    env_allowlist: &'static [&'static str],
    worktree_manager: &'a WorktreeManager,
    commit: &'a str,
    /// The diff this cycle's producer introduced against the cycle's
    /// starting commit -- fed to the agent as `AgentInputMessage::diff`
    /// (ADR-0012, issue #20 Scope B), unless `scope` narrows it to a
    /// correctif (issue #40).
    diff: &'a str,
    /// Findings that triggered this cycle (including CI findings on a
    /// post-convergence reboucle, ADR-0011) -- fed to the agent as
    /// `AgentInputMessage::findings` (ADR-0012). Empty on a run's first
    /// cycle. Read as "the findings that prompted this correctif" instead
    /// when `scope` is `Correctif` (issue #40).
    prior_findings: &'a [Finding],
    /// `ReviewScope::Full` for every step except the first gated one on a
    /// cycle that follows the run's very first full pass;
    /// `ReviewScope::Correctif` only then (issue #40, decision #37 Q2) --
    /// see [`warden_core::ReviewScope`]. This is a **positional** pipeline
    /// mechanic (`run_convergence_loop` only ever sets `Correctif` for
    /// `step_index == 1`), never a role-name check -- see
    /// [`Orchestrator::run_gated_step`]'s own docs for the defensive
    /// re-check this struct's `step_index` backs.
    scope: warden_core::ReviewScope,
    /// Only consulted when this step is functionally "the tester" (evidence
    /// capture, `evidence_tool`/`evidence_store_in_repo`/`warden_home`) --
    /// carried through here rather than threading four separate fields. See
    /// [`Orchestrator::run_gated_step`]'s own docs on this one remaining,
    /// deliberately out-of-scope role-name check (ADR-0009 is a distinct
    /// feature from this issue's execution-path unification).
    config: &'a RunConfig,
    cancel: CancellationToken,
}

/// Outcome of a single producer invocation within a cycle: the commit it
/// produced, and the diff introduced against the cycle's starting commit --
/// the latter is fed to every gated step as `AgentInputMessage::diff`
/// (ADR-0012, issue #20 Scope B).
struct ProducerCycleResult {
    commit: String,
    diff: String,
    /// Issue #24 review, M4: `Some` when this cycle's producer commit
    /// touches `.warden/agents/` against the run's original starting commit
    /// -- see `agent_definition_tampering_finding`'s own docs. `None` on the
    /// overwhelmingly common case (the producer never touches that
    /// directory).
    definition_tampering_finding: Option<Finding>,
}

/// The run this [`Orchestrator`] instance is currently driving, and the
/// [`EventBus`] its events are published on. Set exactly once, at the top of
/// [`Orchestrator::run_convergence_loop`] -- an orchestrator is one-run-
/// per-instance in this codebase (a fresh one is constructed per CLI
/// invocation, see `main.rs`), so this never needs to change after that.
struct RunContext {
    run_id: String,
    event_bus: EventBus,
}

/// One [`Orchestrator::drive_post_convergence_tail`] call's verdict: either
/// the run has reached a terminal [`RunState`] (`Done`/`Failed` -- see
/// [`Orchestrator::apply_ci_result_message`]), or `ChecksFailed` reboucles to
/// the coder within budget, carrying the CI findings to seed into the next
/// cycle.
#[derive(Debug)]
enum PostConvergenceOutcome {
    Terminal(RunState),
    Reboucle { findings: Vec<Finding> },
}

/// Drives the convergence loop against a persisted [`SqlitePool`].
pub struct Orchestrator {
    pool: SqlitePool,
    /// `None` until [`Orchestrator::run_convergence_loop`] starts a run.
    /// Read by [`Orchestrator::publish_event`], called from deep inside the
    /// agent-invocation call chain (`run_agent`) without needing to thread
    /// an `&EventBus`/`run_id` pair through every intermediate signature --
    /// several of those (`run_review`, `run_test`, `run_finding_agent`) are
    /// also exercised directly by unit tests below with a fixed argument
    /// list, so adding parameters there would be a breaking, test-rippling
    /// change for a purely additive observability feature.
    run_context: tokio::sync::OnceCell<RunContext>,
    /// Issue #31: invoked synchronously with the freshly generated run id,
    /// at the exact same point `RunEvent::RunStarted` is published --
    /// before the first cycle, but after the `runs` row and the Event Bus
    /// socket both exist. Lets `main.rs` print the run id and a
    /// ready-to-copy `warden-tui attach` hint to stdout the moment the run
    /// truly starts, instead of only after `run_convergence_loop` returns.
    /// `None` by default (every test below, and any other caller that
    /// doesn't care to observe run start) -- a builder-style setter rather
    /// than a `run_convergence_loop` parameter for the same test-rippling
    /// reason as `run_context` above.
    ///
    /// Review L2: called inline, on the same task that is driving this
    /// run's convergence loop, before the coder's first cycle -- so it
    /// **must not panic** (an unwind here would abort the run mid-flight
    /// with the `runs` row left in a non-terminal state, since nothing
    /// downstream gets a chance to mark it `Failed`) and **must not block**
    /// for any meaningful length of time (whatever it does delays the
    /// coder from starting). `main.rs`'s callback keeps to a couple of
    /// non-blocking, error-checked writes to stdout for exactly this
    /// reason -- see `print_run_started_hint`'s own docs there.
    on_run_started: Option<RunStartedCallback>,
    /// Issue #50: the execution-environment isolation seam every
    /// coder/reviewer/tester invocation runs through ([`run_agent`]). Boxed
    /// behind `Arc<dyn Sandbox>` (rather than a generic parameter, unlike
    /// `R: ToolAdapter`) so a backend can be selected once, at construction
    /// time, without becoming part of every signature `Orchestrator`
    /// exposes -- `warden_sandbox::LocalSandbox` by default (strict parity
    /// with this crate's pre-issue-#50 hand-rolled process isolation, see
    /// [`Orchestrator::new`]); [`Orchestrator::with_sandbox`] is the one
    /// point a future `DockerSandbox` (#49) plugs into, with no other change
    /// to this module.
    sandbox: Arc<dyn Sandbox>,
    /// Issue #55: the lifecycle-hook registry dispatched at every relevant
    /// state transition (see [`Orchestrator::transition`]). **Empty by
    /// default** ([`HookRegistry::new`]), which makes that dispatch a strict
    /// no-op -- behaviour is unchanged until a caller installs hooks via
    /// [`Orchestrator::with_hooks`] and #51 wires their outcomes into the
    /// convergence loop. No concrete hook ships yet (issue #55 is foundation
    /// only).
    hooks: HookRegistry,
}

/// See the `on_run_started` field docs on [`Orchestrator`]. Named alias
/// only to satisfy clippy's `type_complexity` lint.
type RunStartedCallback = Box<dyn Fn(&str) + Send + Sync>;

impl Orchestrator {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            run_context: tokio::sync::OnceCell::new(),
            on_run_started: None,
            sandbox: Arc::new(LocalSandbox::new()),
            hooks: HookRegistry::new(),
        }
    }

    /// Registers `callback` to run once, synchronously, when this
    /// orchestrator's run starts (see the `on_run_started` field docs, in
    /// particular the no-panic/non-blocking contract `callback` must
    /// honour). Consumes and returns `self` so the CLI can set it up in the
    /// same expression that constructs the orchestrator (`main.rs`).
    pub fn on_run_started(mut self, callback: impl Fn(&str) + Send + Sync + 'static) -> Self {
        self.on_run_started = Some(Box::new(callback));
        self
    }

    /// Selects a [`Sandbox`] backend other than the [`LocalSandbox`] default
    /// (issue #50's "backend sélectionnable" acceptance criterion). No
    /// built-in backend other than `LocalSandbox` ships yet (`DockerSandbox`
    /// is issue #49) -- this exists so a caller (`main.rs`, or a test) can
    /// substitute one, and so #49 only ever has to add a variant/construction
    /// path there, never touch [`Orchestrator::run_agent`] itself.
    pub fn with_sandbox(mut self, sandbox: Arc<dyn Sandbox>) -> Self {
        self.sandbox = sandbox;
        self
    }

    /// Installs the lifecycle-hook [`HookRegistry`] dispatched at each relevant
    /// transition (issue #55). Defaults to empty (a no-op seam); this is how a
    /// caller -- or, once #51 lands, `main.rs` from a resolved config -- swaps
    /// in real hooks. Builder-style for the same reason as
    /// [`Orchestrator::with_sandbox`]: a construction-time choice that never
    /// becomes part of any run-time signature.
    pub fn with_hooks(mut self, hooks: HookRegistry) -> Self {
        self.hooks = hooks;
        self
    }

    /// Persists `event` to `events` and broadcasts it on the active run's
    /// [`EventBus`], using the exact same freshly generated id/timestamp for
    /// both (see `db::insert_event`'s docs on why that matters for
    /// `warden-tui`'s replay/live dedup). A no-op if no run is currently in
    /// progress on this instance -- only reachable from a test that calls a
    /// private agent-invocation method directly without going through
    /// [`Orchestrator::run_convergence_loop`] first (see the `run_context`
    /// field docs); the real CLI path always has a context set before any
    /// agent runs.
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

    /// Publishes a [`RunEvent::AgentProgress`] straight to this run's
    /// [`EventBus`], deliberately bypassing `db::insert_event` -- see this
    /// module's own docs on the ADR-0008 amendment this implements (issue
    /// #33): progress is live-only, never persisted to `events`, so a late
    /// attach never replays it.
    ///
    /// **Synchronous and best-effort by design.** Called from inside the
    /// `on_stdout_line` callback [`run_agent`](Orchestrator::run_agent) hands
    /// to [`warden_sandbox::Sandbox::execute`] (`warden_sandbox::LocalSandbox`
    /// runs it from the same per-line drain that used to be
    /// `process::wait_with_progress`, before issue #50 moved it into the
    /// sandbox seam), on the hot path draining an agent's stdout: it must
    /// never `.await` (that would insert backpressure into the very drain
    /// loop `warden_sandbox`'s own deadlock-avoidance contract depends on --
    /// [`EventBus::publish`] is itself synchronous and non-blocking for
    /// exactly this reason, see its own docs) and it must never fail the
    /// run. A missing `run_context` (e.g. a test that calls
    /// [`run_agent`](Orchestrator::run_agent) directly without going through
    /// [`run_convergence_loop`](Orchestrator::run_convergence_loop) first) is
    /// silently a no-op, the same contract [`publish_event`](Orchestrator::publish_event)
    /// already has for the same case.
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

    /// Validates and persists a state transition by re-reading the run's
    /// *currently persisted* state first, rather than trusting an
    /// in-memory value the caller already believes is correct (L5: a
    /// transition validated against a hardcoded `from` constant can never
    /// fail, even if the database has drifted from what the loop assumes).
    /// Write-ahead of intention (ADR-0004): the new state is durable before
    /// this returns, and before the caller acts on it.
    async fn transition(&self, run_id: &str, to: RunState) -> Result<()> {
        let run =
            db::get_run(&self.pool, run_id)
                .await?
                .ok_or_else(|| WardenError::RunNotFound {
                    run_id: run_id.to_string(),
                })?;
        run.state.validate_transition(to, run.total_steps)?;
        db::update_run_state(&self.pool, run_id, to).await?;

        // Issue #55: the single lifecycle-hook dispatch seam. Every legal
        // transition names the state it enters; a subset of those states is a
        // lifecycle milestone with a `HookPoint` (`HookPoint::on_entering`),
        // and hooks registered on it fire here, in registration order. With
        // the default *empty* registry there is provably nothing to dispatch
        // to (`is_empty` guard) -- the `HookContext` is not even built and
        // behaviour is strictly unchanged, which is the foundation's contract.
        //
        // Acting on the outcome -- honouring a `Block`, folding
        // `EmitFindings` into the convergence loop the way reviewer/tester/CI
        // findings are -- is deliberately out of scope (issue #51). Until it
        // lands, a non-`Continue` outcome is surfaced (a visible `warn!`,
        // never silently dropped) but not yet consumed at this seam; the
        // outcome is exercised directly against `HookRegistry::run_hooks` in
        // `crate::hook`'s tests. An `Err` -- a hook that genuinely failed to
        // run -- propagates and fails the transition.
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

    /// Dispatches the **run-level** lifecycle hooks
    /// ([`HookPoint::OnRunStart`] / [`HookPoint::OnRunEnd`]) and returns their
    /// aggregated outcome. These two points bracket a whole run rather than a
    /// state entry, so they fire from explicit calls here -- not from the
    /// `transition` seam, whose `HookPoint::on_entering` mapping deliberately
    /// excludes them. `worktree`/`commit`/`diff` are absent by construction: a
    /// setup/teardown action operates on [`HookContext::repo_path`] (the repo
    /// as a whole), not on any one role's worktree. Empty registry -> a strict
    /// no-op `Continue`, exactly like the transition seam.
    ///
    /// The caller decides how to consume the outcome: `OnRunStart` aborts the
    /// run on a [`HookOutcome::Block`]; `OnRunEnd` is best-effort teardown and
    /// ignores it (see [`Orchestrator::run_teardown`]).
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

    /// Fires [`HookPoint::OnRunEnd`] teardown, best-effort. Runs like a
    /// `finally`: on every exit path of [`Orchestrator::run_convergence_loop`],
    /// including a failed one, whatever setup [`HookPoint::OnRunStart`]
    /// established (a `docker compose` stack, scratch state) is torn down.
    /// Deliberately swallows both a `Block` (meaningless once the run is over)
    /// and an `Err` (a teardown that itself failed to run) into a `warn!`:
    /// teardown must never mask the run's own final state.
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
