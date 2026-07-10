//! Pure re-verification logic (ADR-0002/ADR-0006): no I/O. Given the run's
//! *actually persisted* state and the commit that was *actually* pushed
//! into the local bare gate repo, decides whether that push may be relayed
//! to `origin`. This is the single place that answers "is this safe to
//! forward" -- independent of anything `warden` (or a compromised copy of
//! it) claims about itself.
//!
//! Kept free of I/O specifically so the decision rule itself is testable
//! without a database, socket, or subprocess (code-standards.md: "séparer
//! strictement I/O ... et logique pure").

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
    /// No `runs` row exists for this id in the (real, independently read)
    /// database -- the notification refers to a run the gate has never
    /// heard of.
    RunNotFound { run_id: String },
    /// The run's persisted state is not `Converged`. This is the core
    /// authorization rule (issue #3, acceptance criterion 1): blocks
    /// regardless of what the notification -- or `warden` -- claims.
    NotConverged { actual_state: RunState },
    /// The run is `Converged`, but the commit actually pushed into the bare
    /// gate repo does not match `runs.converged_commit_sha`.
    HashMismatch {
        validated: Option<String>,
        pushed: String,
    },
}

/// The single authorization rule: only a run whose *persisted* state is
/// `RunState::Converged`, and whose *persisted* `converged_commit_sha`
/// matches the commit that was actually pushed, may pass. `run` must come
/// from an independent re-read of SQLite (see `db::get_run_view` /
/// `gate::verify_and_authorize`) -- this function itself takes no shortcut
/// and trusts nothing about the caller's own beliefs.
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

    /// Acceptance criterion 1 (issue #3): a push is blocked if the run's
    /// *real, persisted* state isn't `Converged` -- even if the caller (a
    /// stand-in for "what `warden` believes/claims") supplies a commit sha
    /// as though the run had already converged on it.
    #[test]
    fn blocks_when_persisted_state_is_not_converged_even_if_the_notification_claims_success() {
        let run = GateRunView {
            state: RunState::CoderRunning,
            converged_commit_sha: None,
        };

        // A compromised/buggy `warden` might report any commit here --
        // it's irrelevant, because the decision never looks at it before
        // checking the real state.
        let decision = decide("run-1", Some(&run), "whatever-warden-claims");

        assert_eq!(
            decision,
            GateDecision::Blocked(GateBlockReason::NotConverged {
                actual_state: RunState::CoderRunning
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
        // Defensive: `converged_commit_sha` should always be set alongside
        // the `Converged` transition (see `warden::orchestrator`), but the
        // gate must not treat a missing hash as an automatic pass.
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
