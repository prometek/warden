//! [`Evaluator`]: evaluates one [`Action`] against a [`RuleSet`], producing the single [`Decision`]
//! the orchestrator acts on.

use crate::{Action, Decision, RuleSet};

#[derive(Debug, Clone)]
pub struct Evaluator {
    rules: RuleSet,
}

impl Evaluator {
    pub fn new(rules: RuleSet) -> Self {
        Self { rules }
    }

    pub fn empty() -> Self {
        Self::new(RuleSet::empty())
    }

    /// Evaluates `action` against every rule in this evaluator's [`RuleSet`], in file order, and
    /// returns the single **strictest** decision reached.
    pub fn evaluate(&self, action: &Action) -> Decision {
        let mut decision = Decision::Allow;
        for rule in self.rules.matching(action) {
            decision = escalate(decision, rule.decide(action));
        }
        decision
    }
}

/// Combines two decisions for the same action, keeping the stricter one.
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
        assert_eq!(
            evaluator.evaluate(&action_push("feature/x")),
            Decision::RequireApproval {
                reason: "push to branch \"feature/x\" requires: tests".to_string()
            }
        );
    }

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
