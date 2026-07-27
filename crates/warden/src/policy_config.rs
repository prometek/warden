//! Declarative policy configuration: `<repo>/.warden/policy.yaml` (issue #51,
//! ADR-0016).
//!
//! Mirrors `crate::hook_config::load_repo_hooks`'s own shape exactly: reads
//! the repo's policy file (I/O -- `warden_policy` itself never touches the
//! filesystem, see its own docs), parses it via
//! `warden_policy::RuleSet::from_yaml`, and decides that an **absent** file
//! means "no rules" while a **malformed** one is a hard error.
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
//! # Trust model
//!
//! `.warden/policy.yaml` lives **in the target repo**, exactly like
//! `.warden/hooks.toml` (`crate::hook_config`'s own docs) and
//! `.warden/agents/coder.md` -- honoured **by default**, no opt-in flag. A
//! repo whose policy you have not read still applies before you run it, the
//! same trust boundary every other `.warden/` convention file already
//! carries.
//!
//! # Failure handling
//!
//! A present-but-broken file (malformed YAML, an unknown top-level key, an
//! unknown `action`) is a hard [`WardenError::PolicyConfig`], never silently
//! ignored (code-standards.md: "no silent fallback") -- a typo in a rule
//! must not quietly leave a run with no governance at all. An absent file
//! yields [`warden_policy::RuleSet::empty`] (no rules; every action is
//! allowed, the same no-op an empty `HookRegistry` already is).

use std::path::Path;

use crate::error::{Result, WardenError};

/// The conventional path of a repo's policy file.
fn policy_file_path(repo_path: &Path) -> std::path::PathBuf {
    repo_path.join(".warden").join("policy.yaml")
}

/// Loads `<repo_path>/.warden/policy.yaml` into a [`warden_policy::RuleSet`].
/// An absent file yields [`warden_policy::RuleSet::empty`]; a malformed one
/// is a [`WardenError::PolicyConfig`].
pub fn load_repo_policy(repo_path: &Path) -> Result<warden_policy::RuleSet> {
    let path = policy_file_path(repo_path);
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(warden_policy::RuleSet::empty());
        }
        Err(err) => {
            return Err(WardenError::PolicyConfig {
                path,
                reason: format!("could not read the file: {err}"),
            });
        }
    };

    warden_policy::RuleSet::from_yaml(&contents).map_err(|error| WardenError::PolicyConfig {
        path,
        reason: error.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_policy(dir: &Path, contents: &str) {
        let warden = dir.join(".warden");
        std::fs::create_dir_all(&warden).unwrap();
        std::fs::write(warden.join("policy.yaml"), contents).unwrap();
    }

    #[test]
    fn absent_file_is_an_empty_rule_set_not_an_error() {
        let dir = TempDir::new().unwrap();
        let rules = load_repo_policy(dir.path()).unwrap();
        assert_eq!(rules, warden_policy::RuleSet::empty());
    }

    #[test]
    fn a_well_formed_file_parses_into_its_rules() {
        let dir = TempDir::new().unwrap();
        write_policy(
            dir.path(),
            r#"
            rules:
              - action: git_push
                branch: main
                require: [tests, review]
              - action: shell
                deny: ["rm -rf /"]
            "#,
        );
        let rules = load_repo_policy(dir.path()).unwrap();
        assert_eq!(rules.rules.len(), 2);
    }

    #[test]
    fn malformed_yaml_is_a_hard_error_not_a_silent_empty_rule_set() {
        let dir = TempDir::new().unwrap();
        write_policy(dir.path(), "this is not = valid yaml: [[[");
        let err = load_repo_policy(dir.path()).unwrap_err();
        assert!(matches!(err, WardenError::PolicyConfig { .. }));
    }

    #[test]
    fn an_unknown_action_is_a_hard_error_naming_the_offender() {
        let dir = TempDir::new().unwrap();
        write_policy(dir.path(), "rules:\n  - action: launch_missiles\n");
        let err = load_repo_policy(dir.path()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("launch_missiles"), "{msg}");
    }
}
