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
/// Phase 1 only exercises `Pending` through `Converged` / `MaxCyclesExceeded`
/// / `Failed`. `Pushed`, `AwaitingCi`, and `Done` are reserved for the git
/// gate (Phase 3) and CI watcher (Phase 5) but are modeled now so the state
/// machine doesn't need a breaking change later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RunState {
    Pending,
    CoderRunning,
    AwaitingReviewTest,
    Converged,
    Pushed,
    AwaitingCi,
    Done,
    MaxCyclesExceeded,
    Failed,
}

impl RunState {
    /// Stable string form used as the `RUNS.state` column value. Never
    /// change existing variants' strings without a migration.
    pub fn as_str(self) -> &'static str {
        match self {
            RunState::Pending => "pending",
            RunState::CoderRunning => "coder_running",
            RunState::AwaitingReviewTest => "awaiting_review_test",
            RunState::Converged => "converged",
            RunState::Pushed => "pushed",
            RunState::AwaitingCi => "awaiting_ci",
            RunState::Done => "done",
            RunState::MaxCyclesExceeded => "max_cycles_exceeded",
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
            "awaiting_review_test" => Ok(RunState::AwaitingReviewTest),
            "converged" => Ok(RunState::Converged),
            "pushed" => Ok(RunState::Pushed),
            "awaiting_ci" => Ok(RunState::AwaitingCi),
            "done" => Ok(RunState::Done),
            "max_cycles_exceeded" => Ok(RunState::MaxCyclesExceeded),
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
            RunState::CoderRunning | RunState::AwaitingReviewTest | RunState::AwaitingCi
        )
    }

    /// Terminal states: no further transition is legal once reached.
    pub fn is_terminal(self) -> bool {
        matches!(self, RunState::Done | RunState::Failed)
    }

    fn allowed_next_states(self) -> &'static [RunState] {
        match self {
            RunState::Pending => &[RunState::CoderRunning],
            RunState::CoderRunning => &[RunState::AwaitingReviewTest, RunState::Failed],
            RunState::AwaitingReviewTest => &[
                RunState::Converged,
                RunState::CoderRunning,
                RunState::MaxCyclesExceeded,
                RunState::Failed,
            ],
            RunState::Converged => &[RunState::Pushed],
            RunState::Pushed => &[RunState::AwaitingCi],
            RunState::AwaitingCi => &[RunState::Done, RunState::CoderRunning, RunState::Failed],
            RunState::MaxCyclesExceeded => &[RunState::Failed],
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
            .validate_transition(RunState::AwaitingReviewTest)
            .is_ok());
    }

    #[test]
    fn awaiting_review_test_covers_all_convergence_outcomes() {
        let from = RunState::AwaitingReviewTest;
        assert!(from.validate_transition(RunState::Converged).is_ok());
        assert!(from.validate_transition(RunState::CoderRunning).is_ok());
        assert!(from
            .validate_transition(RunState::MaxCyclesExceeded)
            .is_ok());
        assert!(from.validate_transition(RunState::Failed).is_ok());
        assert!(from.validate_transition(RunState::Done).is_err());
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
    fn intermediate_states_match_recovery_rule() {
        assert!(RunState::CoderRunning.is_intermediate());
        assert!(RunState::AwaitingReviewTest.is_intermediate());
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
            RunState::AwaitingReviewTest,
            RunState::Converged,
            RunState::Pushed,
            RunState::AwaitingCi,
            RunState::Done,
            RunState::MaxCyclesExceeded,
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
