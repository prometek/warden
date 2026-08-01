use std::sync::Arc;

use async_trait::async_trait;
use warden_core::{HookContext, HookOutcome, HookPoint};
use warden_sandbox::{Command, ExecuteOptions, Sandbox, SandboxSpec};

use crate::error::Result;
use crate::policy_gate::{PolicyGate, PolicyOutcome};

/// A deterministic action bound to one or more [`HookPoint`]s.
#[async_trait]
pub trait Hook: Send + Sync {
    /// The points at which this hook fires.
    fn points(&self) -> &[HookPoint];

    /// Runs the hook for `ctx` (whose [`HookContext::point`] is one of [`Hook::points`]) and
    /// reports what it decided.
    async fn run(&self, ctx: &HookContext<'_>) -> Result<HookOutcome>;
}

/// The set of [`Hook`]s a run dispatches, in **registration order**.
#[derive(Default)]
pub struct HookRegistry {
    hooks: Vec<Arc<dyn Hook>>,
}

impl std::fmt::Debug for HookRegistry {
    /// A `dyn Hook` is not `Debug` (and need not be).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookRegistry")
            .field("hooks", &self.hooks.len())
            .finish()
    }
}

impl HookRegistry {
    /// A registry with no hooks -- dispatch is a strict no-op ([`HookOutcome::Continue`]).
    pub fn new() -> Self {
        Self { hooks: Vec::new() }
    }

    /// Appends `hook` to the registry. Its position here fixes its execution order relative to
    /// every other hook that shares one of its points.
    pub fn register(&mut self, hook: Arc<dyn Hook>) {
        self.hooks.push(hook);
    }

    /// Whether any hook is registered at all -- lets a caller (the orchestrator) skip building a
    /// [`HookContext`] when there is provably nothing to dispatch to.
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    /// Runs every hook registered for `point`, in registration order, and aggregates their
    /// [`HookOutcome`]s into one
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

/// A [`Hook`] that runs one shell command through the [`Sandbox`], at the [`HookPoint`]s it is
/// bound to.
pub struct CommandHook {
    points: Vec<HookPoint>,
    /// The raw shell line, kept for log/block messages.
    run: String,
    block_on_failure: bool,
    sandbox: Arc<dyn Sandbox>,
    policy_gate: Arc<PolicyGate>,
}

impl CommandHook {
    /// Binds `run` (a shell line) to `points`, executed through `sandbox` once `policy_gate` allows
    /// it.
    pub fn new(
        points: Vec<HookPoint>,
        run: impl Into<String>,
        block_on_failure: bool,
        sandbox: Arc<dyn Sandbox>,
        policy_gate: Arc<PolicyGate>,
    ) -> Self {
        Self {
            points,
            run: run.into(),
            block_on_failure,
            sandbox,
            policy_gate,
        }
    }

    /// The operator's full environment, as an allowlist of variable *names* (the sandbox resolves
    /// the values).
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
        if let PolicyOutcome::Blocked { reason } =
            evaluate_shell_policy(&self.policy_gate, ctx.run_id, &self.run).await
        {
            return Ok(HookOutcome::Block { reason });
        }

        let output = run_sandboxed_shell(&self.sandbox, ctx.repo_path, &self.run).await?;

        if output.exit_code == 0 {
            return Ok(HookOutcome::Continue);
        }

        let reason = format!(
            "hook command `{}` at {} exited {}{}",
            self.run,
            ctx.point.as_str(),
            output.exit_code,
            format_tail_suffix(&output.stderr_tail)
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

/// The last `max_chars` characters of `text`, trimmed of trailing whitespace -- shared tail-
/// truncation so a reason/log line never dumps a whole build log, just its actionable end.
pub(crate) fn trailing_chars(text: &str, max_chars: usize) -> String {
    let reversed: String = text.trim_end().chars().rev().take(max_chars).collect();
    reversed.chars().rev().collect()
}

/// The outcome of running one shell command through [`run_sandboxed_shell`].
struct SandboxedCommandOutput {
    exit_code: i32,
    stderr_tail: String,
}

async fn run_sandboxed_shell(
    sandbox: &Arc<dyn Sandbox>,
    cwd: &std::path::Path,
    command: &str,
) -> Result<SandboxedCommandOutput> {
    let id = sandbox
        .create(SandboxSpec {
            cwd: cwd.to_path_buf(),
        })
        .await?;

    let exec = sandbox
        .execute(
            &id,
            Command {
                program: "sh".to_string(),
                args: vec!["-c".to_string(), command.to_string()],
                env_allowlist: CommandHook::full_env_allowlist(),
                stdin: None,
            },
            ExecuteOptions::default(),
        )
        .await;
    let waited = match exec {
        Ok(execution) => execution.wait().await,
        Err(err) => Err(err),
    };
    let _ = sandbox.destroy(id.clone()).await;
    let output = waited?;

    Ok(SandboxedCommandOutput {
        exit_code: output.exit_code,
        stderr_tail: trailing_chars(&output.stderr, 500),
    })
}

/// `": <tail>"` when `tail` is non-empty, otherwise an empty string.
pub(crate) fn format_tail_suffix(tail: &str) -> String {
    if tail.is_empty() {
        String::new()
    } else {
        format!(": {tail}")
    }
}

/// Evaluates `command` as a `warden_policy::Action::Shell` against `policy_gate`.
pub(crate) async fn evaluate_shell_policy(
    policy_gate: &PolicyGate,
    run_id: &str,
    command: &str,
) -> PolicyOutcome {
    let description = format!("shell: {command}");
    policy_gate
        .decide(
            run_id,
            &description,
            &warden_policy::Action::Shell {
                command: command.to_string(),
            },
        )
        .await
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tempfile::TempDir;
    use warden_core::{Finding, FindingSource, RunState, Severity};
    use warden_sandbox::LocalSandbox;

    use super::*;

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

    fn empty_policy_gate() -> Arc<PolicyGate> {
        Arc::new(PolicyGate::empty())
    }

    #[tokio::test]
    async fn command_hook_continues_on_a_zero_exit() {
        let sandbox = Arc::new(LocalSandbox::new());
        let hook = CommandHook::new(
            vec![HookPoint::OnRunStart],
            "exit 0",
            true,
            sandbox,
            empty_policy_gate(),
        );
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
            empty_policy_gate(),
        );
        let dir = TempDir::new().unwrap();
        match hook
            .run(&ctx_in(HookPoint::OnRunStart, dir.path()))
            .await
            .unwrap()
        {
            HookOutcome::Block { reason } => {
                assert!(
                    reason.contains("exited 3"),
                    "reason names the exit code: {reason}"
                );
                assert!(
                    reason.contains("boom"),
                    "reason carries the stderr tail: {reason}"
                );
            }
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn command_hook_continues_on_failure_when_not_block_on_failure() {
        let sandbox = Arc::new(LocalSandbox::new());
        let hook = CommandHook::new(
            vec![HookPoint::OnRunEnd],
            "exit 1",
            false,
            sandbox,
            empty_policy_gate(),
        );
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
        let hook = CommandHook::new(
            vec![HookPoint::OnRunStart],
            "touch ran.txt",
            true,
            sandbox,
            empty_policy_gate(),
        );
        let dir = TempDir::new().unwrap();
        hook.run(&ctx_in(HookPoint::OnRunStart, dir.path()))
            .await
            .unwrap();
        assert!(
            dir.path().join("ran.txt").exists(),
            "the command runs with repo_path as its cwd"
        );
    }

    #[tokio::test]
    async fn command_hook_is_blocked_by_a_policy_deny_and_never_runs_the_command() {
        let sandbox = Arc::new(LocalSandbox::new());
        let rules =
            warden_policy::RuleSet::from_yaml("rules:\n  - action: shell\n    deny: [\"touch\"]\n")
                .unwrap();
        let policy_gate = Arc::new(PolicyGate::new(warden_policy::Evaluator::new(rules)));
        let hook = CommandHook::new(
            vec![HookPoint::OnRunStart],
            "touch denied.txt",
            true,
            sandbox,
            policy_gate,
        );
        let dir = TempDir::new().unwrap();
        match hook
            .run(&ctx_in(HookPoint::OnRunStart, dir.path()))
            .await
            .unwrap()
        {
            HookOutcome::Block { reason } => {
                assert!(reason.contains("touch"), "{reason}");
            }
            other => panic!("expected Block, got {other:?}"),
        }
        assert!(
            !dir.path().join("denied.txt").exists(),
            "a denied command must never actually run"
        );
    }

    #[tokio::test]
    async fn a_policy_denied_on_run_start_hook_still_blocks_via_the_existing_dispatch() {
        let sandbox = Arc::new(LocalSandbox::new());
        let rules =
            warden_policy::RuleSet::from_yaml("rules:\n  - action: shell\n    deny: [\"boom\"]\n")
                .unwrap();
        let policy_gate = Arc::new(PolicyGate::new(warden_policy::Evaluator::new(rules)));
        let hook = CommandHook::new(
            vec![HookPoint::OnRunStart],
            "echo boom",
            true,
            sandbox,
            policy_gate,
        );
        let mut registry = HookRegistry::new();
        registry.register(Arc::new(hook));
        let dir = TempDir::new().unwrap();
        let outcome = registry
            .run_hooks(
                HookPoint::OnRunStart,
                &ctx_in(HookPoint::OnRunStart, dir.path()),
            )
            .await
            .unwrap();
        assert!(matches!(outcome, HookOutcome::Block { .. }));
    }
}
