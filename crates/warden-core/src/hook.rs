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
    /// The run is starting -- fires **once**, before the first coder cycle and
    /// before any state transition, so a deterministic *environment setup*
    /// action (bring a `docker compose` stack up, `git fetch`/pull, install
    /// dependencies) runs before the coder ever does. This is the token-saving
    /// motivation for hooks in the first place: such setup is repeatable and
    /// mechanical, so Warden does it itself rather than spending an LLM
    /// round-trip (and widening the agent's tool surface) on it.
    ///
    /// Unlike the cycle/transition points below, this is **not** reachable via
    /// [`HookPoint::on_entering`]: it fires from an explicit dispatch at run
    /// start (while the run is still `Pending`), not on entering a state, and
    /// exactly once per run rather than once per cycle. A
    /// [`HookOutcome::Block`] here aborts the run before the coder runs (the
    /// setup could not be established, so there is nothing to code against).
    OnRunStart,
    /// A new coder cycle is about to begin (entering [`RunState::CoderRunning`]).
    OnCycleStart,
    /// Just before the coder agent runs.
    BeforeCoder,
    /// Just after the coder agent produced its commit.
    AfterCoder,
    /// A cycle's commit was just produced -- the point a deterministic commit
    /// hook (e.g. a conventional-commit check, a sign-off) would run.
    OnCommit,
    /// Just before the reviewer agent runs (entering `RunState::RunningStep(1)`
    /// in the built-in default workflow -- see [`HookPoint::on_entering`]).
    BeforeReview,
    /// Just after the reviewer agent produced its findings.
    AfterReview,
    /// Just before the tester agent runs (entering `RunState::RunningStep(2)`
    /// in the built-in default workflow -- see [`HookPoint::on_entering`]).
    BeforeTest,
    /// Just after the tester agent produced its findings.
    AfterTest,
    /// A cycle has fully closed (findings recorded, next state decided).
    OnCycleEnd,
    /// The run converged (entering [`RunState::Converged`]).
    OnConverged,
    /// Just before the converged commit is pushed (entering [`RunState::Pushed`]).
    BeforePush,
    /// The run is ending -- fires **once**, after the convergence loop exits,
    /// whatever its final state (converged, pushed, budget-exhausted, or
    /// failed). The teardown counterpart of [`HookPoint::OnRunStart`]: it tears
    /// down whatever setup established (bring the `docker compose` stack down,
    /// remove scratch state), so it runs like a `finally` -- on every exit
    /// path, including failure, not only the happy one.
    ///
    /// Also **not** reachable via [`HookPoint::on_entering`] (it brackets the
    /// whole run, not a state entry). A [`HookOutcome::Block`] here is
    /// meaningless -- the run is already over -- so the run-end dispatch does
    /// not abort on one; teardown is best-effort.
    OnRunEnd,
}

impl HookPoint {
    /// Stable string form -- for events/logs/tests. Never change an existing
    /// variant's string without accounting for anything that persisted it.
    pub fn as_str(self) -> &'static str {
        match self {
            HookPoint::OnRunStart => "on_run_start",
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
            HookPoint::OnRunEnd => "on_run_end",
        }
    }

    /// Parses the stable string form ([`HookPoint::as_str`]) back into a
    /// point -- the inverse used when loading a declarative hook config
    /// (`.warden/hooks.toml`, where a hook names its point as `"on_run_start"`
    /// etc.). Returns `None` for an unknown string so the caller can raise a
    /// config error that lists the valid names, rather than this panicking.
    pub fn parse(s: &str) -> Option<HookPoint> {
        Some(match s {
            "on_run_start" => HookPoint::OnRunStart,
            "on_cycle_start" => HookPoint::OnCycleStart,
            "before_coder" => HookPoint::BeforeCoder,
            "after_coder" => HookPoint::AfterCoder,
            "on_commit" => HookPoint::OnCommit,
            "before_review" => HookPoint::BeforeReview,
            "after_review" => HookPoint::AfterReview,
            "before_test" => HookPoint::BeforeTest,
            "after_test" => HookPoint::AfterTest,
            "on_cycle_end" => HookPoint::OnCycleEnd,
            "on_converged" => HookPoint::OnConverged,
            "before_push" => HookPoint::BeforePush,
            "on_run_end" => HookPoint::OnRunEnd,
            _ => return None,
        })
    }

    /// Every point, in declaration order -- for config validation error
    /// messages (listing the valid `point` names) and exhaustiveness tests.
    pub const ALL: [HookPoint; 13] = [
        HookPoint::OnRunStart,
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
        HookPoint::OnRunEnd,
    ];

    /// The hook point that firing *on entering* `state` corresponds to, if
    /// any. This is the mapping the orchestrator's transition seam uses to
    /// dispatch hooks from the single `self.transition(...)` point (issue
    /// #55): every legal transition names the state it enters, and a subset
    /// of those states is a meaningful lifecycle milestone.
    ///
    /// [`HookPoint::OnRunStart`] and [`HookPoint::OnRunEnd`] are deliberately
    /// absent from this mapping: they bracket the whole run rather than a state
    /// entry, and fire from explicit dispatch at run start/end, not here.
    ///
    /// States with no lifecycle point of their own (`Pending`, `AwaitingCi`,
    /// `Done`, `StepCyclesExceeded`, `Failed`) return `None` -- entering them
    /// fires no hook.
    ///
    /// **Issue #73**: `RunState::RunningStep`'s index is mapped by position
    /// in the built-in default workflow only -- index `1` (the reviewer's
    /// own step) is `BeforeReview`, index `2` (the tester's) is `BeforeTest`,
    /// exactly as `Reviewing`/`Testing` used to map before this issue. Any
    /// other index (a custom workflow's own extra step, e.g. `techlead`)
    /// currently has no lifecycle point of its own and returns `None` --
    /// wiring a generic per-step hook point is left to a future issue, not
    /// this one.
    pub fn on_entering(state: RunState) -> Option<HookPoint> {
        match state {
            RunState::CoderRunning => Some(HookPoint::OnCycleStart),
            RunState::RunningStep(1) => Some(HookPoint::BeforeReview),
            RunState::RunningStep(2) => Some(HookPoint::BeforeTest),
            RunState::Converged => Some(HookPoint::OnConverged),
            RunState::Pushed => Some(HookPoint::BeforePush),
            RunState::Pending
            | RunState::RunningStep(_)
            | RunState::AwaitingCi
            | RunState::Done
            | RunState::StepCyclesExceeded(_)
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
    /// The run's repository working directory -- the checkout the run was
    /// launched against (`RUNS.repo_path`). Always present: it is the natural
    /// cwd for a run-level setup/teardown action ([`HookPoint::OnRunStart`] /
    /// [`HookPoint::OnRunEnd`]), which operates on the repo as a whole (bring a
    /// `docker compose` stack up, `git pull`) rather than on any one role's
    /// [`HookContext::worktree`].
    pub repo_path: &'a Path,
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
            HookPoint::on_entering(RunState::RunningStep(1)),
            Some(HookPoint::BeforeReview)
        );
        assert_eq!(
            HookPoint::on_entering(RunState::RunningStep(2)),
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
            RunState::RunningStep(3),
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
