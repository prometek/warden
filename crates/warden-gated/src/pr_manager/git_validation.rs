use super::*;

/// The well-known, content-addressed sha of git's empty tree object -- identical in every
/// repository, since it's hashed from fixed content (`tree\0` with no entries).
pub(crate) const EMPTY_TREE_SHA: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// Independently determines whether `skeleton_commit_sha` changes anything relative to
/// `base_branch`'s current tip on `origin`.
pub(super) async fn skeleton_diff_against_base(
    bare_repo_path: &Path,
    base_branch: &str,
    skeleton_commit_sha: &str,
) -> Result<Vec<String>> {
    let base_sha = match remote_branch_head(bare_repo_path, "origin", base_branch).await? {
        Some(base_sha) => {
            fetch_branch(bare_repo_path, "origin", base_branch).await?;
            Some(base_sha)
        }
        None => None,
    };

    let net_tree_compare_from = base_sha.as_deref().unwrap_or(EMPTY_TREE_SHA);
    let mut offending_files =
        diff_name_only(bare_repo_path, net_tree_compare_from, skeleton_commit_sha).await?;

    let pushed_commits =
        commits_in_range(bare_repo_path, base_sha.as_deref(), skeleton_commit_sha).await?;
    for commit in &pushed_commits {
        offending_files.extend(commit_own_diff(bare_repo_path, commit).await?);
    }

    offending_files.sort();
    offending_files.dedup();
    Ok(offending_files)
}

/// The net set of file paths that differ between two commits/trees -- regardless of what happened
/// in between.
async fn diff_name_only(repo_path: &Path, from: &str, to: &str) -> Result<Vec<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["diff", "--name-only", from, to])
        .output()
        .await?;
    if !output.status.success() {
        return Err(GatedError::GitCommandFailed {
            command: format!(
                "git -C {} diff --name-only {from} {to}",
                repo_path.display()
            ),
            exit_code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_string)
        .collect())
}

/// The sha `base_branch` currently points to on `remote`, or `None` if that branch doesn't exist
/// there yet.
pub(crate) async fn remote_branch_head(
    repo_path: &Path,
    remote: &str,
    branch: &str,
) -> Result<Option<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["ls-remote", "--heads", remote])
        .output()
        .await?;

    if !output.status.success() {
        return Err(GatedError::GitCommandFailed {
            command: format!("git -C {} ls-remote --heads {remote}", repo_path.display()),
            exit_code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    let target_ref = format!("refs/heads/{branch}");
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            let sha = fields.next()?;
            let refname = fields.next()?;
            (refname == target_ref).then(|| sha.to_string())
        }))
}

/// Fetches `branch` from `remote` into `repo_path`'s local object store (read-only) so its history
/// can be walked/diffed locally.
pub(crate) async fn fetch_branch(repo_path: &Path, remote: &str, branch: &str) -> Result<()> {
    run_git(repo_path, &["fetch", "--quiet", remote, branch]).await
}

async fn commits_in_range(
    repo_path: &Path,
    range_start: Option<&str>,
    range_end: &str,
) -> Result<Vec<String>> {
    let range_arg = match range_start {
        Some(start) => format!("{start}..{range_end}"),
        None => range_end.to_string(),
    };
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["rev-list", &range_arg])
        .output()
        .await?;
    if !output.status.success() {
        return Err(GatedError::GitCommandFailed {
            command: format!("git -C {} rev-list {range_arg}", repo_path.display()),
            exit_code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_string)
        .collect())
}

async fn commit_own_diff(repo_path: &Path, commit_sha: &str) -> Result<Vec<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args([
            "diff-tree",
            "--no-commit-id",
            "--name-only",
            "-r",
            "--root",
            "--cc",
            commit_sha,
        ])
        .output()
        .await?;
    if !output.status.success() {
        return Err(GatedError::GitCommandFailed {
            command: format!(
                "git -C {} diff-tree --no-commit-id --name-only -r --root --cc {commit_sha}",
                repo_path.display()
            ),
            exit_code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_string)
        .collect())
}

async fn run_git(repo_path: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(args)
        .output()
        .await?;
    if !output.status.success() {
        return Err(GatedError::GitCommandFailed {
            command: format!("git -C {} {}", repo_path.display(), args.join(" ")),
            exit_code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(())
}
