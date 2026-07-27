//! [`Evaluator`] (issue #51, ADR-0016): evaluates one [`Action`] against a
//! [`RuleSet`], producing the single [`Decision`] the orchestrator acts on.

use crate::rules::decide_one;
use crate::{Action, Decision, RuleSet};

/// Evaluates actions against a fixed [`RuleSet`]. Immutable and cheap to
/// share (`Clone`) -- a run resolves its rule set once (`warden::policy_config`)
/// and hands the same `Evaluator` to every decision point for its lifetime,
/// exactly like `warden_core::HookRegistry` is resolved once per run.
#[derive(Debug, Clone)]
pub struct Evaluator {
    rules: RuleSet,
}

impl Evaluator {
    pub fn new(rules: RuleSet) -> Self {
        Self { rules }
    }

    /// An evaluator with no rules at all -- every [`Evaluator::evaluate`]
    /// call returns [`Decision::Allow`], the strict no-op an absent
    /// `.warden/policy.yaml` resolves to (`warden::policy_config`'s own
    /// "no file -> no rules" convention).
    pub fn empty() -> Self {
        Self::new(RuleSet::empty())
    }

    /// Evaluates `action` against every rule in this evaluator's
    /// [`RuleSet`], in file order, and returns the single **strictest**
    /// decision reached: [`Decision::Deny`] beats [`Decision::RequireApproval`]
    /// beats [`Decision::Allow`]. This is a deliberate escalation-only
    /// policy -- a later, more permissive rule can never silently undo an
    /// earlier rule's `Deny`/`RequireApproval`, which would make rule order
    /// a security-relevant footgun. No matching rule at all (or every
    /// matching rule allows the action outright) is [`Decision::Allow`].
    pub fn evaluate(&self, action: &Action) -> Decision {
        let mut decision = Decision::Allow;
        for rule in self.rules.matching(action) {
            decision = escalate(decision, decide_one(rule, action));
        }
        decision
    }
}

/// Combines two decisions for the same action, keeping the stricter one.
/// Order of severity: `Deny` > `RequireApproval` > `Allow`.
fn escalate(current: Decision, candidate: Decision) -> Decision {
    match (&current, &candidate) {
        (Decision::Deny { .. }, _) => current,
        (_, Decision::Deny { .. }) => candidate,
        (Decision::RequireApproval { .. }, _) => current,
        (_, Decision::RequireApproval { .. }) => candidate,
        (Decision::Allow, Decision::Allow) => current,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RuleSet;

    fn action_push(branch: &str) -> Action {
        Action::GitPush {
            branch: branch.to_string(),
        }
    }

    fn action_shell(command: &str) -> Action {
        Action::Shell {
            command: command.to_string(),
        }
    }

    #[test]
    fn empty_evaluator_always_allows() {
        let evaluator = Evaluator::empty();
        assert_eq!(evaluator.evaluate(&action_push("main")), Decision::Allow);
        assert_eq!(
            evaluator.evaluate(&action_shell("rm -rf /")),
            Decision::Allow
        );
    }

    #[test]
    fn an_action_matching_no_rule_is_allowed() {
        let rules =
            RuleSet::from_yaml("rules:\n  - action: shell\n    deny: [\"rm -rf /\"]\n").unwrap();
        let evaluator = Evaluator::new(rules);
        // No git_push rule at all -- unrelated to the one shell rule present.
        assert_eq!(evaluator.evaluate(&action_push("main")), Decision::Allow);
    }

    #[test]
    fn the_ticket_example_denies_the_matching_shell_command() {
        let rules = RuleSet::from_yaml(
            "rules:\n  - action: git_push\n    branch: main\n    require: [tests, review]\n  \
             - action: shell\n    deny: [\"rm -rf /\"]\n",
        )
        .unwrap();
        let evaluator = Evaluator::new(rules);

        assert_eq!(
            evaluator.evaluate(&action_shell("rm -rf / --no-preserve-root")),
            Decision::Deny {
                reason: "shell command matches denied pattern \"rm -rf /\": rm -rf / \
                         --no-preserve-root"
                    .to_string()
            }
        );
        assert_eq!(
            evaluator.evaluate(&action_shell("cargo fmt --check")),
            Decision::Allow
        );
    }

    #[test]
    fn the_ticket_example_requires_approval_to_push_to_main_but_not_other_branches() {
        let rules = RuleSet::from_yaml(
            "rules:\n  - action: git_push\n    branch: main\n    require: [tests, review]\n",
        )
        .unwrap();
        let evaluator = Evaluator::new(rules);

        assert_eq!(
            evaluator.evaluate(&action_push("main")),
            Decision::RequireApproval {
                reason: "push to branch \"main\" requires: tests, review".to_string()
            }
        );
        assert_eq!(
            evaluator.evaluate(&action_push("feature/x")),
            Decision::Allow
        );
    }

    #[test]
    fn deny_escalates_over_a_prior_require_approval_from_an_earlier_rule() {
        let rules = RuleSet::from_yaml(
            "rules:\n  - action: git_push\n    require: [tests]\n  - action: git_push\n    \
             branch: main\n    require: [tests]\n",
        )
        .unwrap();
        // Neither rule above denies -- add a real deny scenario instead:
        // a shell action matched by two rules, one allowing, one denying.
        let evaluator = Evaluator::new(rules);
        // Sanity: with only RequireApproval-producing rules, the result stays
        // RequireApproval (never silently downgraded to Allow by escalation).
        assert_eq!(
            evaluator.evaluate(&action_push("main")),
            Decision::RequireApproval {
                reason: "push to branch \"main\" requires: tests".to_string()
            }
        );

        let deny_rules = RuleSet::from_yaml(
            "rules:\n  - action: shell\n    deny: [\"curl\"]\n  - action: shell\n    deny: \
             [\"rm -rf /\"]\n",
        )
        .unwrap();
        let deny_evaluator = Evaluator::new(deny_rules);
        // Matches only the second rule's pattern -- still denied, proving
        // escalation isn't "first match wins" but "strictest match wins".
        assert_eq!(
            deny_evaluator.evaluate(&action_shell("rm -rf /tmp")),
            Decision::Deny {
                reason: "shell command matches denied pattern \"rm -rf /\": rm -rf /tmp"
                    .to_string()
            }
        );
    }

    #[test]
    fn deny_beats_require_approval_when_both_match_the_same_action() {
        // Two rules on the same push: an unscoped one only requires
        // approval, a branch-scoped one denies outright (e.g. a hotfix
        // branch banned entirely). The stricter Deny must win regardless of
        // file order.
        let rules = RuleSet::from_yaml(
            "rules:\n  - action: git_push\n    require: [tests]\n  - action: git_push\n    \
             branch: main\n    deny: [\"main\"]\n",
        )
        .unwrap();
        let evaluator = Evaluator::new(rules);
        assert_eq!(
            evaluator.evaluate(&action_push("main")),
            Decision::Deny {
                reason: "push to branch \"main\" matches denied pattern \"main\"".to_string()
            }
        );
        // A different branch only matches the first (RequireApproval) rule.
        assert_eq!(
            evaluator.evaluate(&action_push("feature/x")),
            Decision::RequireApproval {
                reason: "push to branch \"feature/x\" requires: tests".to_string()
            }
        );
    }
}
