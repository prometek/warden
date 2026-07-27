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
//!
//! # `deny` is not a security control
//!
//! [`Rule::Shell`]'s `deny` is a literal-substring check against a
//! `.warden/hooks.toml` command line supplied by the **target repo** and run
//! on the host with the operator's full environment, including
//! `SSH_AUTH_SOCK` and every trusted credential (`warden::hook::CommandHook`'s
//! own "Environment" docs). It stops an *accident* (a copy-pasted `rm -rf /`)
//! and nothing else: `deny: ["curl"]` does not stop `/usr/bin/cur''l`,
//! `wget`, a base64-encoded payload, or any other trivial rewrite a hostile
//! definition author would reach for. Treat it as defence-in-depth, never as
//! a substitute for not running an untrusted repo's hooks in the first
//! place.

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
    /// An attempt to stage a run's converged commit into the local bare gate
    /// repo (ADR-0002), targeting the run's own base/target branch
    /// (`warden::orchestrator::RunConfig::branch` -- the *ref* actually
    /// written is `refs/warden-staging/<run_id>`, not `refs/heads/<branch>`
    /// itself; `branch` here names what the run is targeting, not the
    /// literal ref touched). Never a push straight to `origin`, which stays
    /// `warden-gated`'s exclusive, independently re-verified job
    /// (ADR-0002/0006, ADR-0016's own boundary). Evaluating this action lets
    /// an operator require approval, or forbid it outright, before `warden`
    /// even stages that push.
    GitPush { branch: String },
    /// A shell command `warden` is about to run on an agent's/hook's behalf
    /// -- today, a `.warden/hooks.toml` `CommandHook`'s own `run` line
    /// (`warden::hook::CommandHook`).
    Shell { command: String },
}

/// One `- action: ...` entry of `.warden/policy.yaml`. Internally tagged on
/// `action` (`"git_push"`/`"shell"`) so a field that means nothing for a
/// given action -- `branch`/`require` on a `shell` rule, for instance -- is
/// unrepresentable rather than silently parsed and ignored: `serde`'s
/// `deny_unknown_fields` rejects it per-variant, at [`RuleSet::from_yaml`]
/// time, naming the offending field.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Rule {
    GitPush {
        /// Restricts this rule to pushes targeting exactly this branch
        /// (equality, not substring -- same semantics as `deny` below).
        /// Absent means "every branch".
        #[serde(default)]
        branch: Option<String>,
        /// A non-empty list means this rule's matching pushes require human
        /// approval before proceeding (the list itself -- e.g. `[tests,
        /// review]` -- is carried into [`crate::Decision::RequireApproval`]'s
        /// `reason` for the human approving to read, not separately
        /// enforced by this crate; issue #51 is a minimal foundation, not a
        /// checklist engine).
        #[serde(default)]
        require: Vec<String>,
        /// A list of branch names that, if the pushed branch equals one of
        /// them exactly, deny this action outright -- e.g. `deny: [main]`
        /// forbids pushing `main` under any circumstance, distinct from
        /// `branch` (which only *scopes which pushes this rule applies to*)
        /// and from `require` (which still allows the push once approved).
        /// Exact-match, the same semantics as `branch` above -- deliberately
        /// not a substring/glob check, unlike [`Rule::Shell`]'s own `deny`
        /// (see this module's own docs on why the two engines differ): a
        /// branch name is a single well-defined string an operator names in
        /// full, not a command line where prefix matching is the whole
        /// point.
        #[serde(default)]
        deny: Vec<String>,
    },
    Shell {
        /// A non-empty list of literal substrings that, if any is contained
        /// in the evaluated command, deny it outright. Deliberately simple
        /// substring containment, not a glob/regex engine -- "socle minimal
        /// d'abord" (issue #51's own scope) -- and **not a security
        /// boundary** (see this module's own "`deny` is not a security
        /// control" docs).
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

    /// The [`crate::Decision`] this rule alone contributes for `action`,
    /// once [`Rule::matches`] is already known to be true (the mismatched
    /// combinations are unreachable through [`RuleSet::matching`], the only
    /// caller). `deny` always takes priority over `require` within a single
    /// `GitPush` rule: a branch that is both denied and gated by `require`
    /// is still denied outright -- `require` only ever softens "forbidden"
    /// into "needs a human", it never overrides an explicit `deny`.
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
    /// unknown top-level key, an unknown `action`, a field that means
    /// nothing for the rule's own `action`) -- the caller
    /// (`warden::policy_config`, which reads the file) names *which* file,
    /// never silently falling back to [`RuleSet::empty`] on a parse failure
    /// (code-standards.md: no silent fallback -- a malformed policy file
    /// must fail the run, not quietly run with no governance at all).
    ///
    /// An **empty or comment-only** document is explicitly *not* a parse
    /// failure -- YAML has no `rules:` key to omit-with-a-default there, it
    /// parses to `null` rather than an empty mapping, so this is handled
    /// before struct deserialization rather than left to surface as a
    /// confusing "EOF while parsing a value". It resolves to
    /// [`RuleSet::empty`], consistent with an absent file
    /// (`warden::policy_config`'s own convention) and with
    /// `warden::hook_config::load_repo_hooks`'s TOML equivalent, which
    /// already parses an empty file to an empty table.
    ///
    /// Detected with a plain line scan (every line blank or `#`-commented),
    /// checked *before* any YAML parsing -- not by parsing to
    /// `serde_yaml::Value` first and checking `is_null()` (issue #51 review
    /// round 2, finding B). That alternative was tried and reverted: a
    /// `serde_yaml::Value` deserialized via `from_value` carries no source
    /// marks, so every schema error past this point (an unknown `action`, a
    /// field that means nothing for one, a bad top-level key) lost its
    /// `, line: N, column: M` suffix entirely -- exactly the locator a
    /// multi-rule file needs to be actionable. Parsing straight from `raw`
    /// with `serde_yaml::from_str::<RuleSet>` below keeps it.
    pub fn from_yaml(raw: &str) -> Result<Self> {
        let is_empty_or_comment_only = raw
            .lines()
            .all(|line| line.trim().is_empty() || line.trim().starts_with('#'));
        if is_empty_or_comment_only {
            return Ok(RuleSet::empty());
        }
        serde_yaml::from_str(raw).map_err(|error| PolicyError::InvalidYaml(error.to_string()))
    }

    /// Every rule matching `action`, in file order -- [`crate::Evaluator`]'s
    /// own iteration point, kept here so [`Rule::matches`] stays private to
    /// this module.
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

    /// Issue #51 review round 2, finding B: a schema error must still carry
    /// its source position (`, line: N, column: M`) -- lost entirely by an
    /// earlier version of this function that parsed to `serde_yaml::Value`
    /// first (no source marks survive that round trip) before checking for
    /// an unknown `action`/field. A multi-rule `.warden/policy.yaml` is
    /// barely actionable without it.
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

    /// Issue #51 review, MEDIUM 4: `branch`/`require` mean nothing for a
    /// `shell` rule -- unrepresentable now (the field simply does not exist
    /// on that variant), rejected at parse time rather than silently parsed
    /// and ignored.
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

    /// Issue #51 review, MEDIUM 6: `deny` for `git_push` is exact-match,
    /// same semantics as `branch` -- a name-adjacent branch must never be
    /// caught by accident (unlike `Shell`'s deliberately looser substring
    /// engine).
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
}
