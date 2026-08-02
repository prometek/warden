//! Lifecycle-hook vocabulary: the *pure* half of the deterministic-action seam.

use std::path::Path;

use crate::convergence::Finding;
use crate::state::RunState;

/// A moment in a run's lifecycle at which hooks may fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookPoint {
    OnRunStart,
    BeforeStep,
    AfterStep,
    OnCommit,
    /// The run converged (entering [`RunState::Converged`]).
    OnConverged,
    /// Just before the converged commit is pushed (entering [`RunState::Pushed`]).
    BeforePush,
    /// The run is ending -- fires **once**, after the convergence loop exits, whatever its final
    /// state (converged, pushed, budget-exhausted, or failed).
    OnRunEnd,
}

impl HookPoint {
    /// Stable string form -- for events/logs/tests.
    pub fn as_str(self) -> &'static str {
        match self {
            HookPoint::OnRunStart => "on_run_start",
            HookPoint::BeforeStep => "before_step",
            HookPoint::AfterStep => "after_step",
            HookPoint::OnCommit => "on_commit",
            HookPoint::OnConverged => "on_converged",
            HookPoint::BeforePush => "before_push",
            HookPoint::OnRunEnd => "on_run_end",
        }
    }

    /// Parses the stable string form ([`HookPoint::as_str`]) back into a point.
    pub fn parse(s: &str) -> Option<HookPoint> {
        Some(match s {
            "on_run_start" => HookPoint::OnRunStart,
            "before_step" => HookPoint::BeforeStep,
            "after_step" => HookPoint::AfterStep,
            "on_commit" => HookPoint::OnCommit,
            "on_converged" => HookPoint::OnConverged,
            "before_push" => HookPoint::BeforePush,
            "on_run_end" => HookPoint::OnRunEnd,
            _ => return None,
        })
    }

    /// Every point, in declaration order -- for config validation error messages (listing the valid
    /// `point` names) and exhaustiveness tests.
    pub const ALL: [HookPoint; 7] = [
        HookPoint::OnRunStart,
        HookPoint::BeforeStep,
        HookPoint::AfterStep,
        HookPoint::OnCommit,
        HookPoint::OnConverged,
        HookPoint::BeforePush,
        HookPoint::OnRunEnd,
    ];

    /// The hook point that firing *on entering* `state` corresponds to, if any.
    pub fn on_entering(state: RunState) -> Option<HookPoint> {
        match state {
            RunState::RunningStep(_) => Some(HookPoint::BeforeStep),
            RunState::Converged => Some(HookPoint::OnConverged),
            RunState::Pushed => Some(HookPoint::BeforePush),
            RunState::Pending
            | RunState::AwaitingCi
            | RunState::AwaitingQuotaReset { .. }
            | RunState::ResumingQuota
            | RunState::Done
            | RunState::StepCyclesExceeded(_)
            | RunState::Failed => None,
        }
    }
}

/// What a hook is told about the run at the moment it fires.
#[derive(Debug, Clone, Copy)]
pub struct HookContext<'a> {
    /// Which point fired this hook.
    pub point: HookPoint,
    /// The run this hook fires within (`RUNS.id`).
    pub run_id: &'a str,
    /// The state the run is entering as this hook fires.
    pub state: RunState,
    /// The run's repository working directory -- the checkout the run was launched against
    /// (`RUNS.repo_path`).
    pub repo_path: &'a Path,
    /// The overall loop-iteration counter for the current cycle, when the firing point is inside a
    /// cycle (`None` for run-level points that carry no single cycle).
    pub cycle: Option<u32>,
    /// The role's worktree the action would run against, when one applies.
    pub worktree: Option<&'a Path>,
    /// The cycle's commit, when one has been produced by the firing point.
    pub commit: Option<&'a str>,
    /// The cycle's diff, when available at the firing point.
    pub diff: Option<&'a str>,
}

/// What a hook decides once it has run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookOutcome {
    /// Nothing to report -- the run proceeds unchanged.
    Continue,
    /// The hook refuses to let the run proceed past this point, with a human-readable reason.
    Block {
        reason: String,
    },
    EmitFindings(Vec<Finding>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn on_entering_maps_the_lifecycle_milestone_states() {
        assert_eq!(
            HookPoint::on_entering(RunState::RunningStep(0)),
            Some(HookPoint::BeforeStep)
        );
        assert_eq!(
            HookPoint::on_entering(RunState::Converged),
            Some(HookPoint::OnConverged)
        );
        assert_eq!(
            HookPoint::on_entering(RunState::Pushed),
            Some(HookPoint::BeforePush)
        );
    }

    #[test]
    fn on_entering_has_no_hook_for_non_milestone_states() {
        for state in [
            RunState::Pending,
            RunState::AwaitingCi,
            RunState::Done,
            RunState::StepCyclesExceeded(1),
            RunState::Failed,
        ] {
            assert_eq!(HookPoint::on_entering(state), None);
        }
    }

    #[test]
    fn hook_point_strings_are_unique_and_stable() {
        let mut seen = std::collections::HashSet::new();
        for point in HookPoint::ALL {
            assert!(seen.insert(point.as_str()), "duplicate: {}", point.as_str());
        }
        assert_eq!(seen.len(), HookPoint::ALL.len());
    }

    #[test]
    fn as_str_and_parse_round_trip_for_every_point() {
        for point in HookPoint::ALL {
            assert_eq!(
                HookPoint::parse(point.as_str()),
                Some(point),
                "{} must parse back to itself",
                point.as_str()
            );
        }
        assert_eq!(HookPoint::parse("not_a_point"), None);
    }
}
