use super::*;

/// Guards a gated step's `program` and path-like `args` entries against
/// resolving to a path the producer step controls (issue #26, belt-and-
/// braces; extended from `program` to `args` by issue #59): no adapter
/// shipped today can actually trigger this (see this module's own docs),
/// but nothing stops a future one from naming a script inside the repo
/// under review -- as `program`, or tucked into `args` (e.g. `claude
/// --wrapper ./reviewer.sh`) -- and that would defeat the entire point of
/// running a gated step as an independent check. Always `Ok(())` for
/// `is_producer` -- the producer step (the coder in the built-in default
/// workflow) already runs with full repo access and is the repo's own
/// untrusted step in the first place (`agent_def`'s own module docs), so
/// there is nothing to protect it from here.
///
/// **Issue #73 (trio-unification follow-up)**: takes `role_name`/
/// `is_producer` rather than the closed `AgentRole` this used to -- every
/// workflow step goes through this exact same check now, keyed only on
/// whether it's the pipeline's producer (`workflow.steps[0]`, a positional
/// fact, not a role name), never on whether its name happens to be
/// `"coder"`/`"reviewer"`/`"tester"`. `role_name` is otherwise only used to
/// name the offending role in the returned error.
///
/// Refuses `program`, and any `args` entry [`path_like_candidate`] judges
/// path-like, when it is:
/// - **a relative path** (contains a path separator and is not absolute):
///   resolves against `worktree_path` (the child chdirs there before exec)
///   -- exactly the `./reviewer.sh`-means-the-coder's-own-copy hazard
///   [`spawn`]'s own docs describe.
/// - **an absolute path that resolves inside `worktree_path`, `repo_path`,
///   or `run_worktrees_root`**: the role's own checked-out worktree, the
///   run's base repository, or *any* role's worktree for this run
///   (`<warden_home>/worktrees/<run_id>/`). Issue #26 review, MEDIUM: the
///   original check covered only the checked role's own worktree and the
///   base repo, leaving the *coder's* own worktree
///   (`<run_worktrees_root>/coder`) unchecked -- the most coder-controllable
///   directory on disk, since the coder runs with `Bash` there and writes
///   freely, including files it never commits. `worktree_path` is always a
///   subdirectory of `run_worktrees_root`, so the `run_worktrees_root` check
///   alone already subsumes it; the separate `worktree_path` check is kept
///   only for the more specific error message when the program resolves
///   inside the checked role's *own* worktree specifically.
///
/// A **bare `program` name with no path separator at all** (`"claude"`,
/// `"echo"`) is always allowed: it resolves via `PATH`
/// (`Command::new`/`execvp` semantics), never against `worktree_path`, so it
/// carries none of the above hazard regardless of what the coder committed.
///
/// `args` entries follow the narrower [`path_like_candidate`] heuristic
/// instead (see its own docs), and a bare-name `args` entry is a genuine,
/// *undetected* gap, unlike a bare `program` (issue #59 review, MEDIUM):
/// `program`'s `PATH` reasoning above does not transfer to `args` -- an
/// `args` entry is interpreted by whatever tool `program` names, which
/// typically resolves a bare filename against its own current directory
/// (the role's own worktree, a checkout of the coder's commit), not `PATH`.
/// A future `--wrapper reviewer.sh` (no `./`) is therefore **not** caught by
/// this guard; only entries [`path_like_candidate`] judges path-like are
/// checked at all -- most `args` entries are ordinary values (`--model
/// sonnet`, a whole system prompt), never a path in the first place. Two
/// narrower gaps in the same spirit, added by review round 2's fix for a
/// whitespace-containing value: a path-shaped separator appearing after the
/// value's first whitespace-delimited token, and a path-shaped first token
/// that itself contains shell metacharacters (a quoted assignment, a `$(...)`
/// substitution) -- see [`path_like_candidate`]'s own docs for why both are
/// accepted trade-offs rather than closed further.
///
/// `trusted_arg_values` (issue #59 review, MEDIUM 4) is a caller-vouched
/// escape hatch for the residual false positive [`path_like_candidate`]'s
/// own docs describe: a value in this list is never subjected to the
/// containment check at all, regardless of what it looks like. The caller
/// (`orchestrator::agent_run`) is the only one who may vouch for a value,
/// and only ever does so for a value that provably came from **trusted
/// config**, never repo content -- see that call site's own docs for
/// exactly which values that is and why. An empty slice (as every non-agent
/// caller, and every test that isn't specifically exercising this hatch,
/// passes) means every path-like candidate is checked, unchanged from
/// before this hatch existed.
///
/// `worktree_path`, `repo_path`, and `run_worktrees_root` are all
/// canonicalized before each containment check (walking up to the nearest
/// existing ancestor for a candidate that doesn't exist on disk -- see
/// `canonicalize_best_effort`), so a `..`-laden or symlink-relative
/// `program`/arg can't slip past a purely lexical comparison. If
/// canonicalizing a candidate itself fails for a reason other than "doesn't
/// exist" (e.g. a permissions error walking its ancestors), this fails
/// closed naming that reason, rather than silently skipping the containment
/// check it could no longer perform (code-standards.md: "no silent
/// fallback").
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
        // Any path separator at all (checked for both this platform's own
        // separator and `/`, since a Windows build must still refuse a
        // Unix-style `agents/reviewer.sh` argument) -- a bare name has
        // neither, and resolves via `PATH`, never against `worktree_path`.
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
        // Issue #59 review, MEDIUM 4: a value the caller has explicitly
        // vouched for as trusted, non-path config (never repo content --
        // see this function's own docs) bypasses the containment check
        // entirely. Compared against the *extracted* candidate (post
        // `--flag=` splitting), not the raw `arg`, so this works
        // identically whether the caller's adapter emits `--model
        // <value>` (two argv entries) or `--model=<value>` (one).
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

/// The containment check shared by `program` and path-like `args` entries
/// (issue #59: previously duplicated per candidate inside
/// [`validate_agent_program`] itself, back when `program` was the only
/// candidate it ever checked). Returns `Ok(())` when `candidate` is outside
/// `worktree_path`, `repo_path`, and `run_worktrees_root`; `Err(reason)`
/// otherwise, whether because it resolves inside one of them or because
/// canonicalizing any of the four paths involved failed -- the caller wraps
/// `reason` into the typed error appropriate for what kind of candidate this
/// was (`program` vs. an `args` entry).
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
    // Issue #26 review, MEDIUM: catches a candidate under *another* role's
    // worktree for this same run (most importantly the coder's own,
    // `<run_worktrees_root>/coder` -- the coder writes there freely via
    // `Bash`, including files it never commits) -- the check above only
    // ever covers the checked role's own worktree.
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

/// Issue #59: decides which `args` entries [`validate_agent_program`] even
/// runs the containment check against, and -- for a `--flag=value` entry or
/// a `file://` URI -- which substring of it is actually the path to check.
/// Returns `None` for anything not worth checking at all.
///
/// `--flag=value` is split at the first `=` and only `value` is considered
/// further: `--wrapper=./reviewer.sh` is the same hazard as `--wrapper
/// ./reviewer.sh` split across two argv entries, just packed into one. Only
/// attempted when `arg` starts with `-` (the GNU-style long-flag convention
/// every shipped adapter uses), so a plain positional value that happens to
/// contain `=` isn't misread as a flag.
///
/// A `file://` URI is unwrapped to the path after the scheme
/// ([`strip_file_scheme`]) before anything else is judged -- see that
/// function's own docs on why `file` must never be treated as "just a URL".
///
/// The rule is then evaluated in two tiers, deliberately asymmetric (issue
/// #59 review round 2): a value can be a genuine filesystem path *with*
/// whitespace in it (`./sub dir/tool.sh`, `my tool.sh` inside a worktree the
/// coder wrote to via its `Bash` grant, ADR-0021 §3bis), so whitespace must
/// never blanket-exempt a value that is otherwise unambiguous evidence of a
/// path:
/// - **Strong evidence, checked regardless of whitespace, against the
///   *whole* value**: the value starts with `./`, `../`, or `~`, or is
///   itself absolute. Nothing exempts a value that matches this tier --
///   not whitespace, not a `scheme://` prefix (`Path::is_absolute` is
///   false for `sh://../coder/tool.sh`, so a value here never collides
///   with the URL check below in practice).
/// - **Weak evidence, judged from the value's *first whitespace-delimited
///   token only* (issue #59 review round 2, HIGH)**: that first token
///   alone -- not the rest of the value -- is what gets containment-checked
///   when it merely *contains* a path separator with no unambiguous prefix
///   (`agents/reviewer.sh`). This is deliberately narrower than "the whole
///   value contains a separator somewhere": a relative path missing only
///   its `./` prefix, packed into the same argv entry as unrelated trailing
///   words (`agents/evil script.sh`, or a hypothetical future `--wrapper
///   agents/reviewer.sh --verbose` collapsed into one string) still has
///   that shape in its first token and is still caught -- `check_containment`
///   refuses *any* relative candidate outright, so the first token alone is
///   enough to trigger that refusal, without needing to be the literal real
///   path on disk. A system prompt's first word essentially never looks
///   like a path (verified against every shipped adapter's `build_command`,
///   `ClaudeAdapter`/`CodexAdapter`/`MistralAdapter` in `tool_adapter.rs`:
///   each passes its role's entire system prompt as a single argv entry,
///   and all three built-in default prompts
///   (`DEFAULT_REVIEWER_PROMPT`/`DEFAULT_TESTER_PROMPT`) start with `"You
///   are Warden's ... agent."` -- first token `"You"`), even though the
///   prompt as a whole almost always contains a `/` somewhere later. A
///   single-word value (no whitespace at all) is its own first token, so
///   this subsumes the original single-word weak-tier rule exactly.
///   [`has_non_filesystem_url_scheme`] (a narrow, explicit allowlist of
///   schemes that name a resource fetched over a network protocol, never a
///   local filesystem path -- see its own docs for why this must be an
///   allowlist, not "anything with `://`") is checked against that same
///   first token.
///
/// **Residual gap, deliberately accepted (issue #59 review round 2)**: a
/// path-shaped separator anywhere *after* the first whitespace-delimited
/// token is never detected (`"please run agents/evil.sh now"` is exempt --
/// its first token, `"please"`, has no separator). Scanning every token
/// instead of just the first would reopen the exact false positive this
/// heuristic exists to avoid: real system prompts contain a `/` in
/// running prose (e.g. "reviewer/tester/CI"), just essentially never as the
/// *first* word. This is a narrower, harder-to-exploit variant of the
/// pre-existing bare-filename gap ([`validate_agent_program`]'s own docs) --
/// a future adapter would need to place its own wrapper path first in the
/// argv value for this guard to see it at all.
///
/// **Second residual gap, also deliberately accepted (issue #59 review
/// round 2)**: the first token is further required to contain none of
/// [`SHELL_METACHARACTERS`] before it counts as weak evidence -- found live
/// in this crate's own `orchestrator::convergence` test fixtures, which
/// construct a gated step's `args` directly as multi-line `sh -c` script
/// text (e.g. `"\n  dir='<temp path>'\n  n=$(cat \"$dir/count\" ...)\n..."`).
/// That script's first whitespace-delimited token after leading indentation,
/// `dir='<temp path>'`, contains a `/` (the embedded path is absolute) and
/// was being refused as a relative path by round 2's first cut of this fix
/// -- a real false positive, not a hypothetical one, on legitimate
/// shell-script `args` this codebase's own test suite already exercised.
/// The metacharacter check tells a bare relative path (`agents/evil.sh`:
/// only alphanumerics, `.`, `_`, `-`, `/`) apart from a shell-syntax
/// fragment that merely embeds one (a quoted assignment, a command
/// substitution, ...).
///
/// That check applies **only to the part of the token before its first
/// separator** (issue #59 test round 2). Applying it to the whole token was
/// itself a one-character bypass of exactly the kind round 2 closed for
/// whitespace: `agents/evil.sh` was refused but `agents/evil$.sh` was not,
/// and a coder picks the filenames in its own worktree, so evading the
/// guard cost it one character. Restricting the check to the leading
/// segment keeps the real false positive exempt -- in `dir='<temp path>'`
/// and `$(pwd)/tool.sh` the shell syntax sits before the separator, because
/// the separator belongs to a path the *shell* builds -- while an unusual
/// filename after the separator no longer buys anything.
///
/// This also still accepts the residual, narrower false-positive risk noted
/// before round 2's fix, mitigated by [`validate_agent_program`]'s own
/// `trusted_arg_values` escape hatch rather than special-cased further here:
/// a `model`/`tools` value that happens to be a single path-shaped token
/// with no whitespace (`anthropic/claude-3-opus`, `Bash(./script.sh)`) is
/// still refused by this heuristic alone. No shipped adapter's *default*
/// ever produces such a value (verified above); a caller with a genuine,
/// trusted one vouches for it explicitly instead.
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

    // Strong evidence: checked regardless of whitespace, against the whole
    // value -- see this function's own docs on why the first-token
    // narrowing below must never apply here.
    if candidate.starts_with("./")
        || candidate.starts_with("../")
        || candidate.starts_with('~')
        || Path::new(candidate).is_absolute()
    {
        return Some(candidate);
    }

    // Weak evidence: judged from the first whitespace-delimited token only
    // (issue #59 review round 2) -- see this function's own docs. For a
    // single-word `candidate` this token *is* `candidate`, unchanged from
    // the original single-word rule. `unwrap_or(candidate)` only matters for
    // a candidate that is entirely whitespace, which `candidate.is_empty()`
    // above does not catch -- `split_whitespace` yields nothing for it, and
    // falling back to `candidate` itself (whitespace, no separator) still
    // correctly resolves to "not path-like" below.
    let first_token = candidate.split_whitespace().next().unwrap_or(candidate);
    if has_non_filesystem_url_scheme(first_token) {
        return None;
    }
    let has_separator =
        first_token.contains(std::path::MAIN_SEPARATOR) || first_token.contains('/');
    if !has_separator {
        return None;
    }
    // Issue #59 test round 2: a token carrying shell syntax *before* its
    // first separator (`dir='/tmp/x'`, `$(pwd)/tool.sh`) merely embeds a
    // path -- the separator belongs to something the shell builds, not to a
    // path this argv entry names. A metacharacter *after* the first
    // separator is just an unusual filename (`agents/evil$.sh`), and a
    // coder picks its own filenames, so exempting those would hand back the
    // one-character bypass this round exists to close. See this function's
    // own docs.
    let separator_at = first_token
        .find([std::path::MAIN_SEPARATOR, '/'])
        .expect("has_separator checked above");
    if first_token[..separator_at].contains(SHELL_METACHARACTERS) {
        return None;
    }
    Some(first_token)
}

/// Characters that mark a whitespace-delimited token as shell syntax rather
/// than a bare filename/path -- see [`path_like_candidate`]'s own docs on
/// the real false positive (`dir='<temp path>'`, a quoted shell assignment)
/// this excludes from its weak-evidence tier.
const SHELL_METACHARACTERS: &[char] = &[
    '=', '\'', '"', '$', '`', '(', ')', '{', '}', ';', '|', '&', '<', '>', '!', '*', '?', '[', ']',
];

/// Unwraps a `file://` URI to the path after its scheme -- issue #59 review,
/// MEDIUM 2: a `file://` value **is** a filesystem path (unlike a genuine
/// network-protocol URL), so it must be resolved and containment-checked as
/// one, not exempted for merely looking like a URL. Case-insensitive on the
/// scheme itself (`FILE://`), matching real `file:` URI usage. Anything that
/// isn't a `file://` value is returned unchanged.
fn strip_file_scheme(value: &str) -> &str {
    const FILE_SCHEME: &str = "file://";
    match value.get(..FILE_SCHEME.len()) {
        Some(prefix) if prefix.eq_ignore_ascii_case(FILE_SCHEME) => &value[FILE_SCHEME.len()..],
        _ => value,
    }
}

/// Schemes that name a resource fetched over a network protocol, never a
/// local filesystem path -- the only schemes [`path_like_candidate`]'s weak
/// tier treats as not-a-path. Deliberately an **allowlist**, not "anything
/// with `://`" (issue #59 review, MEDIUM 2): the previous, broader rule
/// accepted *any* syntactically valid scheme in front of a relative path,
/// e.g. `sh://../coder/tool.sh` -- and a coder can `mkdir` a directory
/// literally named `sh:` inside its own worktree (`:` is a valid POSIX
/// filename character), making an invented scheme in front of a relative
/// path name a real coder-written file. `file` is deliberately *not* on
/// this list -- see [`strip_file_scheme`], which handles it before this is
/// ever consulted.
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
