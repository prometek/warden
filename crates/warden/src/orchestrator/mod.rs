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
//! Per-step budgets follow each step's own declared
//! [`warden_core::WorkflowStep::budget`] (issue #73 review, finding F3 --
//! before this fix, `workflow.steps[1]`/`steps[2]` were hardcoded to
//! `max_review_cycles`/`max_test_cycles`, which inverted the rule the moment
//! the built-in pair was reordered): [`warden_core::StepBudget::Review`]/
//! [`warden_core::StepBudget::Test`] back `max_review_cycles`/`max_test_cycles`
//! (decision #37 Q1 -- a blocking finding is charged to whichever step
//! raised it, wherever it sits); every other step shares
//! [`warden_core::StepBudget::Extra`] (`max_extra_step_cycles`), one budget
//! for the whole remaining chain. Evidence capture (ADR-0009, issue #7) is
//! likewise a step's own declared property now
//! ([`warden_core::WorkflowStep::captures_evidence`], finding F2), not a
//! literal role-name check. [`warden_core::decide_next_state_for_step`]
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

pub use config::{GateConfig, RunConfig, UntrustedRepoAgentDefinition};
pub use recovery::{recover_crashed_runs, resume_awaiting_ci_runs, resume_quota_suspended_runs};

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
    /// Issue #59 review, MEDIUM 4: values from this step's own
    /// `AgentDefinition` that [`process::validate_agent_program`] treats as
    /// vouched-safe non-paths, even though they would otherwise look
    /// path-like (e.g. a vendor-prefixed `model: anthropic/claude-3-opus`).
    /// Computed once, in [`trusted_arg_values_for_step`] -- **empty unless
    /// this step is the built-in reviewer/tester *and* its definition was
    /// actually resolved from trusted user config**, never from the repo
    /// under review -- see that function's own docs for exactly why every
    /// other case (the producer, any custom step, or a reviewer/tester
    /// definition read from the repo under `--trust-repo-agents`) always
    /// gets an empty list here.
    trusted_arg_values: Vec<String>,
}

/// Issue #59 review, MEDIUM 4: decides which of `definition`'s own values
/// `step`'s [`ResolvedAgent`] may later vouch for as non-paths to
/// [`process::validate_agent_program`] -- today, only `definition.model`
/// (the concrete false positive the review demonstrated: a vendor-prefixed
/// model id like `anthropic/claude-3-opus` is indistinguishable from a
/// relative path by the separator-based heuristic alone).
///
/// **The only case this ever returns anything**: `step.role` is
/// `"reviewer"` or `"tester"` (the built-in gated pair -- never the
/// producer, which the guard doesn't even check) *and* `untrusted_repo_agent_definitions`
/// (issue #26, [`RunConfig::untrusted_repo_agent_definitions`]) contains no
/// entry for that role. That list is non-empty for a role only when
/// `agent_def::resolve_agent_definition` actually read that role's
/// definition from the repo under review (`--trust-repo-agents`, no
/// user-config file for that role) -- exactly the one case where `model`
/// *is* coder-controlled. Every other shape of step gets an empty list,
/// unconditionally:
/// - **Any step whose role is not literally `"reviewer"`/`"tester"`** --
///   most importantly any custom workflow step beyond the built-in trio
///   (`techlead`, ...): `agent_def::resolve_custom_step_agent_definition`
///   reads `<repo>/.claude/agents/<agent>.md` **unconditionally**, no
///   trusted alternative exists for a custom role, so its `model` (if any)
///   is always coder-controlled repo content. The producer step falls in
///   here too, though it's moot -- `validate_agent_program` never checks a
///   producer's `program`/`args` at all.
/// - **A reviewer/tester step whose definition *was* sourced from the
///   repo** (`untrusted_repo_agent_definitions` names that role): its
///   `model`, along with the rest of that definition, is exactly the
///   coder-controlled content this whole guard exists to distrust.
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

/// Issue #73 (trio-unification follow-up); issue #79: every workflow step's
/// resolved agent, in `config.workflow.steps` order -- `steps[0]` is always
/// the producer's (the coder in the built-in default workflow, always
/// `StepKind::Agent`, `Workflow::parse_yaml`'s own invariant). No role is
/// privileged here: the built-in coder/reviewer/tester and any custom `type:
/// agent` step are resolved and stored identically. `None` at a `type: hook`
/// step's own index (issue #79) -- there is no agent definition to resolve
/// for it, and [`Orchestrator::run_gated_step`] never reads this entry for
/// such a step.
struct ResolvedAgents {
    steps: Vec<Option<ResolvedAgent>>,
    /// This run's `--tool` adapter's own env allowlist (issue #24), resolved
    /// once here since it's a property of the tool, not of any one role --
    /// `--tool` is global for a run (issue #24, "Sélection d'outil par
    /// rôle... hors scope"), so every step shares it.
    env_allowlist: &'static [&'static str],
}

impl ResolvedAgents {
    /// Maps every `type: agent` step's definition up-front, before the loop
    /// spawns anything: a definition the adapter cannot honour must fail the
    /// run at its start, not several cycles in when that step first happens
    /// to run.
    ///
    /// Issue #73 review (F5); issue #79: `config.step_agents` carries one
    /// entry per `type: agent` step in `config.workflow.steps`, in that same
    /// relative order -- **not** one entry per `workflow.steps` overall (a
    /// `type: hook` step has no agent definition at all, so `main.rs`'s own
    /// resolution loop never pushes one for it). The one-time count check
    /// below is what turns a would-be out-of-bounds panic deep into a run
    /// into a fail-fast, typed error before the run even starts, generalized
    /// off "one per agent-kind step" rather than "one per step".
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
/// trio-unification follow-up; issue #79) -- any step but the producer
/// (`workflow.steps[0]`), whether that's the built-in reviewer/tester, a
/// custom `type: agent` role like `techlead`, or a `type: hook` step. One
/// uniform shape for every such step: no role is special-cased here.
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
    /// Issue #79: which mechanism this step runs through --
    /// [`Orchestrator::run_gated_step`]'s own dispatch. Carried in from
    /// `config.workflow.steps[step_index].kind` by the caller, exactly like
    /// `captures_evidence` below.
    kind: warden_core::StepKind,
    /// This step's command + system prompt (issue #24). `Some` iff `kind ==
    /// StepKind::Agent` -- `ResolvedAgents::resolve`'s own invariant.
    agent: Option<&'a ResolvedAgent>,
    /// Issue #79: the shell command a `type: hook` step runs. `Some` iff
    /// `kind == StepKind::Hook` -- `warden_core::WorkflowStep::run`'s own
    /// invariant, carried through unchanged.
    run: Option<&'a str>,
    /// This run's `--tool` adapter's env allowlist (issue #24) --
    /// `ResolvedAgents::env_allowlist`. Unused for a `type: hook` step (it
    /// spawns no agent subprocess).
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
    /// `ReviewScope::Full` for every step except one following its own first
    /// full pass over a run's body of work; `ReviewScope::Correctif` then
    /// (issue #40, decision #37 Q2; generalized beyond the built-in reviewer
    /// by issue #81) -- see [`warden_core::ReviewScope`]. Legal in exactly
    /// two cases, both decided by `run_convergence_loop`, never by this
    /// struct's own caller: the first gated step (`step_index == 1`,
    /// positional, retained unchanged for retro-compat), or any step whose
    /// own declared `gate` is [`warden_core::Gate::ScopedReReview`] --
    /// [`Orchestrator::run_gated_step`]'s own docs describe the defensive
    /// re-check this backs. That re-check derives the step's own declared
    /// gate from `config.workflow.steps[step_index]` itself (issue #81
    /// review, LOW) rather than trusting a separate field this same struct's
    /// caller would otherwise supply -- a caller passing a mismatched
    /// `step_index`/gate pair could otherwise defeat the very re-check meant
    /// to catch it.
    scope: warden_core::ReviewScope,
    /// This step's own declared [`warden_core::WorkflowStep::captures_evidence`]
    /// (issue #73 review, finding F2) -- whether a clean run of *this* step
    /// triggers ADR-0009 evidence capture. Before this, `run_gated_step`
    /// checked `role.as_str() == "tester"` directly; a custom workflow that
    /// renamed its test step lost evidence capture silently. Now it's the
    /// step's own declared property, carried in from `config.workflow.steps
    /// [step_index]` by the caller, so `run_gated_step` itself never
    /// consults a role name at all.
    captures_evidence: bool,
    /// Consulted only when `captures_evidence` is set (evidence capture's
    /// own config: `evidence_tool`/`evidence_store_in_repo`/`warden_home`) --
    /// carried through here rather than threading three separate fields.
    config: &'a RunConfig,
    cancel: CancellationToken,
}

/// Outcome of a single producer invocation within a cycle: the commit it
/// produced, and the diff introduced against the cycle's starting commit --
/// the latter is fed to every gated step as `AgentInputMessage::diff`
/// (ADR-0012, issue #20 Scope B).
#[derive(Debug, Clone)]
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

/// All mutable convergence-loop state that must survive a quota suspension.
/// The static [`RunConfig`] is checkpointed alongside this value; keeping the
/// two separate makes the workflow boundary itself explicit.
#[derive(Debug, Clone)]
struct ConvergenceContinuation {
    /// Fixed commit against which agent-definition tampering is checked for
    /// the lifetime of the run.
    run_base_commit_sha: String,
    /// Commit the next producer cycle starts from, or the commit every
    /// remaining gated step in the active cycle inspects.
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

/// The cycle row already exists when a process suspends during an invocation
/// or between two steps. Its phase says exactly which invocation is next.
#[derive(Debug, Clone)]
struct ActiveCycleContinuation {
    cycle_id: String,
    prior_findings: Vec<Finding>,
    producer_base_commit: String,
    phase: ActiveCyclePhase,
}

#[derive(Debug, Clone)]
enum ActiveCyclePhase {
    /// The producer invocation did not complete successfully and must be
    /// retried from the same cycle boundary.
    Producer,
    /// The producer completed and `next_step_index` is the first gated step
    /// that has not completed in this cycle.
    Gated {
        producer_result: ProducerCycleResult,
        findings: Vec<Finding>,
        next_step_index: u32,
        entered_extra_budget_this_cycle: bool,
    },
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
    /// Issue #51/ADR-0016: the policy decision layer consulted before this
    /// run's `git_push` action (`gate_tail::drive_post_convergence_tail`,
    /// right before staging the converged commit into the local bare gate
    /// repo -- never `origin` itself, which stays `warden-gated`'s own,
    /// independently re-verified job, unchanged). **No rules by default**
    /// ([`PolicyGate::empty`]), so this check is a strict no-op until a
    /// caller installs one via [`Orchestrator::with_policy_gate`].
    policy_gate: Arc<PolicyGate>,
    quota_anticipation_threshold: f64,
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
            policy_gate: Arc::new(PolicyGate::empty()),
            quota_anticipation_threshold: 0.90,
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

    /// Installs the [`PolicyGate`] consulted before this run's `git_push`
    /// action (issue #51/ADR-0016). Defaults to [`PolicyGate::empty`] (a
    /// no-op seam, every push allowed); `main.rs` resolves a real one from
    /// `.warden/policy.yaml` (`crate::policy_config::load_repo_policy`) and
    /// installs it here, the same builder-style construction-time choice as
    /// [`Orchestrator::with_sandbox`]/[`Orchestrator::with_hooks`].
    pub fn with_policy_gate(mut self, policy_gate: Arc<PolicyGate>) -> Self {
        self.policy_gate = policy_gate;
        self
    }

    /// Sets the fraction of consumed CLI quota at which the next workflow
    /// step is withheld. The CLI validates this public configuration value;
    /// keeping the default here preserves API callers' historical behavior.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::test_support::*;
    use warden_core::Gate;

    /// A minimal, filesystem-untouched `RunConfig` -- `ResolvedAgents::resolve`
    /// only ever reads `workflow`/`step_agents`, never the filesystem, so
    /// `repo_path`/`warden_home` need not exist.
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

    /// Issue #79 review, MEDIUM: `MismatchedStepAgentCount` changed
    /// semantics from "one agent per step" to "one agent per `type: agent`
    /// step" -- this is what makes every `.expect(...)` downstream of
    /// `ResolvedAgents::resolve` (e.g. `definitions.next().expect(...)`,
    /// `agents.steps[0].as_ref().expect(...)` in `convergence.rs`) sound.
    /// Pins the new counting rule directly: a workflow with exactly one
    /// `type: agent` step (the producer) and two `type: hook` steps must
    /// reject anything but exactly one resolved agent definition.
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

    /// Issue #79 review, MEDIUM: pins the alignment invariant every
    /// `agents.steps[i]` access relies on -- a `type: agent` step resolves
    /// to `Some`, a `type: hook` step to `None`, at that step's own index in
    /// `config.workflow.steps`, not merely "the right count".
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

    /// A minimal [`WorkflowStep`] naming `role_name` -- every field besides
    /// `role` is irrelevant to [`trusted_arg_values_for_step`], which only
    /// ever reads `step.role`. Always `StepKind::Agent` (issue #79): a
    /// `type: hook` step resolves no agent definition at all, so this
    /// function is never reached for one.
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

    /// The only case that ever vouches for anything: a reviewer/tester step
    /// whose role has no entry in `untrusted_repo_agent_definitions` (i.e.
    /// its definition was resolved from trusted user config, never the repo
    /// under review).
    #[test]
    fn a_reviewer_or_tester_step_sourced_from_trusted_config_vouches_for_its_model() {
        for role_name in ["reviewer", "tester"] {
            let definition = definition_with_model("anthropic/claude-3-opus");
            let trusted = trusted_arg_values_for_step(&step(role_name), &definition, &[]);
            assert_eq!(trusted, vec!["anthropic/claude-3-opus".to_string()]);
        }
    }

    /// Issue #59 review, MEDIUM 4's own explicit ask: the vouching must
    /// come from trusted config, never from repo content. A reviewer/tester
    /// step whose definition *was* actually read from the repo under review
    /// (`--trust-repo-agents`, tracked in `untrusted_repo_agent_definitions`)
    /// must get an empty list -- its `model` is exactly the coder-controlled
    /// content this whole guard exists to distrust, even though it's the
    /// built-in reviewer/tester role.
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

        // The tester's own entry is untouched by the reviewer's -- proves
        // this is looked up per-role, not a blanket "any untrusted entry
        // exists" flag.
        let tester_trusted = trusted_arg_values_for_step(&step("tester"), &definition, &untrusted);
        assert_eq!(tester_trusted, vec!["anthropic/claude-3-opus".to_string()]);
    }

    /// Any step beyond the built-in reviewer/tester pair -- most
    /// importantly a custom workflow step -- never vouches for anything,
    /// regardless of `untrusted_repo_agent_definitions`: a custom step's
    /// definition is always read from `<repo>/.claude/agents/<agent>.md`
    /// (`agent_def::resolve_custom_step_agent_definition`), unconditionally
    /// coder-controlled content, with no trusted alternative to fall back
    /// to the way reviewer/tester have one.
    #[test]
    fn a_custom_step_never_vouches_for_anything() {
        let definition = definition_with_model("anthropic/claude-3-opus");
        let trusted = trusted_arg_values_for_step(&step("techlead"), &definition, &[]);
        assert!(trusted.is_empty());
    }

    /// The producer step is also never vouched for -- moot in practice
    /// (`validate_agent_program` never even checks a producer's `program`/
    /// `args`), but this function must not special-case `"coder"` into
    /// behaving like `"reviewer"`/`"tester"`.
    #[test]
    fn the_producer_step_never_vouches_for_anything() {
        let definition = definition_with_model("anthropic/claude-3-opus");
        let trusted = trusted_arg_values_for_step(&step("coder"), &definition, &[]);
        assert!(trusted.is_empty());
    }

    /// A trusted reviewer/tester step with no `model` set vouches for
    /// nothing -- there is no value to vouch for, not an error.
    #[test]
    fn a_trusted_step_with_no_model_set_vouches_for_nothing() {
        let definition = AgentDefinition::new(None, None, None, None, "be an agent").unwrap();
        let trusted = trusted_arg_values_for_step(&step("reviewer"), &definition, &[]);
        assert!(trusted.is_empty());
    }
}
