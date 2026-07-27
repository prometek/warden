//! The I/O-bearing half of `warden-policy`'s wiring (issue #51, ADR-0016):
//! resolves a [`warden_policy::Decision`] into something an orchestrator
//! decision point can act on, suspending at a human-validation wait point
//! for [`warden_policy::Decision::RequireApproval`].
//!
//! [`warden_policy`] itself never does I/O and never suspends -- it only
//! evaluates rules. This module is where that pure verdict meets the run's
//! actual lifecycle: [`PolicyGate::decide`] `.await`s an [`ApprovalGate`]
//! when a decision needs one, which is the "wait point" ADR-0016 calls for.
//! Two call sites use it today (see each's own docs):
//! - `crate::hook::CommandHook::run` -- a `.warden/hooks.toml` shell command,
//!   evaluated as [`warden_policy::Action::Shell`].
//! - `crate::orchestrator::gate_tail` -- the push into the local bare gate
//!   repo (never `origin` itself), evaluated as [`warden_policy::Action::GitPush`].

use std::sync::Arc;

use async_trait::async_trait;
use warden_policy::{Action, Decision, Evaluator};

/// What is presented to a human when a [`Decision::RequireApproval`] needs
/// their sign-off before Warden proceeds.
#[derive(Debug, Clone, Copy)]
pub struct ApprovalRequest<'a> {
    pub run_id: &'a str,
    /// Human-readable description of the action awaiting approval, e.g.
    /// `"git_push to branch \"main\""` or `"shell: cargo publish"`.
    pub description: &'a str,
    /// The matching rule's own reason (`Decision::RequireApproval`'s
    /// `reason` field) -- e.g. `"push to branch \"main\" requires: tests,
    /// review"`.
    pub reason: &'a str,
}

/// A human-validation wait point (ADR-0016): resolves one
/// [`ApprovalRequest`] to `true` (approved, the action may proceed) or
/// `false` (denied, the action is blocked exactly like a
/// [`Decision::Deny`]). Implemented outside this crate's pure lib code path
/// wherever the concrete approval channel needs to write to a terminal/UI --
/// code-standards.md's "the lib emits tracing spans/events... it never
/// writes to stdout/stderr directly" applies here exactly as it does to
/// [`crate::orchestrator::Orchestrator`]'s own `on_run_started` callback: a
/// real interactive implementation belongs in `main.rs`, this trait only
/// names the seam.
#[async_trait]
pub trait ApprovalGate: Send + Sync {
    async fn approve(&self, request: ApprovalRequest<'_>) -> bool;
}

/// What [`PolicyGate::decide`] resolved a [`Decision`] to, for a caller to
/// act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyOutcome {
    /// The action may proceed.
    Allowed,
    /// The action must not proceed. `reason` is always actionable (why it
    /// was denied, or why an approval was refused/unavailable).
    Blocked { reason: String },
}

/// Combines a [`warden_policy::Evaluator`] with an optional [`ApprovalGate`]
/// into the single seam every decision point in `warden` goes through.
/// Cloned cheaply (`Arc` internals) -- one instance is resolved per run
/// (`main.rs`) and shared between the orchestrator's own push check and
/// every `.warden/hooks.toml` `CommandHook`.
#[derive(Clone)]
pub struct PolicyGate {
    evaluator: Arc<Evaluator>,
    /// `None` by default: no interactive approval channel configured. A
    /// [`Decision::RequireApproval`] with no gate configured is **denied**
    /// (fail-closed, code-standards.md's "no silent fallback") rather than
    /// either silently allowed or left hanging forever.
    approval_gate: Option<Arc<dyn ApprovalGate>>,
}

impl std::fmt::Debug for PolicyGate {
    /// `dyn ApprovalGate` is not `Debug`; whether one is configured at all
    /// is the only externally interesting property.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PolicyGate")
            .field("approval_gate_configured", &self.approval_gate.is_some())
            .finish()
    }
}

impl PolicyGate {
    pub fn new(evaluator: Evaluator) -> Self {
        Self {
            evaluator: Arc::new(evaluator),
            approval_gate: None,
        }
    }

    /// A gate with no rules and no approval channel -- every action is
    /// [`PolicyOutcome::Allowed`]. The default every [`crate::orchestrator::Orchestrator`]
    /// and [`crate::hook_config::load_repo_hooks`] caller gets absent an
    /// explicit `.warden/policy.yaml` (`crate::policy_config`'s own "no file
    /// -> no rules" convention), a strict no-op exactly like
    /// `warden_core::HookRegistry::new`'s empty registry.
    pub fn empty() -> Self {
        Self::new(Evaluator::empty())
    }

    /// Installs `gate` as the human-validation wait point a
    /// [`Decision::RequireApproval`] suspends on.
    pub fn with_approval_gate(mut self, gate: Arc<dyn ApprovalGate>) -> Self {
        self.approval_gate = Some(gate);
        self
    }

    /// Evaluates `action` and resolves it to a [`PolicyOutcome`] a caller can
    /// act on directly, suspending on the configured [`ApprovalGate`] (the
    /// "wait point") when the decision is [`Decision::RequireApproval`].
    pub async fn decide(&self, run_id: &str, description: &str, action: &Action) -> PolicyOutcome {
        match self.evaluator.evaluate(action) {
            Decision::Allow => PolicyOutcome::Allowed,
            Decision::Deny { reason } => {
                tracing::warn!(run_id, description, reason, "policy denied action");
                PolicyOutcome::Blocked { reason }
            }
            Decision::RequireApproval { reason } => match &self.approval_gate {
                Some(gate) => {
                    tracing::info!(
                        run_id,
                        description,
                        reason,
                        "action requires human approval; suspending"
                    );
                    let approved = gate
                        .approve(ApprovalRequest {
                            run_id,
                            description,
                            reason: &reason,
                        })
                        .await;
                    if approved {
                        tracing::info!(run_id, description, "action approved by human");
                        PolicyOutcome::Allowed
                    } else {
                        tracing::warn!(run_id, description, "action denied by human");
                        PolicyOutcome::Blocked {
                            reason: format!("human approval denied: {reason}"),
                        }
                    }
                }
                None => {
                    tracing::warn!(
                        run_id,
                        description,
                        reason,
                        "action requires human approval but no approval gate is configured; \
                         denying (fail-closed)"
                    );
                    PolicyOutcome::Blocked {
                        reason: format!(
                            "requires human approval but no approval gate is configured: {reason}"
                        ),
                    }
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use warden_policy::RuleSet;

    struct FakeApprovalGate {
        approve: bool,
    }

    #[async_trait]
    impl ApprovalGate for FakeApprovalGate {
        async fn approve(&self, _request: ApprovalRequest<'_>) -> bool {
            self.approve
        }
    }

    fn push_action(branch: &str) -> Action {
        Action::GitPush {
            branch: branch.to_string(),
        }
    }

    #[tokio::test]
    async fn an_empty_gate_allows_everything() {
        let gate = PolicyGate::empty();
        assert_eq!(
            gate.decide("run-1", "git_push to main", &push_action("main"))
                .await,
            PolicyOutcome::Allowed
        );
    }

    #[tokio::test]
    async fn a_denied_action_is_blocked_with_the_rule_reason() {
        let rules =
            RuleSet::from_yaml("rules:\n  - action: shell\n    deny: [\"rm -rf /\"]\n").unwrap();
        let gate = PolicyGate::new(Evaluator::new(rules));
        let outcome = gate
            .decide(
                "run-1",
                "shell: rm -rf /",
                &Action::Shell {
                    command: "rm -rf /".to_string(),
                },
            )
            .await;
        match outcome {
            PolicyOutcome::Blocked { reason } => {
                assert!(reason.contains("rm -rf /"), "{reason}");
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn require_approval_with_no_gate_configured_is_blocked_fail_closed() {
        let rules =
            RuleSet::from_yaml("rules:\n  - action: git_push\n    require: [tests]\n").unwrap();
        let gate = PolicyGate::new(Evaluator::new(rules));
        let outcome = gate
            .decide("run-1", "git_push to main", &push_action("main"))
            .await;
        match outcome {
            PolicyOutcome::Blocked { reason } => {
                assert!(reason.contains("no approval gate"), "{reason}");
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn require_approval_proceeds_once_the_configured_gate_approves() {
        let rules =
            RuleSet::from_yaml("rules:\n  - action: git_push\n    require: [tests]\n").unwrap();
        let gate = PolicyGate::new(Evaluator::new(rules))
            .with_approval_gate(Arc::new(FakeApprovalGate { approve: true }));
        let outcome = gate
            .decide("run-1", "git_push to main", &push_action("main"))
            .await;
        assert_eq!(outcome, PolicyOutcome::Allowed);
    }

    #[tokio::test]
    async fn require_approval_is_blocked_when_the_configured_gate_denies() {
        let rules =
            RuleSet::from_yaml("rules:\n  - action: git_push\n    require: [tests]\n").unwrap();
        let gate = PolicyGate::new(Evaluator::new(rules))
            .with_approval_gate(Arc::new(FakeApprovalGate { approve: false }));
        let outcome = gate
            .decide("run-1", "git_push to main", &push_action("main"))
            .await;
        match outcome {
            PolicyOutcome::Blocked { reason } => {
                assert!(reason.contains("human approval denied"), "{reason}");
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }
}
