use std::sync::Arc;

use async_trait::async_trait;
use warden_policy::{Action, Decision, Evaluator};

/// What is presented to a human when a [`Decision::RequireApproval`] needs their sign-off before
/// Warden proceeds.
#[derive(Debug, Clone, Copy)]
pub struct ApprovalRequest<'a> {
    pub run_id: &'a str,
    pub description: &'a str,
    pub reason: &'a str,
}

#[async_trait]
pub trait ApprovalGate: Send + Sync {
    async fn approve(&self, request: ApprovalRequest<'_>) -> bool;
}

/// What [`PolicyGate::decide`] resolved a [`Decision`] to, for a caller to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyOutcome {
    /// The action may proceed.
    Allowed,
    /// The action must not proceed. `reason` is always actionable (why it was denied, or why an
    /// approval was refused/unavailable).
    Blocked { reason: String },
}

/// Combines a [`warden_policy::Evaluator`] with an optional [`ApprovalGate`] into the single seam
/// every decision point in `warden` goes through.
#[derive(Clone)]
pub struct PolicyGate {
    evaluator: Arc<Evaluator>,
    /// `None` by default: no interactive approval channel configured.
    approval_gate: Option<Arc<dyn ApprovalGate>>,
}

impl std::fmt::Debug for PolicyGate {
    /// `dyn ApprovalGate` is not `Debug`; whether one is configured at all is the only externally
    /// interesting property.
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

    /// A gate with no rules and no approval channel -- every action is [`PolicyOutcome::Allowed`].
    pub fn empty() -> Self {
        Self::new(Evaluator::empty())
    }

    /// Installs `gate` as the human-validation wait point a [`Decision::RequireApproval`] suspends
    /// on.
    pub fn with_approval_gate(mut self, gate: Arc<dyn ApprovalGate>) -> Self {
        self.approval_gate = Some(gate);
        self
    }

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
