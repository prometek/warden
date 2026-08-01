//! Declarative policy configuration: `<repo>/.warden/policy.yaml`.

use std::path::Path;

use crate::error::{Result, WardenError};

/// The conventional path of a repo's policy file.
fn policy_file_path(repo_path: &Path) -> std::path::PathBuf {
    repo_path.join(".warden").join("policy.yaml")
}

/// Reads the exact policy document selected for a run.
pub fn read_repo_policy(repo_path: &Path) -> Result<Option<String>> {
    let path = policy_file_path(repo_path);
    match std::fs::read_to_string(&path) {
        Ok(contents) => Ok(Some(contents)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(WardenError::PolicyConfig {
            path,
            reason: format!("could not read the file: {err}"),
        }),
    }
}

/// Parses an already-resolved policy document.
pub fn parse_repo_policy(
    repo_path: &Path,
    contents: Option<&str>,
) -> Result<warden_policy::RuleSet> {
    match contents {
        Some(contents) => {
            warden_policy::RuleSet::from_yaml(contents).map_err(|error| WardenError::PolicyConfig {
                path: policy_file_path(repo_path),
                reason: error.to_string(),
            })
        }
        None => Ok(warden_policy::RuleSet::empty()),
    }
}

/// Loads `<repo_path>/.warden/policy.yaml` into a [`warden_policy::RuleSet`].
pub fn load_repo_policy(repo_path: &Path) -> Result<warden_policy::RuleSet> {
    let contents = read_repo_policy(repo_path)?;
    parse_repo_policy(repo_path, contents.as_deref())
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
