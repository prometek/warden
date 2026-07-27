//! Declarative rules (issue #51, ADR-0016): the `.warden/policy.yaml` shape
//! and the [`Action`] vocabulary [`crate::Evaluator`] evaluates against it.
//!
//! ```yaml
//! rules:
//!   - action: git_push
//!     branch: main
//!     require: [tests, review]
//!   - action: shell
//!     deny: ["rm -rf /"]
//! ```
//!
//! Parsing only -- no filesystem access here (mirrors
//! `warden_core::workflow::Workflow::parse_yaml`'s own "pure parse, caller
//! does I/O" split). Reading `.warden/policy.yaml` off disk, and deciding
//! that an *absent* file means "no rules" while a *malformed* one is a hard
//! error, is `warden::policy_config`'s job (I/O lives in the `warden` crate,
//! never here or in `warden-core`).

use serde::Deserialize;

use crate::error::{PolicyError, Result};

/// One action `warden` is about to take, or is about to let an agent/hook
/// take, that this crate knows how to govern. Closed on purpose (issue #51
/// is a minimal foundation, not an exhaustive action vocabulary, per the
/// issue's own "pas de bibliothèque exhaustive" scope) -- a new kind is a
/// new variant here plus a new arm in [`Rule::matches`]/[`Rule::decide`],
/// never a free-form string a rule could silently fail to match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// An attempt to push a run's commit to `branch`. In this codebase that
    /// is `warden` staging a converged run's commit into the local bare gate
    /// repo (ADR-0002) -- never a push straight to `origin`, which stays
    /// `warden-gated`'s exclusive, independently re-verified job
    /// (ADR-0002/0006, ADR-0016's own boundary). Evaluating this action lets
    /// an operator require approval, or forbid it outright, for specific
    /// branches before `warden` even stages that push.
    GitPush { branch: String },
    /// A shell command `warden` is about to run on an agent's/hook's behalf
    /// -- today, a `.warden/hooks.toml` `CommandHook`'s own `run` line
    /// (`warden::hook::CommandHook`).
    Shell { command: String },
}

/// One `- action: ...` entry of `.warden/policy.yaml`.
///
/// Every field beyond `action` is optional and interpreted according to
/// which [`Action`] kind it is matched against -- `branch` only means
/// anything for `git_push`, `deny` only for `shell` today (see
/// [`Rule::decide`]). Kept as a single flat shape (rather than an
/// action-keyed enum of rule bodies) because that is exactly the shape the
/// ticket's own example uses, and it keeps `.warden/policy.yaml` readable
/// without a tagged-union YAML encoding.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// `"git_push"` or `"shell"` -- validated once, at
    /// [`RuleSet::from_yaml`] time (see [`PolicyError::UnknownAction`]),
    /// never re-validated per evaluation.
    pub action: String,
    /// `git_push` only: restricts this rule to pushes targeting exactly this
    /// branch. Absent means "every branch".
    #[serde(default)]
    pub branch: Option<String>,
    /// `git_push` only: a non-empty list means this rule's matching pushes
    /// require human approval before proceeding (the list itself -- e.g.
    /// `[tests, review]` -- is carried into [`Decision::RequireApproval`]'s
    /// `reason` for the human approving to read, not separately enforced by
    /// this crate; issue #51 is a minimal foundation, not a checklist
    /// engine).
    #[serde(default)]
    pub require: Vec<String>,
    /// A non-empty list of literal substrings that, if any matches, deny
    /// this rule's action outright. For `shell`, matched against the
    /// command line; for `git_push`, matched against the branch name (e.g.
    /// `deny: ["release/"]` bans pushing any `release/*` branch entirely,
    /// distinct from `require`, which still allows the push once approved).
    /// Deliberately simple substring containment, not a glob/regex engine --
    /// "socle minimal d'abord" (issue #51's own scope).
    #[serde(default)]
    pub deny: Vec<String>,
}

impl Rule {
    /// Whether this rule applies to `action` at all.
    fn matches(&self, action: &Action) -> bool {
        match action {
            Action::GitPush { branch } => {
                self.action == "git_push" && self.branch.as_deref().is_none_or(|b| b == branch)
            }
            Action::Shell { .. } => self.action == "shell",
        }
    }

    /// The [`crate::Decision`] this rule alone contributes for `action`,
    /// once [`Rule::matches`] is already known to be true. `deny` always
    /// takes priority over `require` within a single rule: a branch/command
    /// that is both denied and gated by `require` is still denied outright
    /// -- `require` only ever softens "forbidden" into "needs a human", it
    /// never overrides an explicit `deny`.
    fn decide(&self, action: &Action) -> crate::Decision {
        match action {
            Action::Shell { command } => {
                match self
                    .deny
                    .iter()
                    .find(|pattern| command.contains(pattern.as_str()))
                {
                    Some(pattern) => crate::Decision::Deny {
                        reason: format!(
                            "shell command matches denied pattern {pattern:?}: {command}"
                        ),
                    },
                    None => crate::Decision::Allow,
                }
            }
            Action::GitPush { branch } => {
                if let Some(pattern) = self
                    .deny
                    .iter()
                    .find(|pattern| branch.contains(pattern.as_str()))
                {
                    crate::Decision::Deny {
                        reason: format!(
                            "push to branch {branch:?} matches denied pattern {pattern:?}"
                        ),
                    }
                } else if self.require.is_empty() {
                    crate::Decision::Allow
                } else {
                    crate::Decision::RequireApproval {
                        reason: format!(
                            "push to branch {branch:?} requires: {}",
                            self.require.join(", ")
                        ),
                    }
                }
            }
        }
    }
}

/// The parsed, validated contents of `.warden/policy.yaml`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleSet {
    #[serde(default)]
    pub rules: Vec<Rule>,
}

impl RuleSet {
    /// A rule set with no rules -- [`crate::Evaluator::evaluate`] returns
    /// [`crate::Decision::Allow`] for every action against this, which is
    /// what an absent `.warden/policy.yaml` (`warden::policy_config`'s own
    /// "no file -> no rules" convention) and [`crate::Evaluator::empty`] both
    /// resolve to. A strict no-op, the same "empty registry -> no-op"
    /// contract `warden_core::HookRegistry::new` already established.
    pub fn empty() -> Self {
        Self { rules: Vec::new() }
    }

    /// Parses and validates a `.warden/policy.yaml` document. Every failure
    /// is a [`PolicyError`] naming *what* is wrong (malformed YAML, an
    /// unknown top-level key, an unknown `action`) -- the caller
    /// (`warden::policy_config`, which reads the file) names *which* file,
    /// never silently falling back to [`RuleSet::empty`] on a parse failure
    /// (code-standards.md: no silent fallback -- a malformed policy file
    /// must fail the run, not quietly run with no governance at all).
    pub fn from_yaml(raw: &str) -> Result<Self> {
        let parsed: RuleSet = serde_yaml::from_str(raw)
            .map_err(|error| PolicyError::InvalidYaml(error.to_string()))?;

        for (index, rule) in parsed.rules.iter().enumerate() {
            match rule.action.as_str() {
                "git_push" | "shell" => {}
                other => {
                    return Err(PolicyError::UnknownAction {
                        index,
                        action: other.to_string(),
                    })
                }
            }
        }

        Ok(parsed)
    }

    /// Every rule matching `action`, in file order -- [`crate::Evaluator`]'s
    /// own iteration point, kept here so [`Rule::matches`] stays private to
    /// this module.
    pub(crate) fn matching<'a>(&'a self, action: &'a Action) -> impl Iterator<Item = &'a Rule> {
        self.rules.iter().filter(move |rule| rule.matches(action))
    }
}

/// Re-exposed for [`crate::Evaluator`] -- kept `pub(crate)` rather than
/// `pub` since a caller outside this crate has no business calling a single
/// rule's own decision without going through the escalation
/// [`crate::Evaluator::evaluate`] applies across every matching rule.
pub(crate) fn decide_one(rule: &Rule, action: &Action) -> crate::Decision {
    rule.decide(action)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Decision;

    #[test]
    fn parses_the_ticket_example() {
        let yaml = r#"
rules:
  - action: git_push
    branch: main
    require: [tests, review]
  - action: shell
    deny: ["rm -rf /"]
"#;
        let rules = RuleSet::from_yaml(yaml).unwrap();
        assert_eq!(rules.rules.len(), 2);
        assert_eq!(rules.rules[0].action, "git_push");
        assert_eq!(rules.rules[0].branch.as_deref(), Some("main"));
        assert_eq!(rules.rules[0].require, vec!["tests", "review"]);
        assert_eq!(rules.rules[1].action, "shell");
        assert_eq!(rules.rules[1].deny, vec!["rm -rf /"]);
    }

    #[test]
    fn an_absent_or_empty_document_yields_an_empty_rule_set() {
        assert_eq!(RuleSet::from_yaml("rules: []").unwrap(), RuleSet::empty());
    }

    #[test]
    fn rejects_malformed_yaml() {
        let error = RuleSet::from_yaml("not: valid: yaml: at: all: [").unwrap_err();
        assert!(matches!(error, PolicyError::InvalidYaml(_)));
    }

    #[test]
    fn rejects_an_unknown_top_level_key() {
        let error = RuleSet::from_yaml("rules: []\nextra: true\n").unwrap_err();
        assert!(matches!(error, PolicyError::InvalidYaml(_)));
    }

    #[test]
    fn rejects_a_rule_with_an_unknown_action() {
        let yaml = "rules:\n  - action: launch_missiles\n";
        let error = RuleSet::from_yaml(yaml).unwrap_err();
        match error {
            PolicyError::UnknownAction { index, action } => {
                assert_eq!(index, 0);
                assert_eq!(action, "launch_missiles");
            }
            other => panic!("expected UnknownAction, got {other:?}"),
        }
    }

    #[test]
    fn rejects_a_rule_with_an_unknown_field() {
        let yaml = "rules:\n  - action: shell\n    typo_field: nope\n";
        let error = RuleSet::from_yaml(yaml).unwrap_err();
        assert!(matches!(error, PolicyError::InvalidYaml(_)));
    }

    #[test]
    fn a_git_push_rule_with_no_branch_matches_every_branch() {
        let rules =
            RuleSet::from_yaml("rules:\n  - action: git_push\n    require: [tests]\n").unwrap();
        let rule = &rules.rules[0];
        assert!(rule.matches(&Action::GitPush {
            branch: "main".to_string()
        }));
        assert!(rule.matches(&Action::GitPush {
            branch: "feature/x".to_string()
        }));
    }

    #[test]
    fn a_git_push_rule_with_a_branch_only_matches_that_branch() {
        let rules = RuleSet::from_yaml(
            "rules:\n  - action: git_push\n    branch: main\n    require: [tests]\n",
        )
        .unwrap();
        let rule = &rules.rules[0];
        assert!(rule.matches(&Action::GitPush {
            branch: "main".to_string()
        }));
        assert!(!rule.matches(&Action::GitPush {
            branch: "develop".to_string()
        }));
    }

    #[test]
    fn a_shell_rule_never_matches_a_git_push_action_and_vice_versa() {
        let rules = RuleSet::from_yaml(
            "rules:\n  - action: shell\n    deny: [\"rm -rf /\"]\n  - action: git_push\n    require: [tests]\n",
        )
        .unwrap();
        let (shell_rule, push_rule) = (&rules.rules[0], &rules.rules[1]);
        assert!(shell_rule.matches(&Action::Shell {
            command: "rm -rf /".to_string()
        }));
        assert!(!shell_rule.matches(&Action::GitPush {
            branch: "main".to_string()
        }));
        assert!(push_rule.matches(&Action::GitPush {
            branch: "main".to_string()
        }));
        assert!(!push_rule.matches(&Action::Shell {
            command: "rm -rf /".to_string()
        }));
    }

    #[test]
    fn a_shell_rule_denies_only_a_command_containing_the_pattern() {
        let rules =
            RuleSet::from_yaml("rules:\n  - action: shell\n    deny: [\"rm -rf /\"]\n").unwrap();
        let rule = &rules.rules[0];
        assert_eq!(
            decide_one(
                rule,
                &Action::Shell {
                    command: "rm -rf /".to_string()
                }
            ),
            Decision::Deny {
                reason: "shell command matches denied pattern \"rm -rf /\": rm -rf /".to_string()
            }
        );
        assert_eq!(
            decide_one(
                rule,
                &Action::Shell {
                    command: "cargo test".to_string()
                }
            ),
            Decision::Allow
        );
    }

    #[test]
    fn a_git_push_rule_with_require_yields_require_approval() {
        let rules =
            RuleSet::from_yaml("rules:\n  - action: git_push\n    require: [tests, review]\n")
                .unwrap();
        let rule = &rules.rules[0];
        assert_eq!(
            decide_one(
                rule,
                &Action::GitPush {
                    branch: "main".to_string()
                }
            ),
            Decision::RequireApproval {
                reason: "push to branch \"main\" requires: tests, review".to_string()
            }
        );
    }

    #[test]
    fn a_git_push_rule_with_no_require_allows() {
        let rules = RuleSet::from_yaml("rules:\n  - action: git_push\n    branch: main\n").unwrap();
        let rule = &rules.rules[0];
        assert_eq!(
            decide_one(
                rule,
                &Action::GitPush {
                    branch: "main".to_string()
                }
            ),
            Decision::Allow
        );
    }

    #[test]
    fn a_git_push_rule_denies_a_branch_matching_its_deny_pattern() {
        let rules =
            RuleSet::from_yaml("rules:\n  - action: git_push\n    deny: [\"release/\"]\n").unwrap();
        let rule = &rules.rules[0];
        assert_eq!(
            decide_one(
                rule,
                &Action::GitPush {
                    branch: "release/1.0".to_string()
                }
            ),
            Decision::Deny {
                reason: "push to branch \"release/1.0\" matches denied pattern \"release/\""
                    .to_string()
            }
        );
        assert_eq!(
            decide_one(
                rule,
                &Action::GitPush {
                    branch: "main".to_string()
                }
            ),
            Decision::Allow
        );
    }

    #[test]
    fn deny_takes_priority_over_require_within_the_same_git_push_rule() {
        let rules = RuleSet::from_yaml(
            "rules:\n  - action: git_push\n    branch: main\n    deny: [\"main\"]\n    require: [tests]\n",
        )
        .unwrap();
        let rule = &rules.rules[0];
        assert_eq!(
            decide_one(
                rule,
                &Action::GitPush {
                    branch: "main".to_string()
                }
            ),
            Decision::Deny {
                reason: "push to branch \"main\" matches denied pattern \"main\"".to_string()
            }
        );
    }
}
