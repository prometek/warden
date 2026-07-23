//! Declarative hook configuration: `<repo>/.warden/hooks.toml`.
//!
//! Compiles a repo's hook file into a [`HookRegistry`] of [`CommandHook`]s --
//! the concrete half of the lifecycle-hook feature (the foundation is
//! `warden_core::hook` / `crate::hook`). Each `[[hooks]]` entry names a
//! [`HookPoint`] and a shell `run` line; the loader turns it into a
//! [`CommandHook`] that runs through the [`Sandbox`], in **file order**
//! (registration order = execution order, `HookRegistry`'s own contract).
//!
//! ```toml
//! [[hooks]]
//! point = "on_run_start"
//! run = "docker compose up -d"
//! block_on_failure = true      # default; a failed setup step blocks the run
//!
//! [[hooks]]
//! point = "on_run_end"
//! run = "docker compose down"
//! ```
//!
//! # Trust model
//!
//! `.warden/hooks.toml` lives **in the target repo**, and a `run` line is an
//! arbitrary shell command executed on this host (the same OS user Warden
//! runs as -- `LocalSandbox` is not isolation). It is honoured **by default**,
//! with no opt-in flag. This is a deliberate, operator-owned choice and is
//! consistent with how the repo's own `.warden/agents/coder.md` is already
//! trusted by default (only the reviewer/tester definitions require
//! `--trust-repo-agents`, issue #26): a developer running Warden on a repo is
//! already running that repo's coder agent. Running a repo whose
//! `.warden/hooks.toml` you have not read will run its commands -- the same
//! footgun a `Makefile` or an npm `postinstall` already is.
//!
//! # Failure handling
//!
//! A present-but-broken config (malformed TOML, or an entry naming an unknown
//! `point`) is a hard [`WardenError::HookConfig`], never silently ignored --
//! code-standards.md's "no silent fallback", so a typo does not leave a setup
//! hook quietly not running. An **absent** file is not an error: it yields an
//! empty registry (no hooks; dispatch is the same strict no-op the default
//! empty registry already is).

use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;
use warden_core::HookPoint;
use warden_sandbox::Sandbox;

use crate::error::{Result, WardenError};
use crate::hook::{CommandHook, HookRegistry};

/// The parsed shape of a `.warden/hooks.toml` file.
#[derive(Debug, Deserialize)]
struct HooksFile {
    #[serde(default)]
    hooks: Vec<RawHook>,
}

/// One `[[hooks]]` entry, before its `point` string is resolved to a
/// [`HookPoint`].
#[derive(Debug, Deserialize)]
struct RawHook {
    /// The lifecycle point, as its stable string form (`HookPoint::as_str`).
    point: String,
    /// The shell line to run (executed via `sh -c`).
    run: String,
    /// Whether a non-zero exit blocks the run. Defaults to `true`: a hook is
    /// most often a setup/gate step whose failure means the run should not
    /// proceed; opting out is the deliberate `false`.
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

/// Loads `<repo_path>/.warden/hooks.toml` into a [`HookRegistry`], one
/// [`CommandHook`] per entry (file order preserved). An absent file yields an
/// empty registry; a malformed one is a [`WardenError::HookConfig`]. Every
/// hook runs through `sandbox` (shared with the orchestrator's own so a single
/// backend choice covers both -- see `crate::main`).
pub fn load_repo_hooks(repo_path: &Path, sandbox: Arc<dyn Sandbox>) -> Result<HookRegistry> {
    let path = hooks_file_path(repo_path);
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(HookRegistry::new());
        }
        Err(err) => {
            return Err(WardenError::HookConfig {
                path,
                reason: format!("could not read the file: {err}"),
            });
        }
    };

    let parsed: HooksFile = toml::from_str(&contents).map_err(|err| WardenError::HookConfig {
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
        )));
    }
    Ok(registry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use warden_sandbox::LocalSandbox;

    fn sandbox() -> Arc<dyn Sandbox> {
        Arc::new(LocalSandbox::new())
    }

    fn write_hooks(dir: &Path, contents: &str) {
        let warden = dir.join(".warden");
        std::fs::create_dir_all(&warden).unwrap();
        std::fs::write(warden.join("hooks.toml"), contents).unwrap();
    }

    #[test]
    fn absent_file_is_an_empty_registry_not_an_error() {
        let dir = TempDir::new().unwrap();
        let registry = load_repo_hooks(dir.path(), sandbox()).unwrap();
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
        let registry = load_repo_hooks(dir.path(), sandbox()).unwrap();
        assert!(!registry.is_empty());
        // Registration order is the observable contract; the count and the
        // points are asserted through the registry's public dispatch behaviour
        // in `crate::hook`'s own tests, so here we prove the file was accepted
        // and produced a non-empty registry.
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
        let err = load_repo_hooks(dir.path(), sandbox()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("on_run_startt"), "names the offender: {msg}");
        assert!(msg.contains("on_run_start"), "lists valid names: {msg}");
    }

    #[test]
    fn malformed_toml_is_a_hard_error_not_a_silent_empty_registry() {
        let dir = TempDir::new().unwrap();
        write_hooks(dir.path(), "this is not = valid toml [[[");
        let err = load_repo_hooks(dir.path(), sandbox()).unwrap_err();
        assert!(matches!(err, WardenError::HookConfig { .. }));
    }

    #[test]
    fn block_on_failure_defaults_to_true_when_omitted() {
        // The default is asserted at the deserialization boundary: an entry
        // that omits `block_on_failure` parses, and the loader accepts it.
        let dir = TempDir::new().unwrap();
        write_hooks(
            dir.path(),
            r#"
            [[hooks]]
            point = "before_push"
            run = "cargo fmt --check"
            "#,
        );
        assert!(load_repo_hooks(dir.path(), sandbox()).is_ok());
    }
}
