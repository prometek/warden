use super::*;

#[allow(clippy::too_many_arguments)]
pub fn validate_agent_program(
    role_name: &str,
    is_producer: bool,
    program: &str,
    args: &[String],
    worktree_path: &Path,
    repo_path: &Path,
    run_worktrees_root: &Path,
    trusted_arg_values: &[String],
) -> Result<(), ProcessError> {
    if is_producer {
        return Ok(());
    }

    if program.contains(std::path::MAIN_SEPARATOR) || program.contains('/') {
        // Any path separator at all -- a bare name has neither, and resolves via `PATH`, never
        // against `worktree_path`.
        check_containment(program, worktree_path, repo_path, run_worktrees_root).map_err(
            |reason| ProcessError::UntrustedAgentProgram {
                role: role_name.to_string(),
                program: program.to_string(),
                reason,
            },
        )?;
    }

    for arg in args {
        let Some(candidate) = path_like_candidate(arg) else {
            continue;
        };
        if trusted_arg_values
            .iter()
            .any(|trusted| trusted == candidate)
        {
            continue;
        }
        check_containment(candidate, worktree_path, repo_path, run_worktrees_root).map_err(
            |reason| ProcessError::UntrustedAgentArg {
                role: role_name.to_string(),
                arg: arg.clone(),
                reason,
            },
        )?;
    }

    Ok(())
}

/// The containment check shared by `program` and path-like `args` entries.
fn check_containment(
    candidate: &str,
    worktree_path: &Path,
    repo_path: &Path,
    run_worktrees_root: &Path,
) -> Result<(), String> {
    let candidate_path = Path::new(candidate);
    if !candidate_path.is_absolute() {
        return Err(format!(
            "relative path -- would resolve against {}, the role's own worktree (a checkout of \
             the repo the coder can write to)",
            worktree_path.display()
        ));
    }

    let canonical_candidate = canonicalize_best_effort(candidate_path).map_err(|source| {
        format!(
            "cannot resolve its real location to verify it is outside the repo under review: \
             {source}"
        )
    })?;
    let canonical_worktree = canonicalize_best_effort(worktree_path).map_err(|source| {
        format!(
            "cannot resolve the role's own worktree ({}) to verify this is outside it: {source}",
            worktree_path.display()
        )
    })?;
    let canonical_repo = canonicalize_best_effort(repo_path).map_err(|source| {
        format!(
            "cannot resolve the run's base repository ({}) to verify this is outside it: \
             {source}",
            repo_path.display()
        )
    })?;
    let canonical_run_worktrees_root =
        canonicalize_best_effort(run_worktrees_root).map_err(|source| {
            format!(
                "cannot resolve this run's own worktrees root ({}) to verify this is outside \
                 it: {source}",
                run_worktrees_root.display()
            )
        })?;

    if canonical_candidate.starts_with(&canonical_worktree) {
        return Err(format!(
            "resolves inside the role's own worktree ({}) -- a checkout of the repo the coder \
             can write to",
            worktree_path.display()
        ));
    }
    if canonical_candidate.starts_with(&canonical_run_worktrees_root) {
        return Err(format!(
            "resolves inside this run's own worktrees ({}) -- e.g. the coder's, which the \
             coder writes to freely via Bash, including files it never commits",
            run_worktrees_root.display()
        ));
    }
    if canonical_candidate.starts_with(&canonical_repo) {
        return Err(format!(
            "resolves inside the run's base repository ({}), which the coder can write to and \
             commit into",
            repo_path.display()
        ));
    }

    Ok(())
}

fn path_like_candidate(arg: &str) -> Option<&str> {
    let candidate = if arg.starts_with('-') {
        arg.split_once('=').map_or(arg, |(_, value)| value)
    } else {
        arg
    };
    let candidate = strip_file_scheme(candidate);

    if candidate.is_empty() {
        return None;
    }

    if candidate.starts_with("./")
        || candidate.starts_with("../")
        || candidate.starts_with('~')
        || Path::new(candidate).is_absolute()
    {
        return Some(candidate);
    }

    let first_token = candidate.split_whitespace().next().unwrap_or(candidate);
    if has_non_filesystem_url_scheme(first_token) {
        return None;
    }
    let has_separator =
        first_token.contains(std::path::MAIN_SEPARATOR) || first_token.contains('/');
    if !has_separator {
        return None;
    }
    let separator_at = first_token
        .find([std::path::MAIN_SEPARATOR, '/'])
        .expect("has_separator checked above");
    if first_token[..separator_at].contains(SHELL_METACHARACTERS) {
        return None;
    }
    Some(first_token)
}

const SHELL_METACHARACTERS: &[char] = &[
    '=', '\'', '"', '$', '`', '(', ')', '{', '}', ';', '|', '&', '<', '>', '!', '*', '?', '[', ']',
];

/// Unwraps a `file://` URI so it receives normal filesystem containment checks.
fn strip_file_scheme(value: &str) -> &str {
    const FILE_SCHEME: &str = "file://";
    match value.get(..FILE_SCHEME.len()) {
        Some(prefix) if prefix.eq_ignore_ascii_case(FILE_SCHEME) => &value[FILE_SCHEME.len()..],
        _ => value,
    }
}

/// Schemes that name a resource fetched over a network protocol, never a local filesystem path --
/// the only schemes [`path_like_candidate`]'s weak tier treats as not-a-path.
const NON_FILESYSTEM_URL_SCHEMES: &[&str] =
    &["http", "https", "ssh", "git", "ftp", "ftps", "ws", "wss"];

fn has_non_filesystem_url_scheme(value: &str) -> bool {
    let Some(scheme_end) = value.find("://") else {
        return false;
    };
    let scheme = &value[..scheme_end];
    NON_FILESYSTEM_URL_SCHEMES
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(scheme))
}
