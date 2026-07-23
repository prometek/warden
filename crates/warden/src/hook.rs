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
use warden_sandbox::{Command, ExecuteOptions, Sandbox, SandboxSpec};
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

impl std::fmt::Debug for HookRegistry {
    /// A `dyn Hook` is not `Debug` (and need not be); a registry's only
    /// externally interesting property is how many hooks it holds, which is
    /// enough for a test's `unwrap_err` message or a `tracing` field.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookRegistry")
            .field("hooks", &self.hooks.len())
            .finish()
    }
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

/// A [`Hook`] that runs one shell command through the [`Sandbox`], at the
/// [`HookPoint`]s it is bound to. This is the concrete hook a declarative
/// entry in `.warden/hooks.toml` compiles down to -- the deterministic
/// environment prep (`docker compose up -d`, `git fetch`/pull, dependency
/// install) that Warden runs itself rather than spending as agent tokens.
///
/// The command runs against [`HookContext::repo_path`] (the run's repository,
/// the natural cwd for a run-level setup/teardown action), via `sh -c "<run>"`
/// so a full shell line -- pipes, `&&`, env expansion -- works as written.
///
/// # Environment
///
/// Unlike an *agent* invocation -- which the sandbox runs with a cleared
/// environment plus a narrow adapter allowlist, so a coder never inherits the
/// operator's shell -- a `CommandHook` forwards the operator's **full**
/// environment. These are the operator's own trusted infra commands, and they
/// must behave exactly as they would in the operator's shell: `docker` needs
/// `DOCKER_HOST`, `git pull` over SSH needs `SSH_AUTH_SOCK`, everything needs
/// `HOME`. `LocalSandbox` provides no isolation from this host regardless (it
/// is the same OS user), so forwarding the full environment changes nothing
/// about the trust boundary -- it only makes the commands work.
///
/// # Failure
///
/// A non-zero exit is a [`HookOutcome::Block`] when `block_on_failure` is set
/// (a setup step that failed means the environment is not ready -- there is
/// nothing to run against), otherwise a logged `Continue`. An error actually
/// *running* the command (the sandbox failed to spawn) propagates as `Err`,
/// per the [`Hook::run`] contract.
pub struct CommandHook {
    points: Vec<HookPoint>,
    /// The raw shell line, kept for log/block messages.
    run: String,
    block_on_failure: bool,
    sandbox: Arc<dyn Sandbox>,
}

impl CommandHook {
    /// Binds `run` (a shell line) to `points`, executed through `sandbox`.
    /// `block_on_failure` decides whether a non-zero exit blocks the run.
    pub fn new(
        points: Vec<HookPoint>,
        run: impl Into<String>,
        block_on_failure: bool,
        sandbox: Arc<dyn Sandbox>,
    ) -> Self {
        Self {
            points,
            run: run.into(),
            block_on_failure,
            sandbox,
        }
    }

    /// The operator's full environment, as an allowlist of variable *names*
    /// (the sandbox resolves the values). See this type's own docs on why a
    /// hook forwards everything where an agent forwards almost nothing.
    fn full_env_allowlist() -> Vec<String> {
        std::env::vars().map(|(name, _)| name).collect()
    }
}

#[async_trait]
impl Hook for CommandHook {
    fn points(&self) -> &[HookPoint] {
        &self.points
    }

    async fn run(&self, ctx: &HookContext<'_>) -> Result<HookOutcome> {
        let id = self
            .sandbox
            .create(SandboxSpec {
                cwd: ctx.repo_path.to_path_buf(),
            })
            .await?;

        // The sandbox is always destroyed, on the error path as on the success
        // one (the create->destroy pairing `run_agent`'s `SandboxGuard` makes
        // structural -- kept explicit and simpler here since there is no long
        // streaming await to be dropped mid-flight). `execute` borrows `id`
        // until the returned `Execution` is consumed by `wait`; collapsing both
        // into one owned `Result<ExecutionResult>` here ends that borrow before
        // `id` is moved into `destroy`.
        let exec = self
            .sandbox
            .execute(
                &id,
                Command {
                    program: "sh".to_string(),
                    args: vec!["-c".to_string(), self.run.clone()],
                    env_allowlist: Self::full_env_allowlist(),
                    stdin: None,
                },
                ExecuteOptions::default(),
            )
            .await;
        let waited = match exec {
            Ok(execution) => execution.wait().await,
            Err(err) => Err(err),
        };
        let _ = self.sandbox.destroy(id.clone()).await;
        let output = waited?;

        if output.exit_code == 0 {
            return Ok(HookOutcome::Continue);
        }

        // A trimmed stderr tail makes the reason actionable without dumping a
        // whole build log into the event/log line.
        let stderr_tail: String = output.stderr.trim_end().chars().rev().take(500).collect();
        let stderr_tail: String = stderr_tail.chars().rev().collect();
        let reason = format!(
            "hook command `{}` at {} exited {}{}",
            self.run,
            ctx.point.as_str(),
            output.exit_code,
            if stderr_tail.is_empty() {
                String::new()
            } else {
                format!(": {stderr_tail}")
            }
        );

        if self.block_on_failure {
            Ok(HookOutcome::Block { reason })
        } else {
            tracing::warn!(
                run_id = ctx.run_id,
                point = ctx.point.as_str(),
                exit_code = output.exit_code,
                command = %self.run,
                "hook command failed but block_on_failure is false; continuing"
            );
            Ok(HookOutcome::Continue)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tempfile::TempDir;
    use warden_core::{Finding, FindingSource, RunState, Severity};
    use warden_sandbox::LocalSandbox;

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
            repo_path: Path::new("/tmp/repo"),
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

    /// A `HookContext` whose `repo_path` is a real directory, for the
    /// [`CommandHook`] tests that actually run a command against it.
    fn ctx_in<'a>(point: HookPoint, repo_path: &'a Path) -> HookContext<'a> {
        HookContext {
            point,
            run_id: "run-1",
            state: RunState::Pending,
            repo_path,
            cycle: None,
            worktree: None,
            commit: None,
            diff: None,
        }
    }

    #[tokio::test]
    async fn command_hook_continues_on_a_zero_exit() {
        let sandbox = Arc::new(LocalSandbox::new());
        let hook = CommandHook::new(vec![HookPoint::OnRunStart], "exit 0", true, sandbox);
        let dir = TempDir::new().unwrap();
        assert_eq!(
            hook.run(&ctx_in(HookPoint::OnRunStart, dir.path()))
                .await
                .unwrap(),
            HookOutcome::Continue
        );
    }

    #[tokio::test]
    async fn command_hook_blocks_on_a_non_zero_exit_when_block_on_failure() {
        let sandbox = Arc::new(LocalSandbox::new());
        let hook = CommandHook::new(
            vec![HookPoint::OnRunStart],
            "echo boom >&2; exit 3",
            true,
            sandbox,
        );
        let dir = TempDir::new().unwrap();
        match hook
            .run(&ctx_in(HookPoint::OnRunStart, dir.path()))
            .await
            .unwrap()
        {
            HookOutcome::Block { reason } => {
                assert!(reason.contains("exited 3"), "reason names the exit code: {reason}");
                assert!(reason.contains("boom"), "reason carries the stderr tail: {reason}");
            }
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn command_hook_continues_on_failure_when_not_block_on_failure() {
        let sandbox = Arc::new(LocalSandbox::new());
        let hook = CommandHook::new(vec![HookPoint::OnRunEnd], "exit 1", false, sandbox);
        let dir = TempDir::new().unwrap();
        assert_eq!(
            hook.run(&ctx_in(HookPoint::OnRunEnd, dir.path()))
                .await
                .unwrap(),
            HookOutcome::Continue,
            "a non-blocking hook's failure is logged, not a Block"
        );
    }

    #[tokio::test]
    async fn command_hook_runs_in_the_repo_path_cwd() {
        let sandbox = Arc::new(LocalSandbox::new());
        let hook = CommandHook::new(vec![HookPoint::OnRunStart], "touch ran.txt", true, sandbox);
        let dir = TempDir::new().unwrap();
        hook.run(&ctx_in(HookPoint::OnRunStart, dir.path()))
            .await
            .unwrap();
        assert!(
            dir.path().join("ran.txt").exists(),
            "the command runs with repo_path as its cwd"
        );
    }
}
