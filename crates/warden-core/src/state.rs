//! The run state machine (see Architecture.md §6, `RUNS.state`).
//!
//! `RunState` is the single source of truth for what an orchestrated run is
//! doing. Every transition must be validated with [`RunState::validate_transition`]
//! and persisted *before* the corresponding action is taken (write-ahead of
//! intention, ADR-0004) — that persistence happens in the `warden` crate;
//! this module only knows which transitions are legal.
//!
//! **Issue #73**: the closed pair `Reviewing`/`Testing` (and their own
//! exhaustion states `MaxReviewCyclesExceeded`/`MaxTestCyclesExceeded`) that
//! ADR-0014 introduced for exactly two hardcoded phases is now
//! [`RunState::RunningStep`]/[`RunState::StepCyclesExceeded`], each carrying
//! the 0-based index of the currently-running [`crate::workflow::WorkflowStep`]
//! within the run's own [`crate::workflow::Workflow`] (`steps[0]` is always
//! the producer/coder-equivalent role, never itself a `RunningStep` --
//! see that module's own docs). This is what makes the state machine
//! step-indexed rather than wired to exactly two named phases: a workflow
//! with four steps has four legal `RunningStep` indices instead of the two
//! this crate used to hardcode by name. [`RunState::validate_transition`]
//! now takes the run's `total_steps` explicitly, since "is this the last
//! step" (whose clean gate converges the run) can no longer be answered
//! without knowing how many steps the run's own workflow has.
//!
//! The built-in default workflow ([`crate::workflow::Workflow::builtin_default`])
//! has exactly three steps -- coder (index 0), reviewer (index 1), tester
//! (index 2) -- so `RunningStep(1)`/`RunningStep(2)` and
//! `StepCyclesExceeded(1)`/`StepCyclesExceeded(2)` are the exact generic
//! equivalents of the old `Reviewing`/`Testing`/`MaxReviewCyclesExceeded`/
//! `MaxTestCyclesExceeded`, and every legal transition among them is
//! unchanged -- this is what makes issue #73's strict retro-compat
//! requirement (no workflow file present -> identical behaviour) true by
//! construction rather than by re-deriving it.

use crate::error::{CoreError, Result};

/// Lifecycle state of a run, mirroring the state diagram in Architecture.md §6.
///
/// Phase 1 only exercises `Pending` through `Converged` /
/// `RunningStep`'s `StepCyclesExceeded` counterpart / `Failed`. `Pushed`,
/// `AwaitingCi`, and `Done` are reserved for the git gate (Phase 3) and CI
/// watcher (Phase 5) but are modeled now so the state machine doesn't need a
/// breaking change later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RunState {
    Pending,
    CoderRunning,
    /// A gated step (issue #73: any [`crate::workflow::WorkflowStep`] but
    /// the producer) is running or awaiting its own findings. The `u32` is
    /// its 0-based index in the run's [`crate::workflow::Workflow::steps`] --
    /// never `0` (the producer's own index), since the producer runs under
    /// [`RunState::CoderRunning`], not this variant.
    RunningStep(u32),
    Converged,
    Pushed,
    AwaitingCi,
    Done,
    /// The step at this index (same indexing as [`RunState::RunningStep`])
    /// raised a blocking finding on every cycle up to and including its own
    /// cycle budget, with no clean cycle in between -- the run gives up
    /// rather than reboucling forever.
    StepCyclesExceeded(u32),
    Failed,
}

impl RunState {
    /// Stable string form used as the `RUNS.state` column value. Never
    /// change existing variants' strings without a migration.
    ///
    /// Owned (`String`), unlike most `as_str` methods in this crate, because
    /// [`RunState::RunningStep`]/[`RunState::StepCyclesExceeded`] carry a
    /// step index that has to be formatted into the string -- there is no
    /// `&'static str` that could name every possible index ahead of time.
    pub fn as_str(&self) -> String {
        match self {
            RunState::Pending => "pending".to_string(),
            RunState::CoderRunning => "coder_running".to_string(),
            RunState::RunningStep(index) => format!("running_step:{index}"),
            RunState::Converged => "converged".to_string(),
            RunState::Pushed => "pushed".to_string(),
            RunState::AwaitingCi => "awaiting_ci".to_string(),
            RunState::Done => "done".to_string(),
            RunState::StepCyclesExceeded(index) => format!("step_cycles_exceeded:{index}"),
            RunState::Failed => "failed".to_string(),
        }
    }

    /// Parses a `RUNS.state` column value back into a `RunState`. Any value
    /// that isn't one written by [`RunState::as_str`] is a boundary error,
    /// not a panic (a row could come from a corrupt DB or a future schema).
    pub fn parse(raw: &str) -> Result<Self> {
        if let Some(index) = raw.strip_prefix("running_step:") {
            return parse_step_index(raw, index).map(RunState::RunningStep);
        }
        if let Some(index) = raw.strip_prefix("step_cycles_exceeded:") {
            return parse_step_index(raw, index).map(RunState::StepCyclesExceeded);
        }
        match raw {
            "pending" => Ok(RunState::Pending),
            "coder_running" => Ok(RunState::CoderRunning),
            "converged" => Ok(RunState::Converged),
            "pushed" => Ok(RunState::Pushed),
            "awaiting_ci" => Ok(RunState::AwaitingCi),
            "done" => Ok(RunState::Done),
            "failed" => Ok(RunState::Failed),
            other => Err(CoreError::UnknownState(other.to_string())),
        }
    }

    /// States considered "mid-cycle": a run left in one of these states
    /// across an orchestrator restart is only legitimate if a live agent
    /// process is still associated with it. See Architecture.md §6, "Règle
    /// de récupération" and §9 (Disaster Recovery).
    pub fn is_intermediate(self) -> bool {
        matches!(
            self,
            RunState::CoderRunning | RunState::RunningStep(_) | RunState::AwaitingCi
        )
    }

    /// Terminal states: no further transition is legal once reached.
    pub fn is_terminal(self) -> bool {
        matches!(self, RunState::Done | RunState::Failed)
    }

    /// The states legal as this state's successor, given `total_steps`
    /// (`workflow.steps.len()`) -- needed only by [`RunState::RunningStep`],
    /// to decide whether a clean gate advances to the next step or converges
    /// the run (issue #73: this can no longer be answered without knowing
    /// how many steps the run's own workflow has).
    fn allowed_next_states(self, total_steps: u32) -> Vec<RunState> {
        match self {
            // `Failed`: a run whose `on_run_start` setup hook blocked (the
            // environment could not be established) fails before the coder
            // ever runs, straight from `Pending`.
            RunState::Pending => vec![RunState::CoderRunning, RunState::Failed],
            RunState::CoderRunning => {
                // A workflow whose only step is the producer itself (no
                // gates at all) converges directly when its cycle raised no
                // blocking finding -- there is no later gated step to gate
                // on. Every workflow this codebase ships with has at least
                // one gated step, but this keeps the machine correct for the
                // degenerate one-step case too, rather than an unreachable
                // panic.
                //
                // Issue #73 review, finding F4: a blocking finding the
                // producer's own cycle raises (`FindingSource::Warden`, the
                // agent-definition-tampering check -- there is no per-role
                // gated finding at index 0, since the producer's own role
                // never gates) must still be able to reboucle
                // (`CoderRunning`, a self-loop: the very next cycle's
                // producer run) or exhaust its budget
                // (`StepCyclesExceeded(0)`) instead of the run being forced
                // straight to `Converged` regardless. Without this, the
                // orchestrator's own convergence loop would have nowhere
                // legal to transition a blocking single-step cycle to.
                if total_steps <= 1 {
                    vec![
                        RunState::Converged,
                        RunState::CoderRunning,
                        RunState::StepCyclesExceeded(0),
                        RunState::Failed,
                    ]
                } else {
                    vec![RunState::RunningStep(1), RunState::Failed]
                }
            }
            // Issue #73 (was: `Reviewing`/`Testing`, ADR-0014): a gated
            // step's clean cycle advances to the next step, or -- once this
            // is the workflow's *last* step -- converges the run instead. A
            // blocking finding reboucles to `CoderRunning` (within this
            // step's own cycle budget) or exhausts it
            // (`StepCyclesExceeded`).
            RunState::RunningStep(index) => {
                let advance = if index + 1 >= total_steps {
                    RunState::Converged
                } else {
                    RunState::RunningStep(index + 1)
                };
                vec![
                    advance,
                    RunState::CoderRunning,
                    RunState::StepCyclesExceeded(index),
                    RunState::Failed,
                ]
            }
            RunState::Converged => vec![RunState::Pushed],
            RunState::Pushed => vec![RunState::AwaitingCi],
            RunState::AwaitingCi => {
                vec![RunState::Done, RunState::CoderRunning, RunState::Failed]
            }
            RunState::StepCyclesExceeded(_) => vec![RunState::Failed],
            RunState::Done => vec![],
            RunState::Failed => vec![],
        }
    }

    /// Validates that `self -> to` is a legal transition per the state
    /// diagram, given the run's `total_steps` (`workflow.steps.len()`) --
    /// see [`RunState::allowed_next_states`]'s own docs for why this needs
    /// it. Returns [`CoreError::InvalidTransition`] otherwise.
    pub fn validate_transition(self, to: RunState, total_steps: u32) -> Result<()> {
        if self.allowed_next_states(total_steps).contains(&to) {
            Ok(())
        } else {
            Err(CoreError::InvalidTransition { from: self, to })
        }
    }
}

/// Parses the numeric suffix of a `running_step:<n>`/`step_cycles_exceeded:<n>`
/// state string. `full_raw` is only for the error message, so a malformed
/// suffix (non-numeric, empty, overflowing `u32`) names the whole original
/// string a reader would actually see in the database, not just the
/// fragment after the prefix was already stripped.
fn parse_step_index(full_raw: &str, index: &str) -> Result<u32> {
    index
        .parse::<u32>()
        .map_err(|_| CoreError::UnknownState(full_raw.to_string()))
}

/// Role of an agent invoked during a cycle (`AGENT_PROCESSES.role`).
///
/// **Issue #73**: this remains a closed, 3-value enum on purpose -- it is
/// the *internal* wire/execution concern of the built-in coder/reviewer/
/// tester path only (the `AGENT_PROCESSES.role` column, the per-role
/// `cycles.*_worktree_path`/`*_token` columns, `ToolAdapter`'s default
/// prompts/tools, and the hardened, role-asymmetric agent-definition
/// resolution `warden::agent_def` implements for exactly these three roles --
/// see that module's own security docs). It is *not* what issue #73's "open
/// roles" requirement targets: a [`crate::workflow::Workflow`] step's own
/// role is the open [`crate::workflow::Role`], a plain validated string, with
/// no fixed set at all. Coder/reviewer/tester become the workflow engine's
/// *default* pipeline (`crate::workflow::Workflow::builtin_default`), backed
/// by this existing, unchanged, hardened `AgentRole` path; any other step
/// (e.g. a `techlead` role) is backed by `crate::workflow::Role` and a
/// simpler, generic agent resolution instead -- seeing both is what proves
/// the pipeline itself is no longer wired to exactly these three names, even
/// though this type still exists underneath the default path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentRole {
    Coder,
    Reviewer,
    Tester,
}

impl AgentRole {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentRole::Coder => "coder",
            AgentRole::Reviewer => "reviewer",
            AgentRole::Tester => "tester",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "coder" => Ok(AgentRole::Coder),
            "reviewer" => Ok(AgentRole::Reviewer),
            "tester" => Ok(AgentRole::Tester),
            other => Err(CoreError::UnknownRole(other.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The built-in default workflow (issue #73) has exactly three steps --
    /// coder (0), reviewer (1), tester (2) -- so `total_steps = 3`
    /// everywhere in this module's own tests reproduces the pre-issue-#73
    /// `Reviewing`/`Testing` transition table exactly.
    const DEFAULT_TOTAL_STEPS: u32 = 3;

    #[test]
    fn pending_can_move_to_coder_running_or_fail() {
        assert!(RunState::Pending
            .validate_transition(RunState::CoderRunning, DEFAULT_TOTAL_STEPS)
            .is_ok());
        // A blocked `on_run_start` setup hook fails the run straight from
        // `Pending`, before the coder runs.
        assert!(RunState::Pending
            .validate_transition(RunState::Failed, DEFAULT_TOTAL_STEPS)
            .is_ok());
        assert!(RunState::Pending
            .validate_transition(RunState::Converged, DEFAULT_TOTAL_STEPS)
            .is_err());
    }

    #[test]
    fn coder_running_can_fail_on_crash() {
        assert!(RunState::CoderRunning
            .validate_transition(RunState::Failed, DEFAULT_TOTAL_STEPS)
            .is_ok());
        assert!(RunState::CoderRunning
            .validate_transition(RunState::RunningStep(1), DEFAULT_TOTAL_STEPS)
            .is_ok());
    }

    #[test]
    fn coder_running_with_a_single_step_workflow_converges_directly() {
        assert!(RunState::CoderRunning
            .validate_transition(RunState::Converged, 1)
            .is_ok());
        assert!(RunState::CoderRunning
            .validate_transition(RunState::RunningStep(1), 1)
            .is_err());
    }

    /// Issue #73 review, finding F4: a single-step workflow's producer
    /// cycle can also reboucle (a blocking finding, budget not yet
    /// exhausted) or exhaust its budget (`StepCyclesExceeded(0)`) -- it is
    /// not forced straight to `Converged` regardless of what the cycle
    /// raised.
    #[test]
    fn coder_running_with_a_single_step_workflow_can_also_reboucle_or_exhaust_its_budget() {
        assert!(RunState::CoderRunning
            .validate_transition(RunState::CoderRunning, 1)
            .is_ok());
        assert!(RunState::CoderRunning
            .validate_transition(RunState::StepCyclesExceeded(0), 1)
            .is_ok());
        assert!(RunState::CoderRunning
            .validate_transition(RunState::Failed, 1)
            .is_ok());
        // A multi-step workflow's producer never reboucles/exhausts a
        // budget directly from `CoderRunning` -- that's the first *gated*
        // step's own job.
        assert!(RunState::CoderRunning
            .validate_transition(RunState::CoderRunning, DEFAULT_TOTAL_STEPS)
            .is_err());
        assert!(RunState::CoderRunning
            .validate_transition(RunState::StepCyclesExceeded(0), DEFAULT_TOTAL_STEPS)
            .is_err());
    }

    /// Issue #73 (was: `reviewing_gates_testing_and_never_converges_directly`):
    /// the first gated step (index 1, "reviewer") gates the second (index 2,
    /// "tester") -- it never jumps straight to `Converged` itself in a
    /// 3-step workflow (only a clean cycle from the *last* step does).
    #[test]
    fn a_non_last_gated_step_gates_the_next_one_and_never_converges_directly() {
        let from = RunState::RunningStep(1);
        assert!(from
            .validate_transition(RunState::RunningStep(2), DEFAULT_TOTAL_STEPS)
            .is_ok());
        assert!(from
            .validate_transition(RunState::CoderRunning, DEFAULT_TOTAL_STEPS)
            .is_ok());
        assert!(from
            .validate_transition(RunState::StepCyclesExceeded(1), DEFAULT_TOTAL_STEPS)
            .is_ok());
        assert!(from
            .validate_transition(RunState::Failed, DEFAULT_TOTAL_STEPS)
            .is_ok());
        assert!(from
            .validate_transition(RunState::Converged, DEFAULT_TOTAL_STEPS)
            .is_err());
        assert!(from
            .validate_transition(RunState::StepCyclesExceeded(2), DEFAULT_TOTAL_STEPS)
            .is_err());
    }

    /// Issue #73 (was: `testing_covers_all_convergence_outcomes`): the last
    /// gated step (index 2, "tester" in the default workflow) converges the
    /// run on a clean cycle.
    #[test]
    fn the_last_gated_step_covers_all_convergence_outcomes() {
        let from = RunState::RunningStep(2);
        assert!(from
            .validate_transition(RunState::Converged, DEFAULT_TOTAL_STEPS)
            .is_ok());
        assert!(from
            .validate_transition(RunState::CoderRunning, DEFAULT_TOTAL_STEPS)
            .is_ok());
        assert!(from
            .validate_transition(RunState::StepCyclesExceeded(2), DEFAULT_TOTAL_STEPS)
            .is_ok());
        assert!(from
            .validate_transition(RunState::Failed, DEFAULT_TOTAL_STEPS)
            .is_ok());
        assert!(from
            .validate_transition(RunState::Done, DEFAULT_TOTAL_STEPS)
            .is_err());
        assert!(from
            .validate_transition(RunState::RunningStep(3), DEFAULT_TOTAL_STEPS)
            .is_err());
    }

    /// A custom workflow with a fourth step (e.g. `techlead`, issue #73's own
    /// demonstrated new role): the third gated step (index 3) now gates
    /// instead of converging, and the *new* last step (index 3) is the one
    /// that converges.
    #[test]
    fn a_fourth_step_in_a_custom_workflow_gates_instead_of_converging() {
        let total_steps = 4;
        let tester_step = RunState::RunningStep(2);
        assert!(tester_step
            .validate_transition(RunState::RunningStep(3), total_steps)
            .is_ok());
        assert!(tester_step
            .validate_transition(RunState::Converged, total_steps)
            .is_err());

        let techlead_step = RunState::RunningStep(3);
        assert!(techlead_step
            .validate_transition(RunState::Converged, total_steps)
            .is_ok());
        assert!(techlead_step
            .validate_transition(RunState::CoderRunning, total_steps)
            .is_ok());
        assert!(techlead_step
            .validate_transition(RunState::StepCyclesExceeded(3), total_steps)
            .is_ok());
    }

    #[test]
    fn terminal_states_have_no_outgoing_transition() {
        assert!(RunState::Done
            .validate_transition(RunState::CoderRunning, DEFAULT_TOTAL_STEPS)
            .is_err());
        assert!(RunState::Failed
            .validate_transition(RunState::CoderRunning, DEFAULT_TOTAL_STEPS)
            .is_err());
    }

    #[test]
    fn converged_can_only_move_to_pushed() {
        assert!(RunState::Converged
            .validate_transition(RunState::Pushed, DEFAULT_TOTAL_STEPS)
            .is_ok());
        assert!(RunState::Converged
            .validate_transition(RunState::Done, DEFAULT_TOTAL_STEPS)
            .is_err());
        assert!(RunState::Converged
            .validate_transition(RunState::CoderRunning, DEFAULT_TOTAL_STEPS)
            .is_err());
    }

    #[test]
    fn pushed_can_only_move_to_awaiting_ci() {
        assert!(RunState::Pushed
            .validate_transition(RunState::AwaitingCi, DEFAULT_TOTAL_STEPS)
            .is_ok());
        assert!(RunState::Pushed
            .validate_transition(RunState::Done, DEFAULT_TOTAL_STEPS)
            .is_err());
    }

    #[test]
    fn awaiting_ci_covers_all_post_push_outcomes() {
        let from = RunState::AwaitingCi;
        assert!(from
            .validate_transition(RunState::Done, DEFAULT_TOTAL_STEPS)
            .is_ok());
        assert!(from
            .validate_transition(RunState::CoderRunning, DEFAULT_TOTAL_STEPS)
            .is_ok());
        assert!(from
            .validate_transition(RunState::Failed, DEFAULT_TOTAL_STEPS)
            .is_ok());
        assert!(from
            .validate_transition(RunState::Converged, DEFAULT_TOTAL_STEPS)
            .is_err());
    }

    #[test]
    fn step_cycles_exceeded_can_only_move_to_failed() {
        assert!(RunState::StepCyclesExceeded(1)
            .validate_transition(RunState::Failed, DEFAULT_TOTAL_STEPS)
            .is_ok());
        assert!(RunState::StepCyclesExceeded(1)
            .validate_transition(RunState::CoderRunning, DEFAULT_TOTAL_STEPS)
            .is_err());
    }

    #[test]
    fn intermediate_states_match_recovery_rule() {
        assert!(RunState::CoderRunning.is_intermediate());
        assert!(RunState::RunningStep(1).is_intermediate());
        assert!(RunState::RunningStep(2).is_intermediate());
        assert!(RunState::AwaitingCi.is_intermediate());
        assert!(!RunState::Pending.is_intermediate());
        assert!(!RunState::Converged.is_intermediate());
        assert!(!RunState::Failed.is_intermediate());
    }

    #[test]
    fn state_round_trips_through_its_string_form() {
        for state in [
            RunState::Pending,
            RunState::CoderRunning,
            RunState::RunningStep(1),
            RunState::RunningStep(2),
            RunState::Converged,
            RunState::Pushed,
            RunState::AwaitingCi,
            RunState::Done,
            RunState::StepCyclesExceeded(1),
            RunState::StepCyclesExceeded(2),
            RunState::Failed,
        ] {
            assert_eq!(RunState::parse(&state.as_str()).unwrap(), state);
        }
    }

    #[test]
    fn running_step_string_form_names_its_index() {
        assert_eq!(RunState::RunningStep(3).as_str(), "running_step:3");
        assert_eq!(
            RunState::StepCyclesExceeded(3).as_str(),
            "step_cycles_exceeded:3"
        );
    }

    #[test]
    fn unknown_state_string_is_a_typed_error_not_a_panic() {
        assert_eq!(
            RunState::parse("bogus"),
            Err(CoreError::UnknownState("bogus".to_string()))
        );
    }

    #[test]
    fn a_running_step_with_a_non_numeric_index_is_a_typed_error_not_a_panic() {
        assert_eq!(
            RunState::parse("running_step:ghost"),
            Err(CoreError::UnknownState("running_step:ghost".to_string()))
        );
    }

    #[test]
    fn agent_role_round_trips_through_its_string_form() {
        for role in [AgentRole::Coder, AgentRole::Reviewer, AgentRole::Tester] {
            assert_eq!(AgentRole::parse(role.as_str()).unwrap(), role);
        }
        assert!(AgentRole::parse("ghost").is_err());
    }
}
