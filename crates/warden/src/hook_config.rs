//! Declarative hook configuration: `<repo>/.warden/hooks.toml`.

use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;
use warden_core::HookPoint;
use warden_sandbox::Sandbox;

use crate::error::{Result, WardenError};
use crate::hook::{CommandHook, HookRegistry};
use crate::policy_gate::PolicyGate;

/// The parsed shape of a `.warden/hooks.toml` file.
#[derive(Debug, Deserialize)]
struct HooksFile {
    #[serde(default)]
    hooks: Vec<RawHook>,
}

/// One `[[hooks]]` entry, before its `point` string is resolved to a [`HookPoint`].
#[derive(Debug, Deserialize)]
struct RawHook {
    /// The lifecycle point, as its stable string form (`HookPoint::as_str`).
    point: String,
    /// The shell line to run (executed via `sh -c`).
    run: String,
    /// Whether a non-zero exit blocks the run.
    #[serde(default = "default_block_on_failure")]
    block_on_failure: bool,
}

fn default_block_on_failure() -> bool {
    true
}

/// The conventional path of a repo's hook config.
fn hooks_file_path(repo_path: &Path) -> std::path::PathBuf {
    repo_path.join(".warden").join("hooks.toml")
}

/// Reads the exact hook document selected for a run so quota recovery can persist it instead of re-
/// reading a repository that agents may have changed since the run started.
pub fn read_repo_hooks(repo_path: &Path) -> Result<Option<String>> {
    let path = hooks_file_path(repo_path);
    match std::fs::read_to_string(&path) {
        Ok(contents) => Ok(Some(contents)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(WardenError::HookConfig {
            path,
            reason: format!("could not read the file: {err}"),
        }),
    }
}

/// Builds a hook registry from an already-resolved document.
pub fn parse_repo_hooks(
    repo_path: &Path,
    contents: Option<&str>,
    sandbox: Arc<dyn Sandbox>,
    policy_gate: Arc<PolicyGate>,
) -> Result<HookRegistry> {
    let path = hooks_file_path(repo_path);
    let Some(contents) = contents else {
        return Ok(HookRegistry::new());
    };

    let parsed: HooksFile = toml::from_str(contents).map_err(|err| WardenError::HookConfig {
        path: path.clone(),
        reason: err.to_string(),
    })?;

    let mut registry = HookRegistry::new();
    for (index, raw) in parsed.hooks.into_iter().enumerate() {
        let point = HookPoint::parse(&raw.point).ok_or_else(|| WardenError::HookConfig {
            path: path.clone(),
            reason: format!(
                "hook #{} names an unknown point {:?}; valid points are: {}",
                index + 1,
                raw.point,
                HookPoint::ALL
                    .iter()
                    .map(|p| p.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        })?;
        registry.register(Arc::new(CommandHook::new(
            vec![point],
            raw.run,
            raw.block_on_failure,
            Arc::clone(&sandbox),
            Arc::clone(&policy_gate),
        )));
    }
    Ok(registry)
}

/// Loads `<repo_path>/.warden/hooks.toml` into a [`HookRegistry`], one [`CommandHook`] per entry
/// (file order preserved).
pub fn load_repo_hooks(
    repo_path: &Path,
    sandbox: Arc<dyn Sandbox>,
    policy_gate: Arc<PolicyGate>,
) -> Result<HookRegistry> {
    let contents = read_repo_hooks(repo_path)?;
    parse_repo_hooks(repo_path, contents.as_deref(), sandbox, policy_gate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use warden_sandbox::LocalSandbox;

    fn sandbox() -> Arc<dyn Sandbox> {
        Arc::new(LocalSandbox::new())
    }

    fn policy_gate() -> Arc<PolicyGate> {
        Arc::new(PolicyGate::empty())
    }

    fn write_hooks(dir: &Path, contents: &str) {
        let warden = dir.join(".warden");
        std::fs::create_dir_all(&warden).unwrap();
        std::fs::write(warden.join("hooks.toml"), contents).unwrap();
    }

    #[test]
    fn absent_file_is_an_empty_registry_not_an_error() {
        let dir = TempDir::new().unwrap();
        let registry = load_repo_hooks(dir.path(), sandbox(), policy_gate()).unwrap();
        assert!(registry.is_empty(), "no config -> no hooks");
    }

    #[test]
    fn a_well_formed_file_builds_one_hook_per_entry_in_order() {
        let dir = TempDir::new().unwrap();
        write_hooks(
            dir.path(),
            r#"
            [[hooks]]
            point = "on_run_start"
            run = "docker compose up -d"

            [[hooks]]
            point = "on_run_end"
            run = "docker compose down"
            block_on_failure = false
            "#,
        );
        let registry = load_repo_hooks(dir.path(), sandbox(), policy_gate()).unwrap();
        assert!(!registry.is_empty());
    }

    #[test]
    fn an_unknown_point_is_a_hard_error_listing_the_valid_names() {
        let dir = TempDir::new().unwrap();
        write_hooks(
            dir.path(),
            r#"
            [[hooks]]
            point = "on_run_startt"
            run = "true"
            "#,
        );
        let err = load_repo_hooks(dir.path(), sandbox(), policy_gate()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("on_run_startt"), "names the offender: {msg}");
        assert!(msg.contains("on_run_start"), "lists valid names: {msg}");
    }

    #[test]
    fn malformed_toml_is_a_hard_error_not_a_silent_empty_registry() {
        let dir = TempDir::new().unwrap();
        write_hooks(dir.path(), "this is not = valid toml [[[");
        let err = load_repo_hooks(dir.path(), sandbox(), policy_gate()).unwrap_err();
        assert!(matches!(err, WardenError::HookConfig { .. }));
    }

    #[test]
    fn block_on_failure_defaults_to_true_when_omitted() {
        let dir = TempDir::new().unwrap();
        write_hooks(
            dir.path(),
            r#"
            [[hooks]]
            point = "before_push"
            run = "cargo fmt --check"
            "#,
        );
        assert!(load_repo_hooks(dir.path(), sandbox(), policy_gate()).is_ok());
    }
}
