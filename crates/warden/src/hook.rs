//! The **lifecycle-hook seam** (issue #55, ADR-0017): the I/O-bearing half of
//! the deterministic-action foundation whose *pure* half
//! ([`warden_core::HookPoint`]/[`HookContext`]/[`HookOutcome`]) lives in
//! `warden-core`.
//!
//! A [`Hook`] is a deterministic action Warden runs itself at a fixed
//! [`HookPoint`] of a run -- format, test, lint, commit-check -- instead of
//! delegating it to an agent's prompt. Foundation only: this ships the trait,
//! the [`HookRegistry`], and the dispatch ([`HookRegistry::run_hooks`]) the
//! orchestrator calls at its transition seam. The registry is **empty by
//! default**, so the seam is a strict no-op and behaviour is unchanged; no
//! concrete hook ships here.
//!
//! A hook that runs a command is meant to go through the same
//! [`warden_sandbox::Sandbox`] an agent does -- same isolation, fixed action,
//! no LLM. That wiring, and consuming a [`HookOutcome::Block`] /
//! [`HookOutcome::EmitFindings`] in the convergence loop, is issue #51; this
//! module only defines the seam and propagates the outcome.
//!
//! **Disambiguation**: a Warden *lifecycle* hook, distinct from the git
//! `post-receive` hook `warden-gated` installs (`warden-gated`'s `hook.rs`).

use std::sync::Arc;

use async_trait::async_trait;
use warden_core::{HookContext, HookOutcome, HookPoint};

use crate::error::Result;

/// A deterministic action bound to one or more [`HookPoint`]s. Runs inside the
/// `warden` crate (where FS/process/sandbox live), never in `warden-core`.
#[async_trait]
pub trait Hook: Send + Sync {
    /// The points at which this hook fires. Consulted once per dispatch; a
    /// hook may declare several.
    fn points(&self) -> &[HookPoint];

    /// Runs the hook for `ctx` (whose [`HookContext::point`] is one of
    /// [`Hook::points`]) and reports what it decided. An `Err` is a genuine
    /// failure to *run* the action (a spawn failed, I/O broke) and propagates;
    /// a hook that ran fine but wants to stop or reboucle the run says so with
    /// [`HookOutcome`], not `Err`.
    async fn run(&self, ctx: &HookContext<'_>) -> Result<HookOutcome>;
}

/// The set of [`Hook`]s a run dispatches, in **registration order**.
///
/// Order matters and is deterministic: [`HookRegistry::run_hooks`] runs the
/// hooks registered for a point in the order they were [`HookRegistry::register`]ed
/// (issue #55's "ordre d'exécution multi-hooks... déterministe : ordre
/// d'enregistrement"). A single flat `Vec` preserves that global order; a
/// point's hooks are simply the registered ones whose [`Hook::points`] contain
/// it.
///
/// Empty by default ([`HookRegistry::new`]) -- the state the orchestrator ships
/// with, which is what makes the dispatch seam a strict no-op.
#[derive(Default)]
pub struct HookRegistry {
    hooks: Vec<Arc<dyn Hook>>,
}

impl HookRegistry {
    /// A registry with no hooks -- dispatch is a strict no-op
    /// ([`HookOutcome::Continue`]).
    pub fn new() -> Self {
        Self { hooks: Vec::new() }
    }

    /// Appends `hook` to the registry. Its position here fixes its execution
    /// order relative to every other hook that shares one of its points.
    pub fn register(&mut self, hook: Arc<dyn Hook>) {
        self.hooks.push(hook);
    }

    /// Whether any hook is registered at all -- lets a caller (the
    /// orchestrator) skip building a [`HookContext`] when there is provably
    /// nothing to dispatch to.
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    /// Runs every hook registered for `point`, in registration order, and
    /// aggregates their [`HookOutcome`]s into one:
    ///
    /// - the **first** [`HookOutcome::Block`] short-circuits -- no later hook
    ///   on the point runs, and its reason is returned (a block is a hard
    ///   stop; running further deterministic actions past it would be
    ///   meaningless);
    /// - otherwise every [`HookOutcome::EmitFindings`] is concatenated (in
    ///   order) and returned as one `EmitFindings`, so a hook's findings feed
    ///   the convergence loop the same way reviewer/tester/CI findings do
    ///   (ADR-0011), never a parallel channel;
    /// - if no hook blocked and none emitted findings, the result is
    ///   [`HookOutcome::Continue`] -- always the case for the default empty
    ///   registry.
    ///
    /// An `Err` from any hook propagates immediately (a real failure to run
    /// the action).
    pub async fn run_hooks(&self, point: HookPoint, ctx: &HookContext<'_>) -> Result<HookOutcome> {
        let mut emitted: Vec<warden_core::Finding> = Vec::new();
        for hook in self.hooks.iter().filter(|h| h.points().contains(&point)) {
            match hook.run(ctx).await? {
                HookOutcome::Continue => {}
                HookOutcome::Block { reason } => return Ok(HookOutcome::Block { reason }),
                HookOutcome::EmitFindings(findings) => emitted.extend(findings),
            }
        }
        if emitted.is_empty() {
            Ok(HookOutcome::Continue)
        } else {
            Ok(HookOutcome::EmitFindings(emitted))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use warden_core::{Finding, FindingSource, RunState, Severity};

    use super::*;

    /// Records that it ran (and the order in which it did) and returns a
    /// preset outcome. `points` scopes which [`HookPoint`]s it fires on.
    struct FakeHook {
        points: Vec<HookPoint>,
        outcome: HookOutcome,
        order: Arc<AtomicUsize>,
        ran_at: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Hook for FakeHook {
        fn points(&self) -> &[HookPoint] {
            &self.points
        }

        async fn run(&self, _ctx: &HookContext<'_>) -> Result<HookOutcome> {
            self.ran_at.store(
                self.order.fetch_add(1, Ordering::SeqCst) + 1,
                Ordering::SeqCst,
            );
            Ok(self.outcome.clone())
        }
    }

    fn ctx(point: HookPoint) -> HookContext<'static> {
        HookContext {
            point,
            run_id: "run-1",
            state: RunState::CoderRunning,
            cycle: Some(0),
            worktree: None,
            commit: None,
            diff: None,
        }
    }

    fn blocking_finding(desc: &str) -> Finding {
        Finding {
            source: FindingSource::Warden,
            severity: Severity::Blocking,
            file: None,
            description: desc.to_string(),
            action: None,
        }
    }

    #[tokio::test]
    async fn empty_registry_is_a_no_op_continue() {
        let registry = HookRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(
            registry
                .run_hooks(HookPoint::OnCycleStart, &ctx(HookPoint::OnCycleStart))
                .await
                .unwrap(),
            HookOutcome::Continue
        );
    }

    #[tokio::test]
    async fn a_hook_registered_on_a_point_fires_with_its_context_and_outcome() {
        let order = Arc::new(AtomicUsize::new(0));
        let ran_at = Arc::new(AtomicUsize::new(0));
        let mut registry = HookRegistry::new();
        registry.register(Arc::new(FakeHook {
            points: vec![HookPoint::BeforeReview],
            outcome: HookOutcome::Block {
                reason: "nope".to_string(),
            },
            order: order.clone(),
            ran_at: ran_at.clone(),
        }));

        // Fires on its point, its outcome is propagated...
        assert_eq!(
            registry
                .run_hooks(HookPoint::BeforeReview, &ctx(HookPoint::BeforeReview))
                .await
                .unwrap(),
            HookOutcome::Block {
                reason: "nope".to_string()
            }
        );
        assert_eq!(ran_at.load(Ordering::SeqCst), 1, "hook should have run");

        // ...but not on a point it did not register for.
        ran_at.store(0, Ordering::SeqCst);
        assert_eq!(
            registry
                .run_hooks(HookPoint::BeforeTest, &ctx(HookPoint::BeforeTest))
                .await
                .unwrap(),
            HookOutcome::Continue
        );
        assert_eq!(ran_at.load(Ordering::SeqCst), 0, "hook should not have run");
    }

    #[tokio::test]
    async fn hooks_run_in_registration_order() {
        let order = Arc::new(AtomicUsize::new(0));
        let first_at = Arc::new(AtomicUsize::new(0));
        let second_at = Arc::new(AtomicUsize::new(0));
        let mut registry = HookRegistry::new();
        registry.register(Arc::new(FakeHook {
            points: vec![HookPoint::OnCycleEnd],
            outcome: HookOutcome::Continue,
            order: order.clone(),
            ran_at: first_at.clone(),
        }));
        registry.register(Arc::new(FakeHook {
            points: vec![HookPoint::OnCycleEnd],
            outcome: HookOutcome::Continue,
            order: order.clone(),
            ran_at: second_at.clone(),
        }));

        registry
            .run_hooks(HookPoint::OnCycleEnd, &ctx(HookPoint::OnCycleEnd))
            .await
            .unwrap();

        assert_eq!(first_at.load(Ordering::SeqCst), 1);
        assert_eq!(second_at.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn first_block_short_circuits_later_hooks() {
        let order = Arc::new(AtomicUsize::new(0));
        let blocker_at = Arc::new(AtomicUsize::new(0));
        let after_at = Arc::new(AtomicUsize::new(0));
        let mut registry = HookRegistry::new();
        registry.register(Arc::new(FakeHook {
            points: vec![HookPoint::BeforePush],
            outcome: HookOutcome::Block {
                reason: "first".to_string(),
            },
            order: order.clone(),
            ran_at: blocker_at.clone(),
        }));
        registry.register(Arc::new(FakeHook {
            points: vec![HookPoint::BeforePush],
            outcome: HookOutcome::Block {
                reason: "second".to_string(),
            },
            order: order.clone(),
            ran_at: after_at.clone(),
        }));

        assert_eq!(
            registry
                .run_hooks(HookPoint::BeforePush, &ctx(HookPoint::BeforePush))
                .await
                .unwrap(),
            HookOutcome::Block {
                reason: "first".to_string()
            }
        );
        assert_eq!(blocker_at.load(Ordering::SeqCst), 1);
        assert_eq!(
            after_at.load(Ordering::SeqCst),
            0,
            "second hook must not run"
        );
    }

    #[tokio::test]
    async fn emitted_findings_aggregate_in_order() {
        let order = Arc::new(AtomicUsize::new(0));
        let mut registry = HookRegistry::new();
        registry.register(Arc::new(FakeHook {
            points: vec![HookPoint::AfterTest],
            outcome: HookOutcome::EmitFindings(vec![blocking_finding("a")]),
            order: order.clone(),
            ran_at: Arc::new(AtomicUsize::new(0)),
        }));
        registry.register(Arc::new(FakeHook {
            points: vec![HookPoint::AfterTest],
            outcome: HookOutcome::EmitFindings(vec![blocking_finding("b")]),
            order: order.clone(),
            ran_at: Arc::new(AtomicUsize::new(0)),
        }));

        assert_eq!(
            registry
                .run_hooks(HookPoint::AfterTest, &ctx(HookPoint::AfterTest))
                .await
                .unwrap(),
            HookOutcome::EmitFindings(vec![blocking_finding("a"), blocking_finding("b")])
        );
    }
}
