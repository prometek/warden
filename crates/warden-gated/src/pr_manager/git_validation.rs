use super::*;

/// The well-known, content-addressed sha of git's empty tree object --
/// identical in every repository, since it's hashed from fixed content
/// (`tree\0` with no entries). Used as the net-tree comparison point when
/// `base_branch` doesn't exist on `origin` yet: in that case the only
/// content-free skeleton is one whose own tip tree is also empty.
pub(crate) const EMPTY_TREE_SHA: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// Independently determines whether `skeleton_commit_sha` changes anything
/// relative to `base_branch`'s current tip on `origin`. Returns the list of
/// changed file paths (empty means content-free).
///
/// `open_draft` must never trust a caller-supplied sha to truly be "just a
/// branch skeleton" (issue #4 review, finding #1) -- this re-derives that
/// fact itself instead, mirroring how `gate::verify_and_authorize` re-derives
/// convergence rather than trusting the caller. Fetching `base_branch` from
/// `origin` is a read-only operation already within the access
/// `warden-gated` holds (it can already push there) -- no new credential
/// exposure.
///
/// This runs two independent, complementary checks -- neither alone is
/// robust against a history shape a caller (buggy or compromised) might
/// hand it:
///
/// 1. **Net tip-tree equality** (`diff_name_only` against `base_sha`, or
///    the empty tree if `base_branch` doesn't exist on `origin` yet): any
///    file that ends up in the tree `git push` actually transfers, no
///    matter how it got there. This alone would miss content that was
///    added and later removed again within the pushed history (still
///    transferred as blobs, still reachable via `git log`/`git show`, even
///    though the tip tree looks clean) -- see check 2.
/// 2. **Per-commit range walk** (`commits_in_range` + `commit_own_diff`):
///    every commit `git push` would actually transfer is checked
///    individually against its own parent(s), catching exactly the
///    add-then-remove case check 1 misses. `commit_own_diff` uses
///    `--cc` so a *merge* commit's own content (relative to *all* of its
///    parents, not just diffed against a single one) is inspected too --
///    plain `diff-tree` emits nothing at all for a merge commit by
///    default, which would otherwise let a merge that introduces a file
///    present in neither parent slip through this check unnoticed (issue
///    #4 review, merge-commit finding). Check 1 alone would still catch
///    such a file if it survives to the tip, but check 2 is what actually
///    names the offending commit/file directly and covers the
///    survives-then-gets-removed-later variant of the same bypass.
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
        // `base_branch` doesn't exist on `origin` yet -- there's nothing to
        // exclude, so every commit reachable from the skeleton is in scope.
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

/// The net set of file paths that differ between two commits/trees --
/// regardless of what happened in between. Used as the tip-vs-base
/// "backstop" half of `skeleton_diff_against_base`'s two checks.
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

/// The sha `base_branch` currently points to on `remote`, or `None` if that
/// branch doesn't exist there yet.
///
/// Lists *every* head and matches the ref column exactly against
/// `refs/heads/{branch}`, rather than handing `branch` to `git ls-remote` as
/// a pattern: `git ls-remote --heads <remote> <branch>` matches any ref
/// whose path ends in `/branch` (e.g. `refs/heads/feat/main` alongside
/// `refs/heads/main`), and taking the first output line unconditionally
/// could silently pick a sibling branch's sha as the "base" (issue #4
/// review, follow-up finding).
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

/// Fetches `branch` from `remote` into `repo_path`'s local object store
/// (read-only) so its history can be walked/diffed locally.
pub(crate) async fn fetch_branch(repo_path: &Path, remote: &str, branch: &str) -> Result<()> {
    run_git(repo_path, &["fetch", "--quiet", remote, branch]).await
}

/// Lists the commits that `git push {range_end}:refs/heads/<branch>` would
/// actually transfer: everything reachable from `range_end` but not from
/// `range_start` (`git rev-list <start>..<end>`), or -- when there is no
/// `range_start` at all (`base_branch` doesn't exist on `origin` yet) --
/// every commit reachable from `range_end`, since there's nothing to
/// exclude.
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

/// The file paths `commit_sha` itself introduces, relative to its parent(s)
/// -- or relative to the empty tree if it's a root commit (`--root`, so a
/// root commit's real content isn't mistaken for "no diff" just because it
/// has no parent to diff against).
///
/// `--cc` matters here (issue #4 review, merge-commit finding): plain
/// `diff-tree` emits **nothing at all** for a merge commit by default (it
/// only shows non-merge commits' diffs), so a merge that introduces a file
/// present in *neither* parent -- content that still lands on `origin` once
/// pushed -- would otherwise pass this check completely unnoticed. `--cc`
/// (compact combined diff) surfaces exactly the files a merge's own tree
/// disagrees with *every* parent on, which is precisely "content the merge
/// itself introduced" -- and stays silent for an ordinary clean merge that
/// introduces nothing of its own (verified empirically: a merge combining
/// two branches' unrelated files reports nothing; one additionally adding a
/// file present in neither parent reports exactly that file).
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
