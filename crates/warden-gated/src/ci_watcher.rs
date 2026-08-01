use std::time::Duration;

use tokio::time::{sleep, Instant};
use warden_core::{Finding, FindingSource, Severity};

use crate::error::{GatedError, Result};
use crate::pr_manager::PrHandle;

/// Coarse GitHub PR lifecycle, independent of CI/check status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrLifecycle {
    Open,
    Merged,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CheckConclusion {
    Pending,
    Passed,
    Failed,
}

/// One CI check's name, outcome, and (if any) a link to its details -- enough to describe a failure
/// to a human or fold into a [`Finding`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckRun {
    pub name: String,
    pub conclusion: CheckConclusion,
    pub details_url: Option<String>,
}

/// A PR's full polled status: its lifecycle plus every CI check currently reported against its head
/// commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrStatus {
    pub lifecycle: PrLifecycle,
    pub checks: Vec<CheckRun>,
}

/// The net CI signal `PrStatus::checks_rollup` reduces `checks` to -- `decide_step` only needs this
/// coarser view, not every individual check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChecksRollup {
    /// No CI has reported anything against this PR yet -- either it hasn't triggered, or it never
    /// will (the case the inactivity timeout guards against).
    NoChecksYet,
    /// At least one check has reported, none have failed yet, but at least one is still running.
    Pending,
    /// Every reported check passed (a `Skipped`/`Neutral` conclusion counts as passed, not blocking
    /// -- it never ran by design, not because it failed).
    AllPassed,
    /// At least one check failed. Carries the failing checks themselves so the caller can describe
    /// exactly what broke.
    SomeFailed(Vec<CheckRun>),
}

impl PrStatus {
    /// Reduces `checks` to the coarse [`ChecksRollup`] `decide_step` acts on.
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

#[allow(async_fn_in_trait)]
pub trait CiProvider {
    /// Fetches `pr`'s current lifecycle and CI check statuses.
    async fn pr_status(&self, pr: &PrHandle) -> Result<PrStatus>;
}

/// The terminal result of one [`watch_pr`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchOutcome {
    Merged,
    /// Closed without merging.
    Closed,
    ChecksPassed,
    ChecksFailed(Vec<Finding>),
    /// The polled status went unchanged for at least `inactivity_timeout` -- the only thing that
    /// bounds this loop when CI never triggers at all.
    TimedOut,
}

/// Configuration for one [`watch_pr`] invocation.
#[derive(Debug, Clone, Copy)]
pub struct WatchConfig {
    /// How long to sleep between two polls.
    pub poll_interval: Duration,
    /// How long the polled status may go completely unchanged before `watch_pr` gives up and
    /// returns [`WatchOutcome::TimedOut`].
    pub inactivity_timeout: Duration,
    pub max_consecutive_poll_errors: u32,
}

impl WatchConfig {
    /// A sensible default retry budget for transient poll failures: enough to ride out a single
    /// rate-limit/network blip without masking a truly broken `gh`/network setup forever.
    pub const DEFAULT_MAX_CONSECUTIVE_POLL_ERRORS: u32 = 3;
}

/// A comparable snapshot of a [`PrStatus`], used only to detect whether anything changed between
/// two polls (the inactivity clock).
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

/// One [`watch_pr`] iteration's verdict: either the watch is over ([`Self::Terminal`]), or the
/// caller should sleep and poll again.
enum WatchStep {
    Terminal(WatchOutcome),
    KeepWaiting,
}

/// Decides one polling step purely from the latest status plus how long nothing has changed.
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

fn is_transient_poll_error(error: &GatedError) -> bool {
    matches!(
        error,
        GatedError::GhCommandFailed { .. } | GatedError::Io(_)
    )
}

/// Polls `pr`'s status via `provider` until a terminal [`WatchOutcome`] is reached, sleeping
/// `config.poll_interval` between polls -- never busy-spinning.
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

    const DEFAULT_ERROR_BUDGET: u32 = WatchConfig::DEFAULT_MAX_CONSECUTIVE_POLL_ERRORS;

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

    fn transient_error() -> GatedError {
        GatedError::GhCommandFailed {
            command: "gh pr view".to_string(),
            exit_code: Some(1),
            stderr: "rate limited".to_string(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn watch_pr_tolerates_fewer_than_the_configured_consecutive_poll_errors_then_recovers() {
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

    #[test]
    fn the_ci_watcher_path_never_issues_a_gh_merge_argument() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        for relative_path in ["src/ci_watcher.rs", "src/gh_provider.rs"] {
            let contents = std::fs::read_to_string(format!("{manifest_dir}/{relative_path}"))
                .unwrap_or_else(|error| panic!("failed to read {relative_path}: {error}"));
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
