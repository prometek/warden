use crate::error::{CoreError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RunState {
    Pending,
    RunningStep(u32),
    Converged,
    Pushed,
    AwaitingCi,
    AwaitingQuotaReset { resets_at: i64 },
    ResumingQuota,
    Done,
    StepCyclesExceeded(u32),
    Failed,
}

impl RunState {
    pub fn as_str(&self) -> String {
        match self {
            Self::Pending => "pending".to_string(),
            Self::RunningStep(index) => format!("running_step:{index}"),
            Self::Converged => "converged".to_string(),
            Self::Pushed => "pushed".to_string(),
            Self::AwaitingCi => "awaiting_ci".to_string(),
            Self::AwaitingQuotaReset { resets_at } => {
                format!("awaiting_quota_reset:{resets_at}")
            }
            Self::ResumingQuota => "resuming_quota".to_string(),
            Self::Done => "done".to_string(),
            Self::StepCyclesExceeded(index) => format!("step_cycles_exceeded:{index}"),
            Self::Failed => "failed".to_string(),
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        if let Some(index) = raw.strip_prefix("running_step:") {
            return parse_step_index(raw, index).map(Self::RunningStep);
        }
        if let Some(index) = raw.strip_prefix("step_cycles_exceeded:") {
            return parse_step_index(raw, index).map(Self::StepCyclesExceeded);
        }
        if let Some(resets_at) = raw.strip_prefix("awaiting_quota_reset:") {
            return resets_at
                .parse::<i64>()
                .map(|resets_at| Self::AwaitingQuotaReset { resets_at })
                .map_err(|_| CoreError::UnknownState(raw.to_string()));
        }
        match raw {
            "pending" => Ok(Self::Pending),
            // Compatibility for runs persisted before graph workflows.
            "coder_running" => Ok(Self::RunningStep(0)),
            "converged" => Ok(Self::Converged),
            "pushed" => Ok(Self::Pushed),
            "awaiting_ci" => Ok(Self::AwaitingCi),
            "resuming_quota" => Ok(Self::ResumingQuota),
            "done" => Ok(Self::Done),
            "failed" => Ok(Self::Failed),
            other => Err(CoreError::UnknownState(other.to_string())),
        }
    }

    pub fn is_intermediate(self) -> bool {
        matches!(
            self,
            Self::RunningStep(_) | Self::AwaitingCi | Self::ResumingQuota
        )
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed)
    }

    pub fn validate_transition(self, to: Self, total_steps: u32) -> Result<()> {
        let valid_step = |state| matches!(state, Self::RunningStep(index) if index < total_steps);
        let allowed = match self {
            Self::Pending => valid_step(to) || to == Self::Failed,
            Self::RunningStep(index) if index < total_steps => {
                valid_step(to)
                    || matches!(
                        to,
                        Self::Converged
                            | Self::Failed
                            | Self::AwaitingQuotaReset { .. }
                            | Self::StepCyclesExceeded(_)
                    ) && !matches!(to, Self::StepCyclesExceeded(other) if other != index)
            }
            Self::Converged => matches!(to, Self::Pushed | Self::Failed),
            // A `before_push` lifecycle hook fires *after* the write-ahead into `Pushed` (see
            // `Orchestrator::transition`); if it blocks, the run must still be able to reach
            // `Failed` from here.
            Self::Pushed => matches!(to, Self::AwaitingCi | Self::Failed),
            Self::AwaitingCi => matches!(to, Self::Done | Self::Failed) || valid_step(to),
            Self::AwaitingQuotaReset { .. } => to == Self::ResumingQuota,
            Self::ResumingQuota => {
                valid_step(to) || matches!(to, Self::AwaitingQuotaReset { .. } | Self::Failed)
            }
            Self::StepCyclesExceeded(_) => to == Self::Failed,
            Self::Done | Self::Failed | Self::RunningStep(_) => false,
        };
        if allowed {
            Ok(())
        } else {
            Err(CoreError::InvalidTransition { from: self, to })
        }
    }
}

fn parse_step_index(full_raw: &str, index: &str) -> Result<u32> {
    index
        .parse::<u32>()
        .map_err(|_| CoreError::UnknownState(full_raw.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_steps_may_transition_to_any_declared_step_or_terminal() {
        let from = RunState::RunningStep(2);
        for to in [
            RunState::RunningStep(0),
            RunState::RunningStep(3),
            RunState::Converged,
            RunState::Failed,
            RunState::AwaitingQuotaReset { resets_at: 42 },
            RunState::StepCyclesExceeded(2),
        ] {
            assert!(from.validate_transition(to, 4).is_ok(), "{to:?}");
        }
        assert!(from
            .validate_transition(RunState::RunningStep(4), 4)
            .is_err());
        assert!(from
            .validate_transition(RunState::StepCyclesExceeded(1), 4)
            .is_err());
    }

    #[test]
    fn pending_and_ci_resume_at_any_declared_entry() {
        assert!(RunState::Pending
            .validate_transition(RunState::RunningStep(2), 3)
            .is_ok());
        assert!(RunState::AwaitingCi
            .validate_transition(RunState::RunningStep(2), 3)
            .is_ok());
    }

    #[test]
    fn pushed_may_still_fail_a_before_push_hook_block() {
        assert!(RunState::Pushed
            .validate_transition(RunState::Failed, 1)
            .is_ok());
        assert!(RunState::Pushed
            .validate_transition(RunState::AwaitingCi, 1)
            .is_ok());
        assert!(RunState::Pushed
            .validate_transition(RunState::RunningStep(0), 1)
            .is_err());
    }

    /// Widening `Pushed` (for a refused `before_push` hook) must widen it by exactly one edge and
    /// no more: `Failed` is now reachable, every state other than `AwaitingCi` still is not.
    #[test]
    fn pushed_gains_exactly_one_new_successor_and_no_other() {
        let allowed = [RunState::AwaitingCi, RunState::Failed];
        for to in [
            RunState::Pending,
            RunState::RunningStep(0),
            RunState::RunningStep(1),
            RunState::Converged,
            RunState::Pushed,
            RunState::AwaitingCi,
            RunState::AwaitingQuotaReset { resets_at: 42 },
            RunState::ResumingQuota,
            RunState::Done,
            RunState::StepCyclesExceeded(0),
            RunState::Failed,
        ] {
            assert_eq!(
                RunState::Pushed.validate_transition(to, 2).is_ok(),
                allowed.contains(&to),
                "Pushed -> {to:?}"
            );
        }
    }

    /// `Pushed` is deliberately *not* intermediate, so `db::list_intermediate_runs` (and therefore
    /// crash recovery, which forces every run it returns to `Failed`) can never reach the newly
    /// allowed `Pushed -> Failed` edge behind the orchestrator's back.
    #[test]
    fn pushed_is_not_an_intermediate_state_crash_recovery_would_fail() {
        assert!(!RunState::Pushed.is_intermediate());
    }

    #[test]
    fn legacy_coder_state_reads_as_first_graph_step() {
        assert_eq!(
            RunState::parse("coder_running").unwrap(),
            RunState::RunningStep(0)
        );
    }

    #[test]
    fn states_round_trip() {
        for state in [
            RunState::Pending,
            RunState::RunningStep(3),
            RunState::Converged,
            RunState::Pushed,
            RunState::AwaitingCi,
            RunState::AwaitingQuotaReset { resets_at: 42 },
            RunState::ResumingQuota,
            RunState::Done,
            RunState::StepCyclesExceeded(3),
            RunState::Failed,
        ] {
            assert_eq!(RunState::parse(&state.as_str()).unwrap(), state);
        }
    }

    #[test]
    fn terminal_states_have_no_successor() {
        assert!(RunState::Done
            .validate_transition(RunState::Failed, 1)
            .is_err());
        assert!(RunState::Failed
            .validate_transition(RunState::RunningStep(0), 1)
            .is_err());
    }

    #[test]
    fn invalid_state_is_typed_error() {
        assert_eq!(
            RunState::parse("running_step:ghost"),
            Err(CoreError::UnknownState("running_step:ghost".to_string()))
        );
    }
}
