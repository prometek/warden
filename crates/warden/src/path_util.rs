//! A single shared path-canonicalization primitive used by every containment check in this crate.

use std::path::{Path, PathBuf};

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

    #[test]
    fn a_relative_path_with_no_existing_ancestor_at_all_is_not_found() {
        let error =
            canonicalize_best_effort(Path::new("warden-path-util-test-nonexistent-1e6c8f2a"))
                .expect_err("no ancestor exists");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn an_absolute_path_bottoms_out_at_root_which_always_exists() {
        let resolved =
            canonicalize_best_effort(Path::new("/this/does/not/exist/anywhere")).unwrap();
        assert_eq!(resolved, PathBuf::from("/this/does/not/exist/anywhere"));
    }
}
