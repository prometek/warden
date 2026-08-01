//! Declarative rules: the `.warden/policy.yaml` shape and the [`Action`] vocabulary
//! [`crate::Evaluator`] evaluates against it.

use serde::Deserialize;

use crate::error::{PolicyError, Result};

/// One action `warden` is about to take, or is about to let an agent/hook take, that this crate
/// knows how to govern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    GitPush {
        branch: String,
    },
    /// A shell command `warden` is about to run on an agent's/hook's behalf -- today, a
    /// `.warden/hooks.toml` `CommandHook`'s own `run` line (`warden::hook::CommandHook`).
    Shell {
        command: String,
    },
}

/// One `- action:...` entry of `.warden/policy.yaml`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Rule {
    GitPush {
        /// Restricts this rule to pushes targeting exactly this branch (equality, not substring --
        /// same semantics as `deny` below).
        #[serde(default)]
        branch: Option<String>,
        /// A non-empty list means this rule's matching pushes require human approval before
        /// proceeding.
        #[serde(default)]
        require: Vec<String>,
        #[serde(default)]
        deny: Vec<String>,
    },
    Shell {
        /// A non-empty list of literal substrings that, if any is contained in the evaluated
        /// command, deny it outright.
        #[serde(default)]
        deny: Vec<String>,
    },
}

impl Rule {
    /// Whether this rule applies to `action` at all.
    fn matches(&self, action: &Action) -> bool {
        match (self, action) {
            (Rule::GitPush { branch, .. }, Action::GitPush { branch: pushed }) => {
                branch.as_deref().is_none_or(|b| b == pushed)
            }
            (Rule::Shell { .. }, Action::Shell { .. }) => true,
            (Rule::GitPush { .. }, Action::Shell { .. })
            | (Rule::Shell { .. }, Action::GitPush { .. }) => false,
        }
    }

    pub(crate) fn decide(&self, action: &Action) -> crate::Decision {
        match (self, action) {
            (Rule::Shell { deny }, Action::Shell { command }) => match deny
                .iter()
                .find(|pattern| command.contains(pattern.as_str()))
            {
                Some(pattern) => crate::Decision::Deny {
                    reason: format!("shell command matches denied pattern {pattern:?}: {command}"),
                },
                None => crate::Decision::Allow,
            },
            (Rule::GitPush { require, deny, .. }, Action::GitPush { branch }) => {
                if let Some(pattern) = deny.iter().find(|denied| denied.as_str() == branch) {
                    crate::Decision::Deny {
                        reason: format!(
                            "push to branch {branch:?} matches denied branch {pattern:?}"
                        ),
                    }
                } else if require.is_empty() {
                    crate::Decision::Allow
                } else {
                    crate::Decision::RequireApproval {
                        reason: format!(
                            "push to branch {branch:?} requires: {}",
                            require.join(", ")
                        ),
                    }
                }
            }
            (Rule::GitPush { .. }, Action::Shell { .. })
            | (Rule::Shell { .. }, Action::GitPush { .. }) => {
                unreachable!(
                    "Rule::decide is only ever called for a (rule, action) pair Rule::matches \
                     already confirmed agree -- see RuleSet::matching, the only caller"
                )
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
    pub fn empty() -> Self {
        Self { rules: Vec::new() }
    }

    /// Parses and validates a `.warden/policy.yaml` document.
    pub fn from_yaml(raw: &str) -> Result<Self> {
        let is_empty_or_comment_only = raw
            .lines()
            .all(|line| line.trim().is_empty() || line.trim().starts_with('#'));
        if is_empty_or_comment_only {
            return Ok(RuleSet::empty());
        }
        serde_yaml::from_str(raw).map_err(|error| PolicyError::InvalidYaml(error.to_string()))
    }

    /// Every rule matching `action`, in file order -- [`crate::Evaluator`]'s own iteration point,
    /// kept here so [`Rule::matches`] stays private to this module.
    pub(crate) fn matching<'a>(&'a self, action: &'a Action) -> impl Iterator<Item = &'a Rule> {
        self.rules.iter().filter(move |rule| rule.matches(action))
    }
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
        assert_eq!(
            rules.rules[0],
            Rule::GitPush {
                branch: Some("main".to_string()),
                require: vec!["tests".to_string(), "review".to_string()],
                deny: Vec::new(),
            }
        );
        assert_eq!(
            rules.rules[1],
            Rule::Shell {
                deny: vec!["rm -rf /".to_string()],
            }
        );
    }

    #[test]
    fn an_absent_document_yields_an_empty_rule_set() {
        assert_eq!(RuleSet::from_yaml("").unwrap(), RuleSet::empty());
    }

    #[test]
    fn a_whitespace_only_document_yields_an_empty_rule_set() {
        assert_eq!(RuleSet::from_yaml("   \n\n  ").unwrap(), RuleSet::empty());
    }

    #[test]
    fn a_comment_only_document_yields_an_empty_rule_set() {
        assert_eq!(
            RuleSet::from_yaml("# no rules configured yet\n").unwrap(),
            RuleSet::empty()
        );
    }

    #[test]
    fn an_explicit_empty_rules_list_yields_an_empty_rule_set() {
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
    fn a_schema_error_past_a_leading_comment_still_carries_its_line_number() {
        let yaml = "# a leading comment, so the empty-document short-circuit must not fire\n\
                     rules:\n  - action: launch_missiles\n";
        let error = RuleSet::from_yaml(yaml).unwrap_err();
        match error {
            PolicyError::InvalidYaml(msg) => {
                assert!(msg.contains("line"), "expected a line locator: {msg}");
            }
        }
    }

    #[test]
    fn rejects_a_rule_with_an_unknown_action() {
        let yaml = "rules:\n  - action: launch_missiles\n";
        let error = RuleSet::from_yaml(yaml).unwrap_err();
        match error {
            PolicyError::InvalidYaml(msg) => {
                assert!(msg.contains("launch_missiles"), "{msg}");
            }
        }
    }

    #[test]
    fn rejects_a_rule_with_an_unknown_field() {
        let yaml = "rules:\n  - action: shell\n    typo_field: nope\n";
        let error = RuleSet::from_yaml(yaml).unwrap_err();
        assert!(matches!(error, PolicyError::InvalidYaml(_)));
    }

    #[test]
    fn rejects_a_shell_rule_declaring_a_git_push_only_field() {
        for field in ["branch: main", "require: [tests]"] {
            let yaml = format!("rules:\n  - action: shell\n    {field}\n");
            let error = RuleSet::from_yaml(&yaml).unwrap_err();
            assert!(
                matches!(error, PolicyError::InvalidYaml(_)),
                "{field}: {error}"
            );
        }
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
            rule.decide(&Action::Shell {
                command: "rm -rf /".to_string()
            }),
            Decision::Deny {
                reason: "shell command matches denied pattern \"rm -rf /\": rm -rf /".to_string()
            }
        );
        assert_eq!(
            rule.decide(&Action::Shell {
                command: "cargo test".to_string()
            }),
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
            rule.decide(&Action::GitPush {
                branch: "main".to_string()
            }),
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
            rule.decide(&Action::GitPush {
                branch: "main".to_string()
            }),
            Decision::Allow
        );
    }

    #[test]
    fn a_git_push_rule_denies_only_a_branch_matching_exactly() {
        let rules = RuleSet::from_yaml("rules:\n  - action: git_push\n    deny: [main]\n").unwrap();
        let rule = &rules.rules[0];
        assert_eq!(
            rule.decide(&Action::GitPush {
                branch: "main".to_string()
            }),
            Decision::Deny {
                reason: "push to branch \"main\" matches denied branch \"main\"".to_string()
            }
        );
        for unrelated in ["domain-refactor", "maintenance", "remain"] {
            assert_eq!(
                rule.decide(&Action::GitPush {
                    branch: unrelated.to_string()
                }),
                Decision::Allow,
                "{unrelated} must not be caught by an exact-match deny on \"main\""
            );
        }
    }

    #[test]
    fn deny_takes_priority_over_require_within_the_same_git_push_rule() {
        let rules = RuleSet::from_yaml(
            "rules:\n  - action: git_push\n    branch: main\n    deny: [main]\n    require: [tests]\n",
        )
        .unwrap();
        let rule = &rules.rules[0];
        assert_eq!(
            rule.decide(&Action::GitPush {
                branch: "main".to_string()
            }),
            Decision::Deny {
                reason: "push to branch \"main\" matches denied branch \"main\"".to_string()
            }
        );
    }

    #[test]
    fn a_git_push_rule_denies_any_of_several_listed_branches() {
        let rules = RuleSet::from_yaml("rules:\n  - action: git_push\n    deny: [main, release]\n")
            .unwrap();
        let rule = &rules.rules[0];
        for denied in ["main", "release"] {
            assert_eq!(
                rule.decide(&Action::GitPush {
                    branch: denied.to_string()
                }),
                Decision::Deny {
                    reason: format!("push to branch {denied:?} matches denied branch {denied:?}")
                },
                "{denied} must be denied"
            );
        }
        assert_eq!(
            rule.decide(&Action::GitPush {
                branch: "develop".to_string()
            }),
            Decision::Allow
        );
    }

    #[test]
    fn a_branch_scoped_deny_entry_for_a_different_branch_is_unreachable_through_matching() {
        let rules = RuleSet::from_yaml(
            "rules:\n  - action: git_push\n    branch: main\n    deny: [release]\n",
        )
        .unwrap();
        let rule = &rules.rules[0];
        assert!(
            !rule.matches(&Action::GitPush {
                branch: "release".to_string()
            }),
            "the branch scope excludes \"release\" from ever reaching Rule::decide, so its \
             presence in `deny` has no effect through RuleSet::matching"
        );
        assert!(rule.matches(&Action::GitPush {
            branch: "main".to_string()
        }));
        assert_eq!(
            rule.decide(&Action::GitPush {
                branch: "main".to_string()
            }),
            Decision::Allow
        );
    }
}
