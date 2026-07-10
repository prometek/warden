//! CI Watcher (issue #5): polls an already-opened PR's lifecycle and CI
//! check status until a terminal outcome is reached (merged, closed,
//! checks-passed, checks-failed, or an inactivity timeout), and reports that
//! outcome for the orchestrator to act on.
//!
//! Security/scope boundary (issue #5 acceptance criterion, non-negotiable):
//! this module only ever *reads* PR/CI status through [`CiProvider`]. There
//! is no merge capability anywhere in `warden-gated`, and none must ever be
//! added to this module -- "aucun merge automatique n'est déclenché par
//! Warden". Once CI is green ([`WatchOutcome::ChecksPassed`]), the decision
//! to actually merge the PR is left entirely to a human via the PR
//! provider's own UI; Warden's own responsibility for the run ends there.
//!
//! This module never talks to a PR provider's API/CLI directly -- that's
//! [`CiProvider`]'s job, implemented by `gh_provider::GhProvider` (GitHub)
//! today, mirroring how `pr_manager::PrProvider` seams off provider access
//! for the PR lifecycle actions.
//!
//! `decide_next_state_after_ci` in `warden-core` is the pure counterpart
//! that turns a [`WatchOutcome`]'s *coarse* shape into the next `RunState` --
//! deliberately not implemented here, since `warden-gated` only reports
//! (issue #5: "gated only reports, the orchestrator decides").

use std::time::Duration;

use tokio::time::{sleep, Instant};
use warden_core::{Finding, FindingSource, Severity};

use crate::error::Result;
use crate::pr_manager::PrHandle;

// ---------------------------------------------------------------------------
// Domain types (pure)
// ---------------------------------------------------------------------------

/// Coarse GitHub PR lifecycle, independent of CI/check status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrLifecycle {
    Open,
    Merged,
    Closed,
}

/// One check's outcome, already reconciled across GitHub's two overlapping
/// check-reporting APIs (the newer Checks API and the legacy commit
/// Statuses API) -- `gh pr view --json statusCheckRollup` returns entries in
/// either shape depending on which API a given CI integration uses, and
/// [`CiProvider`] implementations are expected to normalize both into this
/// one set before returning a [`PrStatus`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CheckConclusion {
    Pending,
    Passed,
    Failed,
}

/// One CI check's name, outcome, and (if any) a link to its details --
/// enough to describe a failure to a human or fold into a [`Finding`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckRun {
    pub name: String,
    pub conclusion: CheckConclusion,
    pub details_url: Option<String>,
}

/// A PR's full polled status: its lifecycle plus every CI check currently
/// reported against its head commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrStatus {
    pub lifecycle: PrLifecycle,
    pub checks: Vec<CheckRun>,
}

/// The net CI signal `PrStatus::checks_rollup` reduces `checks` to --
/// `decide_step` only needs this coarser view, not every individual check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChecksRollup {
    /// No CI has reported anything against this PR yet -- either it hasn't
    /// triggered, or it never will (the case the inactivity timeout guards
    /// against).
    NoChecksYet,
    /// At least one check has reported, none have failed yet, but at least
    /// one is still running.
    Pending,
    /// Every reported check passed (a `Skipped`/`Neutral` conclusion counts
    /// as passed, not blocking -- it never ran by design, not because it
    /// failed).
    AllPassed,
    /// At least one check failed. Carries the failing checks themselves so
    /// the caller can describe exactly what broke.
    SomeFailed(Vec<CheckRun>),
}

impl PrStatus {
    /// Reduces `checks` to the coarse [`ChecksRollup`] `decide_step` acts on.
    /// A failed check always wins over a merely-pending one -- there's no
    /// reason to keep waiting on other checks once at least one has already
    /// failed.
    pub fn checks_rollup(&self) -> ChecksRollup {
        let failed: Vec<CheckRun> = self
            .checks
            .iter()
            .filter(|check| check.conclusion == CheckConclusion::Failed)
            .cloned()
            .collect();
        if !failed.is_empty() {
            return ChecksRollup::SomeFailed(failed);
        }
        if self.checks.is_empty() {
            return ChecksRollup::NoChecksYet;
        }
        if self
            .checks
            .iter()
            .any(|check| check.conclusion == CheckConclusion::Pending)
        {
            return ChecksRollup::Pending;
        }
        ChecksRollup::AllPassed
    }
}

// ---------------------------------------------------------------------------
// Provider seam
// ---------------------------------------------------------------------------

/// Thin seam over a PR provider's CI/PR status, mirroring
/// `pr_manager::PrProvider`'s split between pure orchestration (this module)
/// and provider-specific I/O (`gh_provider::GhProvider`). Read-only by
/// construction -- see this module's top-level doc comment.
///
/// `async fn` in this trait is intentional for the same reason as
/// `PrProvider`'s: every call site awaits it directly on its own task
/// rather than boxing it into a `dyn` trait object.
#[allow(async_fn_in_trait)]
pub trait CiProvider {
    /// Fetches `pr`'s current lifecycle and CI check statuses.
    async fn pr_status(&self, pr: &PrHandle) -> Result<PrStatus>;
}

// ---------------------------------------------------------------------------
// Watch outcome & configuration
// ---------------------------------------------------------------------------

/// The terminal result of one [`watch_pr`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchOutcome {
    Merged,
    /// Closed without merging.
    Closed,
    ChecksPassed,
    /// Carries one blocking [`Finding`] per failed check
    /// (`FindingSource::Ci`), formatted the same way reviewer/tester
    /// findings are, so the orchestrator can treat them uniformly.
    ChecksFailed(Vec<Finding>),
    /// The polled status went unchanged for at least `inactivity_timeout` --
    /// the only thing that bounds this loop when CI never triggers at all.
    TimedOut,
}

/// Configuration for one [`watch_pr`] invocation. Both durations are
/// explicit, caller-supplied inputs, never hardcoded constants (issue #5:
/// "timeout d'inactivité configurable").
#[derive(Debug, Clone, Copy)]
pub struct WatchConfig {
    /// How long to sleep between two polls. `watch_pr` never busy-spins: it
    /// always awaits `tokio::time::sleep(poll_interval)` between iterations.
    pub poll_interval: Duration,
    /// How long the polled status may go completely unchanged before
    /// `watch_pr` gives up and returns [`WatchOutcome::TimedOut`] (issue #5
    /// acceptance criterion: "le timeout d'inactivité interrompt proprement
    /// la surveillance si la CI ne se déclenche jamais").
    pub inactivity_timeout: Duration,
}

// ---------------------------------------------------------------------------
// Pure decision logic
// ---------------------------------------------------------------------------

/// A comparable snapshot of a [`PrStatus`], used only to detect whether
/// anything changed between two polls (the inactivity clock). Checks are
/// sorted by name so GitHub returning the same checks in a different order
/// between polls is never mistaken for a status change.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StatusSnapshot {
    lifecycle: PrLifecycle,
    checks: Vec<(String, CheckConclusion)>,
}

impl StatusSnapshot {
    fn from_status(status: &PrStatus) -> Self {
        let mut checks: Vec<(String, CheckConclusion)> = status
            .checks
            .iter()
            .map(|check| (check.name.clone(), check.conclusion))
            .collect();
        checks.sort();
        Self {
            lifecycle: status.lifecycle,
            checks,
        }
    }
}

/// One [`watch_pr`] iteration's verdict: either the watch is over
/// ([`Self::Terminal`]), or the caller should sleep and poll again.
enum WatchStep {
    Terminal(WatchOutcome),
    KeepWaiting,
}

/// Decides one polling step purely from the latest status plus how long
/// nothing has changed -- no clock/IO of its own; [`watch_pr`] is the only
/// caller, and it supplies real elapsed time.
fn decide_step(
    status: &PrStatus,
    idle_elapsed: Duration,
    inactivity_timeout: Duration,
) -> WatchStep {
    match status.lifecycle {
        PrLifecycle::Merged => return WatchStep::Terminal(WatchOutcome::Merged),
        PrLifecycle::Closed => return WatchStep::Terminal(WatchOutcome::Closed),
        PrLifecycle::Open => {}
    }

    match status.checks_rollup() {
        ChecksRollup::SomeFailed(failed_checks) => WatchStep::Terminal(WatchOutcome::ChecksFailed(
            failed_checks_to_findings(&failed_checks),
        )),
        ChecksRollup::AllPassed => WatchStep::Terminal(WatchOutcome::ChecksPassed),
        ChecksRollup::NoChecksYet | ChecksRollup::Pending => {
            if idle_elapsed >= inactivity_timeout {
                WatchStep::Terminal(WatchOutcome::TimedOut)
            } else {
                WatchStep::KeepWaiting
            }
        }
    }
}

/// Turns failed CI checks into blocking findings, the same shape reviewer/
/// tester findings already take (`warden_core::Finding`) so the orchestrator
/// can treat a CI failure uniformly with any other blocking finding.
fn failed_checks_to_findings(failed_checks: &[CheckRun]) -> Vec<Finding> {
    failed_checks
        .iter()
        .map(|check| Finding {
            source: FindingSource::Ci,
            severity: Severity::Blocking,
            file: None,
            description: match &check.details_url {
                Some(url) => format!("CI check {:?} failed ({url})", check.name),
                None => format!("CI check {:?} failed", check.name),
            },
            action: None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Watch loop (I/O)
// ---------------------------------------------------------------------------

/// Polls `pr`'s status via `provider` until a terminal [`WatchOutcome`] is
/// reached, sleeping `config.poll_interval` between polls -- never
/// busy-spinning. The inactivity clock resets on every poll whose status
/// differs from the previous one, so a CI run that's genuinely progressing
/// (new checks appearing, a pending check finishing) is never cut off just
/// because it takes a while; only a status that's been stuck unchanged for
/// `config.inactivity_timeout` ends the watch as [`WatchOutcome::TimedOut`].
///
/// Never merges the PR -- see this module's top-level doc comment.
pub async fn watch_pr<P: CiProvider>(
    pr: &PrHandle,
    provider: &P,
    config: &WatchConfig,
) -> Result<WatchOutcome> {
    let mut last_snapshot: Option<StatusSnapshot> = None;
    let mut last_change_at = Instant::now();

    loop {
        let status = provider.pr_status(pr).await?;
        let snapshot = StatusSnapshot::from_status(&status);
        let now = Instant::now();
        if last_snapshot.as_ref() != Some(&snapshot) {
            last_change_at = now;
            last_snapshot = Some(snapshot);
        }
        let idle_elapsed = now.saturating_duration_since(last_change_at);

        match decide_step(&status, idle_elapsed, config.inactivity_timeout) {
            WatchStep::Terminal(outcome) => return Ok(outcome),
            WatchStep::KeepWaiting => sleep(config.poll_interval).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn passed(name: &str) -> CheckRun {
        CheckRun {
            name: name.to_string(),
            conclusion: CheckConclusion::Passed,
            details_url: None,
        }
    }

    fn pending(name: &str) -> CheckRun {
        CheckRun {
            name: name.to_string(),
            conclusion: CheckConclusion::Pending,
            details_url: None,
        }
    }

    fn failed(name: &str) -> CheckRun {
        CheckRun {
            name: name.to_string(),
            conclusion: CheckConclusion::Failed,
            details_url: Some(format!("https://example.invalid/{name}")),
        }
    }

    fn open_status(checks: Vec<CheckRun>) -> PrStatus {
        PrStatus {
            lifecycle: PrLifecycle::Open,
            checks,
        }
    }

    // ---- PrStatus::checks_rollup -------------------------------------------

    #[test]
    fn no_checks_rolls_up_to_no_checks_yet() {
        assert_eq!(
            open_status(vec![]).checks_rollup(),
            ChecksRollup::NoChecksYet
        );
    }

    #[test]
    fn all_passed_checks_roll_up_to_all_passed() {
        let status = open_status(vec![passed("build"), passed("lint")]);
        assert_eq!(status.checks_rollup(), ChecksRollup::AllPassed);
    }

    #[test]
    fn one_pending_check_among_passed_ones_rolls_up_to_pending() {
        let status = open_status(vec![passed("build"), pending("integration")]);
        assert_eq!(status.checks_rollup(), ChecksRollup::Pending);
    }

    #[test]
    fn any_failed_check_wins_over_pending_ones() {
        let status = open_status(vec![pending("integration"), failed("build")]);
        assert_eq!(
            status.checks_rollup(),
            ChecksRollup::SomeFailed(vec![failed("build")])
        );
    }

    // ---- decide_step --------------------------------------------------------

    const SHORT: Duration = Duration::from_secs(1);
    const LONG: Duration = Duration::from_secs(3600);

    #[test]
    fn merged_is_terminal_regardless_of_checks() {
        let status = PrStatus {
            lifecycle: PrLifecycle::Merged,
            checks: vec![failed("build")],
        };
        assert!(matches!(
            decide_step(&status, Duration::ZERO, LONG),
            WatchStep::Terminal(WatchOutcome::Merged)
        ));
    }

    #[test]
    fn closed_is_terminal_regardless_of_checks() {
        let status = PrStatus {
            lifecycle: PrLifecycle::Closed,
            checks: vec![passed("build")],
        };
        assert!(matches!(
            decide_step(&status, Duration::ZERO, LONG),
            WatchStep::Terminal(WatchOutcome::Closed)
        ));
    }

    #[test]
    fn all_passed_checks_are_terminal_as_checks_passed() {
        let status = open_status(vec![passed("build")]);
        assert!(matches!(
            decide_step(&status, Duration::ZERO, LONG),
            WatchStep::Terminal(WatchOutcome::ChecksPassed)
        ));
    }

    #[test]
    fn failed_checks_are_terminal_and_carry_one_finding_per_failure() {
        let status = open_status(vec![failed("build"), passed("lint")]);
        match decide_step(&status, Duration::ZERO, LONG) {
            WatchStep::Terminal(WatchOutcome::ChecksFailed(findings)) => {
                assert_eq!(findings.len(), 1);
                assert_eq!(findings[0].source, FindingSource::Ci);
                assert_eq!(findings[0].severity, Severity::Blocking);
                assert!(findings[0].description.contains("build"));
                assert!(findings[0].description.contains("example.invalid"));
            }
            _ => panic!("expected a terminal ChecksFailed step"),
        }
    }

    #[test]
    fn no_checks_yet_keeps_waiting_while_under_the_inactivity_timeout() {
        let status = open_status(vec![]);
        assert!(matches!(
            decide_step(&status, SHORT, LONG),
            WatchStep::KeepWaiting
        ));
    }

    #[test]
    fn no_checks_yet_times_out_once_idle_elapsed_reaches_the_timeout() {
        let status = open_status(vec![]);
        assert!(matches!(
            decide_step(&status, LONG, LONG),
            WatchStep::Terminal(WatchOutcome::TimedOut)
        ));
    }

    #[test]
    fn a_still_pending_check_also_times_out_once_stuck_long_enough() {
        let status = open_status(vec![pending("integration")]);
        assert!(matches!(
            decide_step(&status, LONG, LONG),
            WatchStep::Terminal(WatchOutcome::TimedOut)
        ));
    }

    // ---- watch_pr (I/O loop, fake in-memory provider) -----------------------

    /// A [`CiProvider`] that returns a fixed sequence of statuses, one per
    /// call, standing in for real `gh` polling so `watch_pr`'s loop
    /// (sleep-between-polls, inactivity clock, terminal detection) is
    /// exercised without any network/subprocess I/O.
    struct ScriptedProvider {
        responses: std::sync::Mutex<std::collections::VecDeque<PrStatus>>,
    }

    impl ScriptedProvider {
        fn new(responses: Vec<PrStatus>) -> Self {
            Self {
                responses: std::sync::Mutex::new(responses.into()),
            }
        }
    }

    impl CiProvider for ScriptedProvider {
        async fn pr_status(&self, _pr: &PrHandle) -> Result<PrStatus> {
            let mut responses = self.responses.lock().unwrap();
            Ok(responses
                .pop_front()
                .expect("ScriptedProvider ran out of scripted responses"))
        }
    }

    #[tokio::test]
    async fn watch_pr_returns_checks_passed_once_all_checks_succeed() {
        let provider = ScriptedProvider::new(vec![
            open_status(vec![pending("build")]),
            open_status(vec![passed("build")]),
        ]);
        let config = WatchConfig {
            poll_interval: Duration::from_millis(1),
            inactivity_timeout: LONG,
        };

        let outcome = watch_pr(&PrHandle { number: 1 }, &provider, &config)
            .await
            .unwrap();
        assert_eq!(outcome, WatchOutcome::ChecksPassed);
    }

    #[tokio::test]
    async fn watch_pr_times_out_when_nothing_ever_changes() {
        let provider = ScriptedProvider::new(vec![
            open_status(vec![]),
            open_status(vec![]),
            open_status(vec![]),
        ]);
        let config = WatchConfig {
            poll_interval: Duration::from_millis(1),
            inactivity_timeout: Duration::from_millis(2),
        };

        let outcome = watch_pr(&PrHandle { number: 1 }, &provider, &config)
            .await
            .unwrap();
        assert_eq!(outcome, WatchOutcome::TimedOut);
    }
}
