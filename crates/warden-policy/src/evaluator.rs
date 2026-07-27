//! [`Evaluator`] (issue #51, ADR-0016): evaluates one [`Action`] against a
//! [`RuleSet`], producing the single [`Decision`] the orchestrator acts on.

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
            decision = escalate(decision, rule.decide(action));
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

    /// Two rules both matching the same push, both `RequireApproval` -- the
    /// aggregate must stay `RequireApproval`, never silently collapse to
    /// `Allow` just because neither individually denies.
    #[test]
    fn two_require_approval_rules_matching_the_same_push_stay_require_approval() {
        let rules = RuleSet::from_yaml(
            "rules:\n  - action: git_push\n    require: [tests]\n  - action: git_push\n    \
             branch: main\n    require: [review]\n",
        )
        .unwrap();
        let evaluator = Evaluator::new(rules);
        assert_eq!(
            evaluator.evaluate(&action_push("main")),
            Decision::RequireApproval {
                reason: "push to branch \"main\" requires: tests".to_string()
            },
            "the first matching rule's own decision wins the escalation tie"
        );
    }

    /// Two `shell` rules with different patterns, only the second matching --
    /// proves escalation aggregates across every matching rule ("strictest
    /// match wins"), not just the first one in file order.
    #[test]
    fn a_deny_rule_further_down_the_file_still_denies() {
        let rules = RuleSet::from_yaml(
            "rules:\n  - action: shell\n    deny: [\"curl\"]\n  - action: shell\n    deny: \
             [\"rm -rf /\"]\n",
        )
        .unwrap();
        let evaluator = Evaluator::new(rules);
        assert_eq!(
            evaluator.evaluate(&action_shell("rm -rf /tmp")),
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
             branch: main\n    deny: [main]\n",
        )
        .unwrap();
        let evaluator = Evaluator::new(rules);
        assert_eq!(
            evaluator.evaluate(&action_push("main")),
            Decision::Deny {
                reason: "push to branch \"main\" matches denied branch \"main\"".to_string()
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

    /// Two byte-for-byte identical `deny` rules matching the same action --
    /// a duplicate a hand-edited or generated `.warden/policy.yaml` could
    /// plausibly contain. The duplicate must not change the outcome (still a
    /// single `Deny`, not doubled, aggregated, or rejected as invalid): a
    /// `RuleSet` has no uniqueness constraint on its own, by design (issue
    /// #51 is a minimal foundation, not a validating registry beyond the
    /// schema itself).
    #[test]
    fn a_duplicate_deny_rule_still_resolves_to_a_single_deny() {
        let rules = RuleSet::from_yaml(
            "rules:\n  - action: shell\n    deny: [\"rm -rf /\"]\n  - action: shell\n    deny: \
             [\"rm -rf /\"]\n",
        )
        .unwrap();
        let evaluator = Evaluator::new(rules);
        assert_eq!(
            evaluator.evaluate(&action_shell("rm -rf /")),
            Decision::Deny {
                reason: "shell command matches denied pattern \"rm -rf /\": rm -rf /".to_string()
            }
        );
    }

    /// The escalation reduction is order-independent for the strictest
    /// outcome reached (`Deny` always wins, regardless of where in the file
    /// it appears) -- proven here by evaluating the exact same three
    /// overlapping `git_push` rules in both file orders and asserting an
    /// identical `Decision` either way. Only the specific `Deny`/
    /// `RequireApproval` *reason string* surfaced may differ (whichever
    /// matching rule of that strictness the escalation walk reaches first),
    /// which is why this only compares the `Decision` variant's kind, not
    /// its `reason` payload.
    #[test]
    fn reordering_overlapping_git_push_rules_does_not_change_which_decision_kind_wins() {
        fn decision_kind(decision: &Decision) -> &'static str {
            match decision {
                Decision::Allow => "allow",
                Decision::Deny { .. } => "deny",
                Decision::RequireApproval { .. } => "require_approval",
            }
        }

        let forward = RuleSet::from_yaml(
            "rules:\n  - action: git_push\n    require: [tests]\n  - action: git_push\n    \
             branch: main\n    require: [review]\n  - action: git_push\n    branch: main\n    \
             deny: [main]\n",
        )
        .unwrap();
        let reversed = RuleSet::from_yaml(
            "rules:\n  - action: git_push\n    branch: main\n    deny: [main]\n  - action: \
             git_push\n    branch: main\n    require: [review]\n  - action: git_push\n    \
             require: [tests]\n",
        )
        .unwrap();

        for branch in ["main", "feature/x"] {
            let expected =
                decision_kind(&Evaluator::new(forward.clone()).evaluate(&action_push(branch)));
            let actual =
                decision_kind(&Evaluator::new(reversed.clone()).evaluate(&action_push(branch)));
            assert_eq!(
                expected, actual,
                "branch {branch:?}: rule order must not change the strictest decision reached"
            );
        }
        // Pin the actual decisions too, not just their equality across order.
        assert_eq!(
            Evaluator::new(forward.clone()).evaluate(&action_push("main")),
            Decision::Deny {
                reason: "push to branch \"main\" matches denied branch \"main\"".to_string()
            }
        );
        assert_eq!(
            Evaluator::new(forward).evaluate(&action_push("feature/x")),
            Decision::RequireApproval {
                reason: "push to branch \"feature/x\" requires: tests".to_string()
            }
        );
    }
}
