//! The run state machine (see Architecture.md §6, `RUNS.state`).

use crate::error::{CoreError, Result};

/// Lifecycle state of a run, mirroring the state diagram in Architecture.md §6.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RunState {
    Pending,
    CoderRunning,
    /// A gated step is running or awaiting its own findings.
    RunningStep(u32),
    Converged,
    Pushed,
    AwaitingCi,
    /// Quota was exhausted or reached the configured anticipation threshold.
    AwaitingQuotaReset {
        resets_at: i64,
    },
    /// A startup process atomically claimed an elapsed quota continuation and is reconstructing or
    /// driving it.
    ResumingQuota,
    Done,
    StepCyclesExceeded(u32),
    Failed,
}

impl RunState {
    /// Stable string form used as the `RUNS.state` column value.
    pub fn as_str(&self) -> String {
        match self {
            RunState::Pending => "pending".to_string(),
            RunState::CoderRunning => "coder_running".to_string(),
            RunState::RunningStep(index) => format!("running_step:{index}"),
            RunState::Converged => "converged".to_string(),
            RunState::Pushed => "pushed".to_string(),
            RunState::AwaitingCi => "awaiting_ci".to_string(),
            RunState::AwaitingQuotaReset { resets_at } => {
                format!("awaiting_quota_reset:{resets_at}")
            }
            RunState::ResumingQuota => "resuming_quota".to_string(),
            RunState::Done => "done".to_string(),
            RunState::StepCyclesExceeded(index) => format!("step_cycles_exceeded:{index}"),
            RunState::Failed => "failed".to_string(),
        }
    }

    /// Parses a `RUNS.state` column value back into a `RunState`.
    pub fn parse(raw: &str) -> Result<Self> {
        if let Some(index) = raw.strip_prefix("running_step:") {
            return parse_step_index(raw, index).map(RunState::RunningStep);
        }
        if let Some(index) = raw.strip_prefix("step_cycles_exceeded:") {
            return parse_step_index(raw, index).map(RunState::StepCyclesExceeded);
        }
        if let Some(resets_at) = raw.strip_prefix("awaiting_quota_reset:") {
            return resets_at
                .parse::<i64>()
                .map(|resets_at| RunState::AwaitingQuotaReset { resets_at })
                .map_err(|_| CoreError::UnknownState(raw.to_string()));
        }
        match raw {
            "pending" => Ok(RunState::Pending),
            "coder_running" => Ok(RunState::CoderRunning),
            "converged" => Ok(RunState::Converged),
            "pushed" => Ok(RunState::Pushed),
            "awaiting_ci" => Ok(RunState::AwaitingCi),
            "resuming_quota" => Ok(RunState::ResumingQuota),
            "done" => Ok(RunState::Done),
            "failed" => Ok(RunState::Failed),
            other => Err(CoreError::UnknownState(other.to_string())),
        }
    }

    /// States considered "mid-cycle": a run left in one of these states across an orchestrator
    /// restart is only legitimate if a live agent process is still associated with it.
    pub fn is_intermediate(self) -> bool {
        matches!(
            self,
            RunState::CoderRunning
                | RunState::RunningStep(_)
                | RunState::AwaitingCi
                | RunState::ResumingQuota
        )
    }

    /// Terminal states: no further transition is legal once reached.
    pub fn is_terminal(self) -> bool {
        matches!(self, RunState::Done | RunState::Failed)
    }

    /// The states legal as this state's successor, given `total_steps` (`workflow.steps.len()`).
    fn allowed_next_states(self, total_steps: u32) -> Vec<RunState> {
        match self {
            RunState::Pending => vec![RunState::CoderRunning, RunState::Failed],
            RunState::CoderRunning => {
                // A workflow whose only step is the producer itself (no gates at all) converges
                // directly when its cycle raised no blocking finding -- there is no later gated
                // step to gate on.
                if total_steps <= 1 {
                    vec![
                        RunState::Converged,
                        RunState::CoderRunning,
                        RunState::StepCyclesExceeded(0),
                        RunState::Failed,
                        RunState::AwaitingQuotaReset { resets_at: 0 },
                    ]
                } else {
                    vec![
                        RunState::RunningStep(1),
                        RunState::Failed,
                        RunState::AwaitingQuotaReset { resets_at: 0 },
                    ]
                }
            }
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
                    RunState::AwaitingQuotaReset { resets_at: 0 },
                ]
            }
            RunState::Converged => vec![RunState::Pushed, RunState::Failed],
            RunState::Pushed => vec![RunState::AwaitingCi],
            RunState::AwaitingCi => {
                vec![RunState::Done, RunState::CoderRunning, RunState::Failed]
            }
            RunState::AwaitingQuotaReset { .. } => vec![RunState::ResumingQuota],
            // an atomic startup claim leaves `ResumingQuota` only for the exact checkpointed
            // boundary.
            RunState::ResumingQuota => vec![
                RunState::CoderRunning,
                RunState::RunningStep(1),
                RunState::AwaitingQuotaReset { resets_at: 0 },
                RunState::Failed,
            ],
            RunState::StepCyclesExceeded(_) => vec![RunState::Failed],
            RunState::Done => vec![],
            RunState::Failed => vec![],
        }
    }

    pub fn validate_transition(self, to: RunState, total_steps: u32) -> Result<()> {
        if self == RunState::ResumingQuota {
            let resumes_at_valid_gated_step =
                matches!(to, RunState::RunningStep(index) if index > 0 && index < total_steps);
            if resumes_at_valid_gated_step {
                return Ok(());
            }
        }
        if self.allowed_next_states(total_steps).iter().any(|allowed| {
            allowed == &to
                || matches!(
                    (allowed, &to),
                    (
                        RunState::AwaitingQuotaReset { .. },
                        RunState::AwaitingQuotaReset { .. }
                    )
                )
        }) {
            Ok(())
        } else {
            Err(CoreError::InvalidTransition { from: self, to })
        }
    }
}

/// Parses the numeric suffix of a `running_step:<n>`/`step_cycles_exceeded:<n>` state string.
fn parse_step_index(full_raw: &str, index: &str) -> Result<u32> {
    index
        .parse::<u32>()
        .map_err(|_| CoreError::UnknownState(full_raw.to_string()))
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

    const DEFAULT_TOTAL_STEPS: u32 = 3;

    #[test]
    fn pending_can_move_to_coder_running_or_fail() {
        assert!(RunState::Pending
            .validate_transition(RunState::CoderRunning, DEFAULT_TOTAL_STEPS)
            .is_ok());
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
    fn quota_suspension_is_a_persistable_non_intermediate_state() {
        let awaiting = RunState::AwaitingQuotaReset {
            resets_at: 1_785_686_400,
        };
        assert_eq!(RunState::parse(&awaiting.as_str()).unwrap(), awaiting);
        assert!(!awaiting.is_intermediate());
        assert!(RunState::CoderRunning
            .validate_transition(awaiting, DEFAULT_TOTAL_STEPS)
            .is_ok());
        assert!(awaiting
            .validate_transition(RunState::ResumingQuota, DEFAULT_TOTAL_STEPS)
            .is_ok());
        assert!(RunState::ResumingQuota.is_intermediate());
        assert!(RunState::ResumingQuota
            .validate_transition(RunState::Failed, DEFAULT_TOTAL_STEPS)
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
        assert!(RunState::CoderRunning
            .validate_transition(RunState::CoderRunning, DEFAULT_TOTAL_STEPS)
            .is_err());
        assert!(RunState::CoderRunning
            .validate_transition(RunState::StepCyclesExceeded(0), DEFAULT_TOTAL_STEPS)
            .is_err());
    }

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
    fn converged_can_move_to_pushed_or_fail_on_a_policy_denial() {
        assert!(RunState::Converged
            .validate_transition(RunState::Pushed, DEFAULT_TOTAL_STEPS)
            .is_ok());
        assert!(RunState::Converged
            .validate_transition(RunState::Failed, DEFAULT_TOTAL_STEPS)
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
        assert!(RunState::ResumingQuota.is_intermediate());
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
            RunState::AwaitingQuotaReset {
                resets_at: 1_785_686_400,
            },
            RunState::ResumingQuota,
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
