//! Pure re-verification logic: no I/O.

use warden_core::RunState;

use crate::db::GateRunView;

/// The gate's answer for a single push attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateDecision {
    /// Safe to relay `commit_sha` to `origin`.
    Allow { commit_sha: String },
    /// Not safe to relay -- see [`GateBlockReason`] for why.
    Blocked(GateBlockReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateBlockReason {
    /// No `runs` row exists for this id in the (real, independently read) database -- the
    /// notification refers to a run the gate has never heard of.
    RunNotFound { run_id: String },
    /// The run's persisted state is not `Converged`.
    NotConverged { actual_state: RunState },
    /// The run is `Converged`, but the commit actually pushed into the bare gate repo does not
    /// match `runs.converged_commit_sha`.
    HashMismatch {
        validated: Option<String>,
        pushed: String,
    },
}

pub fn decide(run_id: &str, run: Option<&GateRunView>, pushed_commit_sha: &str) -> GateDecision {
    let Some(run) = run else {
        return GateDecision::Blocked(GateBlockReason::RunNotFound {
            run_id: run_id.to_string(),
        });
    };

    if run.state != RunState::Converged {
        return GateDecision::Blocked(GateBlockReason::NotConverged {
            actual_state: run.state,
        });
    }

    match &run.converged_commit_sha {
        Some(validated) if validated == pushed_commit_sha => GateDecision::Allow {
            commit_sha: pushed_commit_sha.to_string(),
        },
        validated => GateDecision::Blocked(GateBlockReason::HashMismatch {
            validated: validated.clone(),
            pushed: pushed_commit_sha.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn converged_run(commit_sha: &str) -> GateRunView {
        GateRunView {
            state: RunState::Converged,
            converged_commit_sha: Some(commit_sha.to_string()),
        }
    }

    #[test]
    fn allows_a_converged_run_whose_pushed_commit_matches_the_validated_hash() {
        let run = converged_run("abc123");
        let decision = decide("run-1", Some(&run), "abc123");
        assert_eq!(
            decision,
            GateDecision::Allow {
                commit_sha: "abc123".to_string()
            }
        );
    }

    #[test]
    fn blocks_when_persisted_state_is_not_converged_even_if_the_notification_claims_success() {
        let run = GateRunView {
            state: RunState::RunningStep(0),
            converged_commit_sha: None,
        };

        let decision = decide("run-1", Some(&run), "whatever-warden-claims");

        assert_eq!(
            decision,
            GateDecision::Blocked(GateBlockReason::NotConverged {
                actual_state: RunState::RunningStep(0)
            })
        );
    }

    #[test]
    fn blocks_a_converged_run_whose_pushed_commit_does_not_match_the_validated_hash() {
        let run = converged_run("validated-sha");
        let decision = decide("run-1", Some(&run), "different-sha");
        assert_eq!(
            decision,
            GateDecision::Blocked(GateBlockReason::HashMismatch {
                validated: Some("validated-sha".to_string()),
                pushed: "different-sha".to_string(),
            })
        );
    }

    #[test]
    fn blocks_a_converged_run_with_no_validated_hash_recorded_yet() {
        let run = GateRunView {
            state: RunState::Converged,
            converged_commit_sha: None,
        };
        let decision = decide("run-1", Some(&run), "some-sha");
        assert_eq!(
            decision,
            GateDecision::Blocked(GateBlockReason::HashMismatch {
                validated: None,
                pushed: "some-sha".to_string(),
            })
        );
    }

    #[test]
    fn blocks_a_run_id_with_no_matching_row_at_all() {
        let decision = decide("ghost-run", None, "some-sha");
        assert_eq!(
            decision,
            GateDecision::Blocked(GateBlockReason::RunNotFound {
                run_id: "ghost-run".to_string()
            })
        );
    }
}
