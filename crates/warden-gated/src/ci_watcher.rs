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

use crate::error::{GatedError, Result};
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
///
/// **The inactivity timeout has no absolute wall-clock cap.** Its clock
/// resets on *any* observed status change (see [`watch_pr`]'s doc comment),
/// so a PR whose status keeps changing on every single poll (e.g. a
/// perpetually flapping check) never times out, no matter how long the
/// watch has been running in total -- this is by design (issue #5's own
/// framing is "si la CI ne se déclenche jamais", an idle condition, not a
/// total-duration budget), not an oversight.
#[derive(Debug, Clone, Copy)]
pub struct WatchConfig {
    /// How long to sleep between two polls. `watch_pr` never busy-spins: it
    /// always awaits `tokio::time::sleep(poll_interval)` between iterations.
    pub poll_interval: Duration,
    /// How long the polled status may go completely unchanged before
    /// `watch_pr` gives up and returns [`WatchOutcome::TimedOut`] (issue #5
    /// acceptance criterion: "le timeout d'inactivité interrompt proprement
    /// la surveillance si la CI ne se déclenche jamais"). See this struct's
    /// top-level doc comment for the idle-vs-absolute distinction.
    pub inactivity_timeout: Duration,
    /// How many *consecutive* transient poll failures (a `gh` command or
    /// I/O failure -- rate limit, network blip; never a malformed/
    /// unparseable response, which always aborts immediately regardless of
    /// this budget) `watch_pr` tolerates before giving up. Resets to zero on
    /// any successful poll. Issue #5 fix-cycle 1: the ticket's own
    /// flakiness motivation ("différences d'environnement, flakiness")
    /// applies just as much to polling CI status as to CI itself.
    pub max_consecutive_poll_errors: u32,
}

impl WatchConfig {
    /// A sensible default retry budget for transient poll failures: enough
    /// to ride out a single rate-limit/network blip without masking a truly
    /// broken `gh`/network setup forever.
    pub const DEFAULT_MAX_CONSECUTIVE_POLL_ERRORS: u32 = 3;
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

/// Whether `error` represents a transient failure to reach `gh`/the network
/// (worth retrying a poll, up to [`WatchConfig::max_consecutive_poll_errors`]
/// times in a row) rather than a malformed/unexpected response, which must
/// abort `watch_pr` immediately and loudly regardless of that budget --
/// code-standards.md: "ne jamais faire confiance à la sortie d'un agent
/// CLI" applies just as much to a provider's own malformed output as to an
/// agent's. `GhCommandFailed`/`Io` are the only variants `CiProvider::
/// pr_status` implementations return for "couldn't even talk to `gh`"; every
/// other `GatedError` variant a `pr_status` call could plausibly return
/// (`UnparsablePrStatusJson`, `UnknownPrLifecycle`, `UnknownCheckConclusion`,
/// `MalformedCheckEntry`) means `gh` *did* respond, just not with something
/// this module understands -- never worth retrying, since the same
/// malformed response would come back every time.
fn is_transient_poll_error(error: &GatedError) -> bool {
    matches!(
        error,
        GatedError::GhCommandFailed { .. } | GatedError::Io(_)
    )
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
/// `config.inactivity_timeout` ends the watch as [`WatchOutcome::TimedOut`]
/// (see [`WatchConfig`]'s doc comment for why that has no absolute
/// wall-clock cap).
///
/// A transient poll failure (`is_transient_poll_error`) is tolerated and
/// retried, up to `config.max_consecutive_poll_errors` times in a row --
/// reset to zero on the next successful poll. Exceeding that budget, or any
/// non-transient (malformed/unexpected) error, aborts the watch immediately
/// with that error; a malformed response is never silently retried or
/// swallowed.
///
/// Never merges the PR -- see this module's top-level doc comment.
pub async fn watch_pr<P: CiProvider>(
    pr: &PrHandle,
    provider: &P,
    config: &WatchConfig,
) -> Result<WatchOutcome> {
    tracing::info!(
        pr_number = pr.number,
        poll_interval = ?config.poll_interval,
        inactivity_timeout = ?config.inactivity_timeout,
        max_consecutive_poll_errors = config.max_consecutive_poll_errors,
        "watch_pr: starting to watch a PR for a terminal CI/lifecycle outcome"
    );

    let mut last_snapshot: Option<StatusSnapshot> = None;
    let mut last_change_at = Instant::now();
    let mut consecutive_poll_errors: u32 = 0;

    loop {
        let status = match provider.pr_status(pr).await {
            Ok(status) => {
                consecutive_poll_errors = 0;
                status
            }
            Err(error) if is_transient_poll_error(&error) => {
                consecutive_poll_errors += 1;
                if consecutive_poll_errors > config.max_consecutive_poll_errors {
                    tracing::error!(
                        pr_number = pr.number,
                        %error,
                        consecutive_poll_errors,
                        max_consecutive_poll_errors = config.max_consecutive_poll_errors,
                        "watch_pr: giving up after too many consecutive transient CI poll failures"
                    );
                    return Err(error);
                }
                tracing::warn!(
                    pr_number = pr.number,
                    %error,
                    consecutive_poll_errors,
                    max_consecutive_poll_errors = config.max_consecutive_poll_errors,
                    "watch_pr: tolerating a transient CI poll failure, will retry after the next sleep"
                );
                sleep(config.poll_interval).await;
                continue;
            }
            Err(error) => {
                tracing::error!(
                    pr_number = pr.number,
                    %error,
                    "watch_pr: aborting on a non-transient (malformed/unexpected) poll error"
                );
                return Err(error);
            }
        };

        tracing::debug!(
            pr_number = pr.number,
            lifecycle = ?status.lifecycle,
            check_count = status.checks.len(),
            "watch_pr: polled PR status"
        );

        let snapshot = StatusSnapshot::from_status(&status);
        let now = Instant::now();
        if last_snapshot.as_ref() != Some(&snapshot) {
            if last_snapshot.is_some() {
                tracing::debug!(
                    pr_number = pr.number,
                    "watch_pr: status changed since the last poll; resetting the inactivity clock"
                );
            }
            last_change_at = now;
            last_snapshot = Some(snapshot);
        }
        let idle_elapsed = now.saturating_duration_since(last_change_at);

        match decide_step(&status, idle_elapsed, config.inactivity_timeout) {
            WatchStep::Terminal(outcome) => {
                tracing::info!(
                    pr_number = pr.number,
                    outcome = ?outcome,
                    "watch_pr: reached a terminal outcome"
                );
                return Ok(outcome);
            }
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

    /// A sensible retry budget for the loop tests below that don't
    /// themselves exercise the transient-poll-error tolerance -- non-zero
    /// so a genuinely unrelated transient failure wouldn't make these tests
    /// flaky, but otherwise irrelevant to what each test is checking.
    const DEFAULT_ERROR_BUDGET: u32 = WatchConfig::DEFAULT_MAX_CONSECUTIVE_POLL_ERRORS;

    // Fix-cycle 1 (issue #5 review): these loop tests now run under tokio's
    // paused virtual clock (`start_paused = true`) instead of real
    // sub-millisecond sleeps -- code-standards.md "tests déterministes, pas
    // de temps réel non mocké". `sleep`/`Instant::now()` inside `watch_pr`
    // still behave correctly (auto-advance fires pending timers instantly),
    // so these run in effectively zero wall-clock time with no flakiness.

    #[tokio::test(start_paused = true)]
    async fn watch_pr_returns_checks_passed_once_all_checks_succeed() {
        let provider = ScriptedProvider::new(vec![
            open_status(vec![pending("build")]),
            open_status(vec![passed("build")]),
        ]);
        let config = WatchConfig {
            poll_interval: Duration::from_millis(1),
            inactivity_timeout: LONG,
            max_consecutive_poll_errors: DEFAULT_ERROR_BUDGET,
        };

        let outcome = watch_pr(&PrHandle { number: 1 }, &provider, &config)
            .await
            .unwrap();
        assert_eq!(outcome, WatchOutcome::ChecksPassed);
    }

    #[tokio::test(start_paused = true)]
    async fn watch_pr_times_out_when_nothing_ever_changes() {
        let provider = ScriptedProvider::new(vec![
            open_status(vec![]),
            open_status(vec![]),
            open_status(vec![]),
        ]);
        let config = WatchConfig {
            poll_interval: Duration::from_millis(1),
            inactivity_timeout: Duration::from_millis(2),
            max_consecutive_poll_errors: DEFAULT_ERROR_BUDGET,
        };

        let outcome = watch_pr(&PrHandle { number: 1 }, &provider, &config)
            .await
            .unwrap();
        assert_eq!(outcome, WatchOutcome::TimedOut);
    }

    /// Acceptance criterion (issue #5): merged-PR detection, exercised
    /// through the full polling loop (not just `decide_step` in isolation)
    /// -- a PR that's still pending on one poll and reports `MERGED` on the
    /// next must end the watch as `WatchOutcome::Merged`.
    #[tokio::test(start_paused = true)]
    async fn watch_pr_returns_merged_once_the_pr_lifecycle_flips_to_merged() {
        let provider = ScriptedProvider::new(vec![
            open_status(vec![pending("build")]),
            PrStatus {
                lifecycle: PrLifecycle::Merged,
                checks: vec![passed("build")],
            },
        ]);
        let config = WatchConfig {
            poll_interval: Duration::from_millis(1),
            inactivity_timeout: Duration::from_secs(3600),
            max_consecutive_poll_errors: DEFAULT_ERROR_BUDGET,
        };

        let outcome = watch_pr(&PrHandle { number: 1 }, &provider, &config)
            .await
            .unwrap();
        assert_eq!(outcome, WatchOutcome::Merged);
    }

    /// Acceptance criterion (issue #5): closed-without-merging detection,
    /// exercised through the full polling loop.
    #[tokio::test(start_paused = true)]
    async fn watch_pr_returns_closed_once_the_pr_is_closed_without_merging() {
        let provider = ScriptedProvider::new(vec![
            open_status(vec![pending("build")]),
            PrStatus {
                lifecycle: PrLifecycle::Closed,
                checks: vec![pending("build")],
            },
        ]);
        let config = WatchConfig {
            poll_interval: Duration::from_millis(1),
            inactivity_timeout: Duration::from_secs(3600),
            max_consecutive_poll_errors: DEFAULT_ERROR_BUDGET,
        };

        let outcome = watch_pr(&PrHandle { number: 1 }, &provider, &config)
            .await
            .unwrap();
        assert_eq!(outcome, WatchOutcome::Closed);
    }

    /// Acceptance criterion (issue #5): a CI failure must surface findings
    /// so the orchestrator can decide to reboucle vers le coder --
    /// exercised through the full polling loop, not just `decide_step`.
    #[tokio::test(start_paused = true)]
    async fn watch_pr_returns_checks_failed_with_findings_once_a_check_fails() {
        let provider = ScriptedProvider::new(vec![
            open_status(vec![pending("build")]),
            open_status(vec![failed("build"), passed("lint")]),
        ]);
        let config = WatchConfig {
            poll_interval: Duration::from_millis(1),
            inactivity_timeout: Duration::from_secs(3600),
            max_consecutive_poll_errors: DEFAULT_ERROR_BUDGET,
        };

        let outcome = watch_pr(&PrHandle { number: 1 }, &provider, &config)
            .await
            .unwrap();
        match outcome {
            WatchOutcome::ChecksFailed(findings) => {
                assert_eq!(findings.len(), 1);
                assert_eq!(findings[0].source, FindingSource::Ci);
                assert_eq!(findings[0].severity, Severity::Blocking);
                assert!(findings[0].description.contains("build"));
            }
            other => panic!("expected ChecksFailed, got {other:?}"),
        }
    }

    // ---- transient poll-error tolerance (issue #5 fix-cycle 1) -------------

    /// A [`CiProvider`] whose scripted responses are full `Result`s rather
    /// than always-`Ok` statuses, standing in for a `gh` invocation that
    /// sometimes fails transiently before recovering (or doesn't).
    struct ScriptedResultProvider {
        responses: std::sync::Mutex<std::collections::VecDeque<Result<PrStatus>>>,
    }

    impl ScriptedResultProvider {
        fn new(responses: Vec<Result<PrStatus>>) -> Self {
            Self {
                responses: std::sync::Mutex::new(responses.into()),
            }
        }
    }

    impl CiProvider for ScriptedResultProvider {
        async fn pr_status(&self, _pr: &PrHandle) -> Result<PrStatus> {
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("ScriptedResultProvider ran out of scripted responses")
        }
    }

    /// A `gh`-command failure -- the shape `is_transient_poll_error`
    /// recognizes as transient/retryable (e.g. a rate limit or network
    /// blip), not a malformed response.
    fn transient_error() -> GatedError {
        GatedError::GhCommandFailed {
            command: "gh pr view".to_string(),
            exit_code: Some(1),
            stderr: "rate limited".to_string(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn watch_pr_tolerates_fewer_than_the_configured_consecutive_poll_errors_then_recovers() {
        // K = 2 consecutive transient failures, N (budget) = 3 -- still
        // within budget, so `watch_pr` must retry through them and still
        // reach its terminal outcome from the next, successful poll.
        let provider = ScriptedResultProvider::new(vec![
            Err(transient_error()),
            Err(transient_error()),
            Ok(open_status(vec![passed("build")])),
        ]);
        let config = WatchConfig {
            poll_interval: Duration::from_millis(1),
            inactivity_timeout: Duration::from_secs(3600),
            max_consecutive_poll_errors: 3,
        };

        let outcome = watch_pr(&PrHandle { number: 1 }, &provider, &config)
            .await
            .unwrap();
        assert_eq!(outcome, WatchOutcome::ChecksPassed);
    }

    #[tokio::test(start_paused = true)]
    async fn watch_pr_aborts_after_more_than_the_configured_consecutive_poll_errors() {
        // N + 1 = 4 consecutive transient failures against a budget of 3:
        // the first three are tolerated/retried, the fourth exceeds the
        // budget and must abort with that same typed error.
        let provider = ScriptedResultProvider::new(vec![
            Err(transient_error()),
            Err(transient_error()),
            Err(transient_error()),
            Err(transient_error()),
        ]);
        let config = WatchConfig {
            poll_interval: Duration::from_millis(1),
            inactivity_timeout: Duration::from_secs(3600),
            max_consecutive_poll_errors: 3,
        };

        let result = watch_pr(&PrHandle { number: 1 }, &provider, &config).await;
        assert!(matches!(result, Err(GatedError::GhCommandFailed { .. })));
    }

    #[tokio::test(start_paused = true)]
    async fn watch_pr_aborts_immediately_on_a_malformed_response_without_retrying() {
        // A non-transient (malformed/unexpected) error must abort on the
        // very first occurrence, never counted against the transient-error
        // budget and never silently retried -- code-standards.md: never
        // trust/paper over an unparseable provider response.
        let provider = ScriptedResultProvider::new(vec![Err(GatedError::UnknownPrLifecycle(
            "BOGUS".to_string(),
        ))]);
        let config = WatchConfig {
            poll_interval: Duration::from_millis(1),
            inactivity_timeout: Duration::from_secs(3600),
            max_consecutive_poll_errors: 3,
        };

        let result = watch_pr(&PrHandle { number: 1 }, &provider, &config).await;
        assert!(matches!(result, Err(GatedError::UnknownPrLifecycle(_))));
    }

    // ---- busy-spin proof (deterministic, virtual clock) --------------------

    /// Issue #5: `watch_pr` "never busy-spins: it always awaits
    /// `tokio::time::sleep(poll_interval)` between iterations" (this
    /// module's own doc comment on `watch_pr`). Fix-cycle 1 (issue #5
    /// review): proven deterministically under tokio's paused virtual
    /// clock, rather than measuring real wall-clock gaps between polls
    /// (code-standards.md: "tests déterministes, pas de temps réel non
    /// mocké") -- a busy-spinning implementation would issue far more polls
    /// than the virtual time budget allows (since none of its iterations
    /// would ever await a timer to advance the paused clock), so bounding
    /// the poll count against how much virtual time actually elapsed proves
    /// each iteration genuinely waited on `sleep`.
    struct CountingProvider {
        poll_count: std::sync::Mutex<u32>,
    }

    impl CiProvider for CountingProvider {
        async fn pr_status(&self, _pr: &PrHandle) -> Result<PrStatus> {
            *self.poll_count.lock().unwrap() += 1;
            Ok(open_status(vec![]))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn watch_pr_polls_at_most_once_per_poll_interval_of_virtual_time_elapsed() {
        let poll_interval = Duration::from_millis(30);
        let inactivity_timeout = Duration::from_millis(90);
        let provider = CountingProvider {
            poll_count: std::sync::Mutex::new(0),
        };
        let config = WatchConfig {
            poll_interval,
            inactivity_timeout,
            max_consecutive_poll_errors: DEFAULT_ERROR_BUDGET,
        };

        let outcome = watch_pr(&PrHandle { number: 1 }, &provider, &config)
            .await
            .unwrap();
        assert_eq!(outcome, WatchOutcome::TimedOut);

        // Every poll but the first is preceded by one full `sleep(poll_interval)`
        // (the loop only reaches a next poll via that await point), so the
        // number of polls issued can never exceed
        // `inactivity_timeout / poll_interval + 1` regardless of how much
        // real wall-clock time this test takes to run -- a busy-spinning
        // loop that never actually awaited the timer would instead issue an
        // effectively unbounded number of polls before the virtual clock
        // ever advanced past `inactivity_timeout`.
        let max_possible_polls =
            (inactivity_timeout.as_millis() / poll_interval.as_millis()) as u32 + 1;
        let poll_count = *provider.poll_count.lock().unwrap();
        assert!(
            poll_count >= 2,
            "expected at least two polls before timing out, got {poll_count}"
        );
        assert!(
            poll_count <= max_possible_polls,
            "watch_pr issued {poll_count} polls, more than the {max_possible_polls} a \
             sleep-between-polls loop could possibly issue before \
             {inactivity_timeout:?} of virtual time elapsed -- looks like it busy-spun \
             instead of awaiting the timer"
        );
    }

    /// Acceptance criterion (issue #5, non-negotiable): "aucun merge
    /// automatique n'est déclenché par Warden". `CiProvider` (this module,
    /// above) exposes only `pr_status` -- there is no method on the trait
    /// that could merge a PR, so no `watch_pr` caller can trigger one
    /// through this seam. This is a static regression guard for the actual
    /// `gh` invocation in the watcher's only real implementation
    /// (`gh_provider::GhProvider`): it fails loudly if a `gh pr merge` (or
    /// equivalent "merge" argument) is ever wired into the CI-watching path.
    #[test]
    fn the_ci_watcher_path_never_issues_a_gh_merge_argument() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        for relative_path in ["src/ci_watcher.rs", "src/gh_provider.rs"] {
            let contents = std::fs::read_to_string(format!("{manifest_dir}/{relative_path}"))
                .unwrap_or_else(|error| panic!("failed to read {relative_path}: {error}"));
            // Scan production code only: this very test file's own source
            // (read back via `relative_path == "src/ci_watcher.rs"`) quotes
            // the literal it's guarding against in its own doc comments/
            // assertion message, so scanning past the `#[cfg(test)]` module
            // boundary would trivially match itself.
            let production_code = contents
                .split_once("#[cfg(test)]")
                .map(|(before, _)| before)
                .unwrap_or(contents.as_str());
            let merge_arg = format!("{quote}merge{quote}", quote = '"');
            assert!(
                !production_code.contains(&merge_arg),
                "{relative_path}'s production code must never pass a `merge` argument to `gh` -- \
                 the CI watcher must stay read-only (issue #5)"
            );
        }
    }
}
