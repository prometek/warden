//! A single shared path-canonicalization primitive used by every containment
//! check in this crate (issue #26 review, MEDIUM).
//!
//! Before this module existed, [`canonicalize_best_effort`] was copy-pasted
//! three times -- `agent_def.rs`, `process.rs`, and `worktree.rs` -- on the
//! theory that each caller wanted a different error type wrapped around it
//! and the coupling of a shared helper wasn't worth it. In practice the
//! `agent_def.rs` and `process.rs` copies were byte-for-byte identical (both
//! already returned a plain [`std::io::Result`]), and the `worktree.rs` copy
//! quietly drifted from the other two's fixed algorithm: it kept popping
//! path segments on *any* `canonicalize` error, not just
//! [`std::io::ErrorKind::NotFound`], and silently discarded a
//! `strip_prefix` failure via `.unwrap_or(Path::new(""))` -- exactly the
//! "no silent fallback" violation the other two copies were fixed to avoid.
//! That mattered for real: `WorktreeManager::new`'s "worktrees_root must not
//! be inside the repo" containment check
//! (see [`crate::worktree::WorktreeManager::new`]) used the unfixed copy, so
//! a permissions error (`EACCES`/`ELOOP`) on an ancestor of `worktrees_root`
//! could make it walk straight past the failure and compare a truncated
//! path -- silently *not* failing closed, unlike every other containment
//! check in this crate.
//!
//! One function, one fixed algorithm, used everywhere a containment check in
//! this crate needs to resolve a path that may not exist yet:
//! [`crate::process::validate_agent_program`]'s `program` vs.
//! `worktree_path`/`repo_path`/`run_worktrees_root` check,
//! [`crate::agent_def::user_config_resolves_inside_repo_or_worktrees`]'s
//! `user_config_agents_dir` vs. `repo_path`/`<warden_home>/worktrees/` check,
//! and [`crate::worktree::WorktreeManager::new`]'s
//! `worktrees_root` vs. `main_repo_path` check. Each caller maps the
//! [`std::io::Error`] this returns into its own typed error at the call
//! site -- that mapping is a one-line `.map_err(...)`, cheap enough not to
//! justify a fourth error type shared across three otherwise-unrelated
//! modules.

use std::path::{Path, PathBuf};

/// Canonicalizes `path`, walking up to the nearest existing ancestor if
/// `path` itself (or an intermediate component) doesn't exist yet -- e.g. a
/// `program` argument nobody has spawned before, or
/// `~/.config/warden/agents/reviewer.md` before the user has ever created
/// that directory.
///
/// Fails closed: a `canonicalize` failure for any reason other than
/// [`std::io::ErrorKind::NotFound`] (a permissions error, `ELOOP`, ...) is
/// propagated immediately rather than silently walked past -- an ancestor
/// that exists but can't be canonicalized for some other reason means this
/// function can no longer verify what `path` actually resolves to, and
/// continuing to pop past it would defeat the exact containment check every
/// caller uses this for (code-standards.md: "no silent fallback").
pub(crate) fn canonicalize_best_effort(path: &Path) -> std::io::Result<PathBuf> {
    match path.canonicalize() {
        Ok(canonical) => Ok(canonical),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let file_name = path.file_name().ok_or(error)?;
            let parent = path.parent().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("no existing ancestor found for {}", path.display()),
                )
            })?;
            Ok(canonicalize_best_effort(parent)?.join(file_name))
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn resolves_a_fully_existing_path_via_std_canonicalize() {
        let dir = TempDir::new().unwrap();
        let resolved = canonicalize_best_effort(dir.path()).unwrap();
        assert_eq!(resolved, dir.path().canonicalize().unwrap());
    }

    #[test]
    fn walks_up_past_a_non_existent_tail_to_the_nearest_real_ancestor() {
        let dir = TempDir::new().unwrap();
        let candidate = dir.path().join("does").join("not").join("exist.md");
        let resolved = canonicalize_best_effort(&candidate).unwrap();
        assert_eq!(
            resolved,
            dir.path().canonicalize().unwrap().join("does/not/exist.md")
        );
    }

    /// The fail-closed guarantee this module's docs describe: a permissions
    /// error on an *existing* ancestor must propagate, never be silently
    /// walked past the way the old `worktree.rs` copy did.
    #[cfg(unix)]
    #[test]
    fn propagates_a_permission_error_on_an_existing_ancestor_instead_of_walking_past_it() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().unwrap();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        let candidate = locked.join("nested").join("target.md");

        let mut perms = std::fs::metadata(&locked).unwrap().permissions();
        perms.set_mode(0o000);
        std::fs::set_permissions(&locked, perms.clone()).unwrap();

        let result = canonicalize_best_effort(&candidate);

        // Restore permissions before any assertion can panic and leak a
        // directory `TempDir::drop` can't clean up.
        perms.set_mode(0o755);
        std::fs::set_permissions(&locked, perms).unwrap();

        let error = result.expect_err(
            "a permissions error on an existing ancestor must fail closed, not resolve",
        );
        assert_ne!(
            error.kind(),
            std::io::ErrorKind::NotFound,
            "must surface the real permissions failure, not a manufactured NotFound"
        );
    }

    /// The recursion bottoms out at `/`, which always exists -- so an
    /// absolute path with no real ancestor beyond the root still resolves
    /// successfully (`/` canonicalizes, then every missing component is
    /// joined back on). The genuine "no existing ancestor" failure only
    /// happens for a relative path whose walk runs out of parents before
    /// finding one that exists (`Path::parent()` eventually returns `None`).
    #[test]
    fn a_relative_path_with_no_existing_ancestor_at_all_is_not_found() {
        let error =
            canonicalize_best_effort(Path::new("warden-path-util-test-nonexistent-1e6c8f2a"))
                .expect_err("no ancestor exists");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }

    /// The mirror image of the relative case above: even with no real
    /// ancestor beyond it, `/` itself always exists, so this must resolve
    /// successfully rather than error.
    #[test]
    fn an_absolute_path_bottoms_out_at_root_which_always_exists() {
        let resolved =
            canonicalize_best_effort(Path::new("/this/does/not/exist/anywhere")).unwrap();
        assert_eq!(resolved, PathBuf::from("/this/does/not/exist/anywhere"));
    }
}
