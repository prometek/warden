//! The run state machine (see Architecture.md §6, `RUNS.state`).
//!
//! `RunState` is the single source of truth for what an orchestrated run is
//! doing. Every transition must be validated with [`RunState::validate_transition`]
//! and persisted *before* the corresponding action is taken (write-ahead of
//! intention, ADR-0004) — that persistence happens in the `warden` crate;
//! this module only knows which transitions are legal.

use crate::error::{CoreError, Result};

/// Lifecycle state of a run, mirroring the state diagram in Architecture.md §6.
///
/// **ADR-0014, issue #37/#43**: the single `AwaitingReviewTest` state (and its
/// one shared `max_cycles` budget) that Phase A/B (#41/#42) still ran on is
/// split into two per-phase states -- `Reviewing` (coder->reviewer, gating
/// the tester) and `Testing` (tester only ever runs once a cycle's review is
/// clean) -- each with its own exhaustion state
/// (`MaxReviewCyclesExceeded`/`MaxTestCyclesExceeded`) and budget, tracked by
/// `warden::db`'s `runs.max_review_cycles`/`max_test_cycles` columns and
/// decided by [`crate::decide_next_state`]. A scoped re-review triggered by a
/// tester finding is charged to the review budget, never the test budget
/// (decision #37 Q1) -- see [`crate::decide_next_state`]'s own docs for how.
///
/// Phase 1 only exercises `Pending` through `Converged` /
/// `MaxReviewCyclesExceeded`/`MaxTestCyclesExceeded` / `Failed`. `Pushed`,
/// `AwaitingCi`, and `Done` are reserved for the git gate (Phase 3) and CI
/// watcher (Phase 5) but are modeled now so the state machine doesn't need a
/// breaking change later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RunState {
    Pending,
    CoderRunning,
    Reviewing,
    Testing,
    Converged,
    Pushed,
    AwaitingCi,
    Done,
    MaxReviewCyclesExceeded,
    MaxTestCyclesExceeded,
    Failed,
}

impl RunState {
    /// Stable string form used as the `RUNS.state` column value. Never
    /// change existing variants' strings without a migration.
    pub fn as_str(self) -> &'static str {
        match self {
            RunState::Pending => "pending",
            RunState::CoderRunning => "coder_running",
            RunState::Reviewing => "reviewing",
            RunState::Testing => "testing",
            RunState::Converged => "converged",
            RunState::Pushed => "pushed",
            RunState::AwaitingCi => "awaiting_ci",
            RunState::Done => "done",
            RunState::MaxReviewCyclesExceeded => "max_review_cycles_exceeded",
            RunState::MaxTestCyclesExceeded => "max_test_cycles_exceeded",
            RunState::Failed => "failed",
        }
    }

    /// Parses a `RUNS.state` column value back into a `RunState`. Any value
    /// that isn't one written by [`RunState::as_str`] is a boundary error,
    /// not a panic (a row could come from a corrupt DB or a future schema).
    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "pending" => Ok(RunState::Pending),
            "coder_running" => Ok(RunState::CoderRunning),
            "reviewing" => Ok(RunState::Reviewing),
            "testing" => Ok(RunState::Testing),
            "converged" => Ok(RunState::Converged),
            "pushed" => Ok(RunState::Pushed),
            "awaiting_ci" => Ok(RunState::AwaitingCi),
            "done" => Ok(RunState::Done),
            "max_review_cycles_exceeded" => Ok(RunState::MaxReviewCyclesExceeded),
            "max_test_cycles_exceeded" => Ok(RunState::MaxTestCyclesExceeded),
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
            RunState::CoderRunning | RunState::Reviewing | RunState::Testing | RunState::AwaitingCi
        )
    }

    /// Terminal states: no further transition is legal once reached.
    pub fn is_terminal(self) -> bool {
        matches!(self, RunState::Done | RunState::Failed)
    }

    fn allowed_next_states(self) -> &'static [RunState] {
        match self {
            RunState::Pending => &[RunState::CoderRunning],
            RunState::CoderRunning => &[RunState::Reviewing, RunState::Failed],
            // Issue #43: `Testing` is reached only once this cycle's review
            // is clean (Phase A gate, issue #41) -- `Converged` is therefore
            // never a legal direct successor of `Reviewing` itself, only of
            // `Testing` (a review-clean cycle whose tester also came back
            // clean). A blocking reviewer/tampering finding reboucles to
            // `CoderRunning` (within budget) or exhausts the review budget.
            RunState::Reviewing => &[
                RunState::Testing,
                RunState::CoderRunning,
                RunState::MaxReviewCyclesExceeded,
                RunState::Failed,
            ],
            RunState::Testing => &[
                RunState::Converged,
                RunState::CoderRunning,
                RunState::MaxTestCyclesExceeded,
                RunState::Failed,
            ],
            RunState::Converged => &[RunState::Pushed],
            RunState::Pushed => &[RunState::AwaitingCi],
            RunState::AwaitingCi => &[RunState::Done, RunState::CoderRunning, RunState::Failed],
            RunState::MaxReviewCyclesExceeded => &[RunState::Failed],
            RunState::MaxTestCyclesExceeded => &[RunState::Failed],
            RunState::Done => &[],
            RunState::Failed => &[],
        }
    }

    /// Validates that `self -> to` is a legal transition per the state
    /// diagram. Returns [`CoreError::InvalidTransition`] otherwise.
    pub fn validate_transition(self, to: RunState) -> Result<()> {
        if self.allowed_next_states().contains(&to) {
            Ok(())
        } else {
            Err(CoreError::InvalidTransition { from: self, to })
        }
    }
}

/// Role of an agent invoked during a cycle (`AGENT_PROCESSES.role`).
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

    #[test]
    fn pending_can_only_move_to_coder_running() {
        assert!(RunState::Pending
            .validate_transition(RunState::CoderRunning)
            .is_ok());
        assert!(RunState::Pending
            .validate_transition(RunState::Converged)
            .is_err());
    }

    #[test]
    fn coder_running_can_fail_on_crash() {
        assert!(RunState::CoderRunning
            .validate_transition(RunState::Failed)
            .is_ok());
        assert!(RunState::CoderRunning
            .validate_transition(RunState::Reviewing)
            .is_ok());
    }

    /// Issue #43: `Reviewing` gates `Testing` -- it never jumps straight to
    /// `Converged` itself (only a review-clean cycle whose tester also came
    /// back clean, from `Testing`, does).
    #[test]
    fn reviewing_gates_testing_and_never_converges_directly() {
        let from = RunState::Reviewing;
        assert!(from.validate_transition(RunState::Testing).is_ok());
        assert!(from.validate_transition(RunState::CoderRunning).is_ok());
        assert!(from
            .validate_transition(RunState::MaxReviewCyclesExceeded)
            .is_ok());
        assert!(from.validate_transition(RunState::Failed).is_ok());
        assert!(from.validate_transition(RunState::Converged).is_err());
        assert!(from
            .validate_transition(RunState::MaxTestCyclesExceeded)
            .is_err());
    }

    #[test]
    fn testing_covers_all_convergence_outcomes() {
        let from = RunState::Testing;
        assert!(from.validate_transition(RunState::Converged).is_ok());
        assert!(from.validate_transition(RunState::CoderRunning).is_ok());
        assert!(from
            .validate_transition(RunState::MaxTestCyclesExceeded)
            .is_ok());
        assert!(from.validate_transition(RunState::Failed).is_ok());
        assert!(from.validate_transition(RunState::Done).is_err());
        assert!(from
            .validate_transition(RunState::MaxReviewCyclesExceeded)
            .is_err());
    }

    #[test]
    fn terminal_states_have_no_outgoing_transition() {
        assert!(RunState::Done
            .validate_transition(RunState::CoderRunning)
            .is_err());
        assert!(RunState::Failed
            .validate_transition(RunState::CoderRunning)
            .is_err());
    }

    #[test]
    fn converged_can_only_move_to_pushed() {
        assert!(RunState::Converged
            .validate_transition(RunState::Pushed)
            .is_ok());
        assert!(RunState::Converged
            .validate_transition(RunState::Done)
            .is_err());
        assert!(RunState::Converged
            .validate_transition(RunState::CoderRunning)
            .is_err());
    }

    #[test]
    fn pushed_can_only_move_to_awaiting_ci() {
        assert!(RunState::Pushed
            .validate_transition(RunState::AwaitingCi)
            .is_ok());
        assert!(RunState::Pushed
            .validate_transition(RunState::Done)
            .is_err());
    }

    #[test]
    fn awaiting_ci_covers_all_post_push_outcomes() {
        let from = RunState::AwaitingCi;
        assert!(from.validate_transition(RunState::Done).is_ok());
        assert!(from.validate_transition(RunState::CoderRunning).is_ok());
        assert!(from.validate_transition(RunState::Failed).is_ok());
        assert!(from.validate_transition(RunState::Converged).is_err());
    }

    #[test]
    fn max_review_cycles_exceeded_can_only_move_to_failed() {
        assert!(RunState::MaxReviewCyclesExceeded
            .validate_transition(RunState::Failed)
            .is_ok());
        assert!(RunState::MaxReviewCyclesExceeded
            .validate_transition(RunState::CoderRunning)
            .is_err());
    }

    #[test]
    fn max_test_cycles_exceeded_can_only_move_to_failed() {
        assert!(RunState::MaxTestCyclesExceeded
            .validate_transition(RunState::Failed)
            .is_ok());
        assert!(RunState::MaxTestCyclesExceeded
            .validate_transition(RunState::CoderRunning)
            .is_err());
    }

    #[test]
    fn intermediate_states_match_recovery_rule() {
        assert!(RunState::CoderRunning.is_intermediate());
        assert!(RunState::Reviewing.is_intermediate());
        assert!(RunState::Testing.is_intermediate());
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
            RunState::Reviewing,
            RunState::Testing,
            RunState::Converged,
            RunState::Pushed,
            RunState::AwaitingCi,
            RunState::Done,
            RunState::MaxReviewCyclesExceeded,
            RunState::MaxTestCyclesExceeded,
            RunState::Failed,
        ] {
            assert_eq!(RunState::parse(state.as_str()).unwrap(), state);
        }
    }

    #[test]
    fn unknown_state_string_is_a_typed_error_not_a_panic() {
        assert_eq!(
            RunState::parse("bogus"),
            Err(CoreError::UnknownState("bogus".to_string()))
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
