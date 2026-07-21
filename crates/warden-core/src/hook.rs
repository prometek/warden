//! Lifecycle-hook vocabulary (issue #55, ADR-0017): the *pure* half of the
//! deterministic-action seam.
//!
//! Some actions Warden performs around a run are **repeatable and
//! deterministic** -- formatting, running the test suite, committing, linting.
//! Delegating them to the *agent* (in its prompt) is wrong on three axes:
//! security (the agent needs `Bash`/tooling to do them, widening its surface),
//! tokens (each action is an LLM round-trip), and determinism (an LLM can
//! forget, vary, or mis-execute a mechanical step). Lifecycle **hooks** are
//! run by Warden itself, deterministically, at fixed points in a run instead.
//!
//! This module holds only the *types* that describe such a hook's contract --
//! [`HookPoint`] (when), [`HookContext`] (what it is told), [`HookOutcome`]
//! (what it decides). It stays as pure as the rest of `warden-core`: no
//! filesystem, no process, no clock. The `Hook` trait, the registry, and the
//! dispatch that actually *runs* hooks live in the `warden` crate, where FS,
//! process, and the sandbox seam (issue #50) already live -- a hook that runs
//! a command goes through the same `Sandbox` an agent does, same isolation,
//! but a fixed action rather than an LLM.
//!
//! **Disambiguation**: "hook" here is a Warden *lifecycle* hook, distinct from
//! the git `post-receive` hook `warden-gated` installs (`warden-gated`'s own
//! `hook.rs`).
//!
//! **Scope of issue #55**: foundation only -- these types, the trait, the
//! registry, and the dispatch seam wired to the orchestrator's transitions
//! with an *empty* registry (a strict no-op). No concrete hook (fmt, `cargo
//! test`, commit, lint...) and no declarative config format ship here; those
//! come on top.

use std::path::Path;

use crate::convergence::Finding;
use crate::state::RunState;

/// A moment in a run's lifecycle at which hooks may fire. Mirrors the flow of
/// a single cycle (coder -> review -> test) and the run-level milestones
/// around it, so a deterministic action can be pinned to exactly when it
/// should happen -- e.g. `cargo fmt` on [`HookPoint::AfterCoder`], `cargo
/// test` on [`HookPoint::BeforeTest`], a lint gate on
/// [`HookPoint::OnCycleEnd`].
///
/// The variants are the *vocabulary*; issue #55 wires the subset that maps
/// cleanly onto an existing [`RunState`] entry (see
/// [`HookPoint::on_entering`]) at the orchestrator's single transition seam.
/// The remaining points (`BeforeCoder`/`AfterCoder`, `AfterReview`,
/// `AfterTest`, `OnCommit`, `OnCycleEnd`) are defined now and wired at the
/// richer call sites that carry their context as concrete hooks arrive -- no
/// enum change is needed then.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookPoint {
    /// A new coder cycle is about to begin (entering [`RunState::CoderRunning`]).
    OnCycleStart,
    /// Just before the coder agent runs.
    BeforeCoder,
    /// Just after the coder agent produced its commit.
    AfterCoder,
    /// A cycle's commit was just produced -- the point a deterministic commit
    /// hook (e.g. a conventional-commit check, a sign-off) would run.
    OnCommit,
    /// Just before the reviewer agent runs (entering [`RunState::Reviewing`]).
    BeforeReview,
    /// Just after the reviewer agent produced its findings.
    AfterReview,
    /// Just before the tester agent runs (entering [`RunState::Testing`]).
    BeforeTest,
    /// Just after the tester agent produced its findings.
    AfterTest,
    /// A cycle has fully closed (findings recorded, next state decided).
    OnCycleEnd,
    /// The run converged (entering [`RunState::Converged`]).
    OnConverged,
    /// Just before the converged commit is pushed (entering [`RunState::Pushed`]).
    BeforePush,
}

impl HookPoint {
    /// Stable string form -- for events/logs/tests. Never change an existing
    /// variant's string without accounting for anything that persisted it.
    pub fn as_str(self) -> &'static str {
        match self {
            HookPoint::OnCycleStart => "on_cycle_start",
            HookPoint::BeforeCoder => "before_coder",
            HookPoint::AfterCoder => "after_coder",
            HookPoint::OnCommit => "on_commit",
            HookPoint::BeforeReview => "before_review",
            HookPoint::AfterReview => "after_review",
            HookPoint::BeforeTest => "before_test",
            HookPoint::AfterTest => "after_test",
            HookPoint::OnCycleEnd => "on_cycle_end",
            HookPoint::OnConverged => "on_converged",
            HookPoint::BeforePush => "before_push",
        }
    }

    /// The hook point that firing *on entering* `state` corresponds to, if
    /// any. This is the mapping the orchestrator's transition seam uses to
    /// dispatch hooks from the single `self.transition(...)` point (issue
    /// #55): every legal transition names the state it enters, and a subset
    /// of those states is a meaningful lifecycle milestone.
    ///
    /// States with no lifecycle point of their own (`Pending`, `AwaitingCi`,
    /// `Done`, the `Max*Exceeded` exhaustion states, `Failed`) return `None`
    /// -- entering them fires no hook.
    pub fn on_entering(state: RunState) -> Option<HookPoint> {
        match state {
            RunState::CoderRunning => Some(HookPoint::OnCycleStart),
            RunState::Reviewing => Some(HookPoint::BeforeReview),
            RunState::Testing => Some(HookPoint::BeforeTest),
            RunState::Converged => Some(HookPoint::OnConverged),
            RunState::Pushed => Some(HookPoint::BeforePush),
            RunState::Pending
            | RunState::AwaitingCi
            | RunState::Done
            | RunState::MaxReviewCyclesExceeded
            | RunState::MaxTestCyclesExceeded
            | RunState::Failed => None,
        }
    }
}

/// What a hook is told about the run at the moment it fires. Deliberately a
/// bundle of **borrowed references** -- the type carries no ownership and does
/// no I/O of its own; the `warden`-side dispatch builds it from state it
/// already holds and hands it out for the duration of the hook call.
///
/// `worktree`, `commit`, and `diff` are `Option`: not every hook point has
/// them (e.g. [`HookPoint::OnCycleStart`] fires before any commit exists), and
/// the foundation's transition seam populates only what it holds there --
/// richer context is threaded from the call sites that carry it as concrete
/// hooks that need it arrive.
#[derive(Debug, Clone, Copy)]
pub struct HookContext<'a> {
    /// Which point fired this hook.
    pub point: HookPoint,
    /// The run this hook fires within (`RUNS.id`).
    pub run_id: &'a str,
    /// The state the run is entering as this hook fires.
    pub state: RunState,
    /// The overall loop-iteration counter for the current cycle, when the
    /// firing point is inside a cycle (`None` for run-level points that carry
    /// no single cycle).
    pub cycle: Option<u32>,
    /// The role's worktree the action would run against, when one applies.
    pub worktree: Option<&'a Path>,
    /// The cycle's commit, when one has been produced by the firing point.
    pub commit: Option<&'a str>,
    /// The cycle's diff, when available at the firing point.
    pub diff: Option<&'a str>,
}

/// What a hook decides once it has run. Aggregated across all hooks on a
/// point by the `warden`-side dispatch (registration order, deterministic).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookOutcome {
    /// Nothing to report -- the run proceeds unchanged. The only outcome the
    /// default *empty* registry can ever produce, which is what makes issue
    /// #55's seam a strict no-op.
    Continue,
    /// The hook refuses to let the run proceed past this point, with a
    /// human-readable reason. Consuming this (a policy gate) is issue #51, not
    /// this foundation.
    Block { reason: String },
    /// The hook produced [`Finding`]s that must feed the convergence loop the
    /// *same way* reviewer/tester/CI findings do (ADR-0011) -- not a parallel
    /// channel. A deterministic lint/test hook uses this to reboucle the coder
    /// on a blocking finding. Wiring these into the loop is issue #51.
    EmitFindings(Vec<Finding>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn on_entering_maps_the_lifecycle_milestone_states() {
        assert_eq!(
            HookPoint::on_entering(RunState::CoderRunning),
            Some(HookPoint::OnCycleStart)
        );
        assert_eq!(
            HookPoint::on_entering(RunState::Reviewing),
            Some(HookPoint::BeforeReview)
        );
        assert_eq!(
            HookPoint::on_entering(RunState::Testing),
            Some(HookPoint::BeforeTest)
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
            RunState::MaxReviewCyclesExceeded,
            RunState::MaxTestCyclesExceeded,
            RunState::Failed,
        ] {
            assert_eq!(HookPoint::on_entering(state), None);
        }
    }

    #[test]
    fn hook_point_strings_are_unique_and_stable() {
        let points = [
            HookPoint::OnCycleStart,
            HookPoint::BeforeCoder,
            HookPoint::AfterCoder,
            HookPoint::OnCommit,
            HookPoint::BeforeReview,
            HookPoint::AfterReview,
            HookPoint::BeforeTest,
            HookPoint::AfterTest,
            HookPoint::OnCycleEnd,
            HookPoint::OnConverged,
            HookPoint::BeforePush,
        ];
        let mut seen = std::collections::HashSet::new();
        for point in points {
            assert!(seen.insert(point.as_str()), "duplicate: {}", point.as_str());
        }
        assert_eq!(seen.len(), points.len());
    }
}
