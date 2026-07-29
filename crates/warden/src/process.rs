//! Subprocess Adapter (ADR-0005): spawns one child as a
//! `tokio::process::Command`, cancellable via a `CancellationToken`
//! (code-standards.md: "tokio pour l'annulation propre des sous-process").
//! stdout is captured and handed back as-is — parsing/validating it into
//! [`warden_core::Finding`]s happens at the boundary in `warden-core`, this
//! module never interprets agent output itself.
//!
//! **Issue #50: this is no longer the coder/reviewer/tester invocation
//! path.** Every agent now runs through `warden_sandbox::Sandbox`
//! (`warden_sandbox::LocalSandbox` by default -- a strict-parity port of
//! what [`spawn`]/[`wait`] used to do for that path, including its own
//! per-invocation env-allowlist forwarding and per-line progress callback),
//! wired in by `orchestrator::Orchestrator::run_agent`. [`spawn`]/[`wait`]
//! remain here for the one caller that still needs the general primitive
//! for a non-agent subprocess: the Evidence Capture Adapter (`evidence.rs`,
//! via [`spawn_and_wait`]).
//!
//! `spawn` and `wait` are split so a caller can persist the child's PID to
//! SQLite *before* awaiting completion — that's the same crash-detection
//! shape `warden_sandbox::Execution` gives the agent path (its own
//! `pid`/`wait` split, `warden_sandbox`'s own docs), just for a plain
//! subprocess here instead.
//!
//! [`wait`] writes an optional stdin payload (if any) and closes the write
//! half concurrently with draining stdout/stderr and awaiting exit — see its
//! own docs for why writing stdin any other way can deadlock.
//!
//! # Issue #26: [`validate_agent_program`], a belt-and-braces guard on `program` (and, since #59, `args`)
//!
//! No built-in [`crate::tool_adapter::ToolAdapter`] shipped today ever names
//! a `command.program` that resolves inside the repo under review --
//! `ClaudeAdapter::build_command` always names a bare `claude`, resolved via
//! `PATH` (see [`spawn`]'s own docs on why a relative program is otherwise
//! long-standing, accepted behaviour). But nothing in the type system stops
//! a *future* adapter from doing exactly that, and the entire point of
//! running the reviewer/tester as an independent gate (Architecture.md §1)
//! is that the coder must never control what they execute -- so this is
//! checked once, structurally, at the one call site every coder/reviewer/
//! tester spawn in this codebase goes through
//! (`orchestrator::Orchestrator::run_agent`), rather than trusted to stay
//! true of every adapter forever.
//!
//! **Issue #59** scopes this same reasoning to `args`, deliberately left
//! uncovered by #26: a future adapter naming something like `claude
//! --wrapper ./reviewer.sh` would reintroduce the exact hole #26 closes,
//! since `./reviewer.sh` still resolves against the role's own worktree --
//! `program` being clean says nothing about `args`. [`path_like_candidate`]
//! decides which `args` entries even get the containment check (a
//! conservative separator-based heuristic, not a per-adapter declaration of
//! which args are paths -- that would need a `ToolAdapter` API change this
//! issue deliberately avoids); see its own docs for the exact rule, and
//! [`validate_agent_program`]'s own docs for the residual gap this
//! heuristic does *not* close (a bare-name `args` entry with no separator
//! at all, e.g. `--wrapper reviewer.sh`) and the `trusted_arg_values`
//! escape hatch for a caller-vouched non-path value the heuristic would
//! otherwise misjudge.

use std::path::Path;

use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

use crate::error::ProcessError;
use crate::path_util::canonicalize_best_effort;

/// A single agent invocation to run in an isolated worktree.
#[derive(Debug, Clone)]
pub struct AgentCommand {
    pub program: String,
    pub args: Vec<String>,
}

impl AgentCommand {
    pub fn new(
        program: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }
}

/// Outcome of a completed (non-cancelled) agent invocation.
#[derive(Debug)]
pub struct AgentOutcome {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

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
/// sonnet`, a whole system prompt), never a path in the first place.
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
/// #59 review, HIGH): a value can be a genuine filesystem path *with*
/// whitespace in it (`./sub dir/tool.sh`, `my tool.sh` inside a worktree the
/// coder wrote to via its `Bash` grant, ADR-0021 §3bis), so whitespace must
/// never blanket-exempt a value that is otherwise unambiguous evidence of a
/// path:
/// - **Strong evidence, checked regardless of whitespace**: the value
///   starts with `./`, `../`, or `~`, or is itself absolute. Nothing
///   exempts a value that matches this tier -- not whitespace, not a
///   `scheme://` prefix (`Path::is_absolute` is false for
///   `sh://../coder/tool.sh`, so a value here never collides with the URL
///   check below in practice).
/// - **Weak evidence, exempted by whitespace or a non-filesystem URL
///   scheme**: the value merely *contains* a path separator somewhere,
///   with no unambiguous prefix (`agents/reviewer.sh`, or prose like
///   "reviewer/tester/CI raised" from a system prompt). Two exemptions
///   apply only here:
///   - **Whitespace.** Verified against every shipped adapter's
///     `build_command` (`ClaudeAdapter`/`CodexAdapter`/`MistralAdapter` in
///     `tool_adapter.rs`): each passes its role's entire system prompt as a
///     single argv entry, and all three built-in default prompts
///     (`DEFAULT_REVIEWER_PROMPT`/`DEFAULT_TESTER_PROMPT`, both starting
///     with `"You are Warden's ... agent."`) contain at least one `/`.
///     Without this exemption, restricted to this weak tier only, every
///     reviewer/tester invocation using a shipped adapter's default prompt
///     would be refused outright -- breaking the working product, not
///     closing a hole.
///   - [`has_non_filesystem_url_scheme`] -- a narrow, explicit allowlist of
///     schemes that name a resource fetched over a network protocol, never
///     a local filesystem path (see its own docs for why this must be an
///     allowlist, not "anything with `://`").
///
/// This intentionally accepts one residual, narrower false-positive risk on
/// the weak tier, mitigated by [`validate_agent_program`]'s own
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

    // Strong evidence: checked regardless of whitespace (issue #59 review,
    // HIGH) -- see this function's own docs on why the whitespace exemption
    // below must never reach this tier.
    if candidate.starts_with("./")
        || candidate.starts_with("../")
        || candidate.starts_with('~')
        || Path::new(candidate).is_absolute()
    {
        return Some(candidate);
    }

    // Weak evidence: only this tier gets the whitespace/URL-scheme
    // exemptions -- see this function's own docs.
    if candidate.contains(char::is_whitespace) {
        return None;
    }
    if has_non_filesystem_url_scheme(candidate) {
        return None;
    }
    let has_separator = candidate.contains(std::path::MAIN_SEPARATOR) || candidate.contains('/');
    has_separator.then_some(candidate)
}

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

/// Spawns `command` with `cwd` set (code-standards.md: "Agent Subprocess
/// Protocol"). The environment is not inherited from the current process —
/// `env_clear()` always runs first, only `PATH` forwarded on top
/// (Architecture.md §10, "Isolation environnement des sous-processus").
///
/// Issue #50 review, MEDIUM 3: this and [`wait`] no longer sit on the
/// coder/reviewer/tester invocation path at all — every agent runs through
/// `warden_sandbox::Sandbox` now (`warden_sandbox::LocalSandbox::execute` is
/// the strict-parity port of what used to live here, including its own
/// env-allowlist forwarding and per-line progress callback), routed via
/// `orchestrator::Orchestrator::run_agent`. What remains here is the
/// narrower subset the Evidence Capture Adapter (`evidence.rs`, via
/// [`spawn_and_wait`]) actually needs: no extra env allowlist, no per-line
/// callback — carrying that now-dead functionality forward as a second,
/// separately maintained copy of `LocalSandbox`'s own deadlock-avoidance
/// logic was exactly the drift risk two copies of the same subprocess-drain
/// code creates. `[`validate_agent_program`]` is unaffected by this — it is
/// still the one checkpoint every coder/reviewer/tester spawn goes through,
/// just called from `Orchestrator::run_agent` before the sandbox's own
/// `execute`, not before this function.
///
/// **A relative `command.program` resolves against `cwd`** — the child
/// chdirs before exec. Long-standing behaviour, documented here rather than
/// changed — refusing relative paths is a product decision, and it would
/// break the plain-script case a custom `AgentCommand` might still exist to
/// serve for a non-agent subprocess (evidence capture).
///
/// stdin is piped (ADR-0012, issue #20 Scope B heritage) rather than
/// inherited, so [`wait`]'s optional payload write never leaks the
/// orchestrator's own stdin into the child. A child that never reads stdin
/// at all is *not* unconditionally unaffected: a payload small enough to fit
/// in the OS pipe buffer (typically 64KiB) is written without blocking and
/// simply sits there unread until the child exits, but a larger payload
/// blocks [`wait`]'s write until either the child reads enough to make room
/// or exits and closes its read end (a broken pipe, handled explicitly —
/// see [`wait`]).
///
/// Returns the still-running [`Child`] so the caller can read its PID
/// (`child.id()`) and persist it before calling [`wait`].
pub fn spawn(command: &AgentCommand, cwd: &Path) -> Result<Child, ProcessError> {
    let mut cmd = Command::new(&command.program);
    cmd.args(&command.args)
        .current_dir(cwd)
        .env_clear()
        .kill_on_drop(true)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }

    cmd.spawn().map_err(|source| ProcessError::Spawn {
        command: command.program.clone(),
        source,
    })
}

/// Awaits a previously [`spawn`]ed child, cancellable via `cancel`.
///
/// If `cancel` fires first, the child is killed and
/// [`ProcessError::Cancelled`] is returned.
///
/// `stdin_payload`, if given, is written to the child's stdin and the write
/// half is then closed (dropped) so the child sees EOF rather than hanging
/// forever waiting for more input — this happens even when `stdin_payload`
/// is `None`, since a piped stdin ([`spawn`]) that's never closed would
/// otherwise hang a child that reads until EOF before proceeding.
///
/// **Deadlock avoidance**: the write, the stdout/stderr draining, and the
/// wait for exit all run *concurrently* (`tokio::join!`), not sequentially.
/// Writing the whole payload before draining anything (or draining only
/// after exit, as this function used to) risks a classic pipe deadlock: a
/// child that interleaves reading stdin with writing enough stdout/stderr to
/// fill the OS pipe buffer (typically 64KiB) before it has consumed all of
/// stdin will block on its own full stdout/stderr pipe; meanwhile we'd be
/// blocked writing to a stdin the child has stopped reading — neither side
/// can make progress. Running all four concurrently means each blocked
/// read/write just yields to the executor, and progress on any one of them
/// unblocks the others.
///
/// **Stdin write failures** (H1, issue #20 review): a broken pipe (the
/// child closed or never read stdin before exiting) is logged at `warn` and
/// treated as a normal, non-fatal outcome — see
/// [`classify_stdin_write_error`]. Any other write error fails this call
/// with [`ProcessError::StdinWrite`] instead of letting the run continue
/// silently: `stdin_payload` is always a single JSON object, so a partial
/// write is unparsable by the child by construction, and there is no
/// recovery short of failing the invocation.
///
/// Uses `child.wait()` (borrows `&mut self`) rather than
/// `wait_with_output()` (which consumes `self`) so `child` is still
/// available to `kill()` in the cancellation branch of the `select!` below.
pub async fn wait(
    mut child: Child,
    command_name: &str,
    stdin_payload: Option<String>,
    cancel: CancellationToken,
) -> Result<AgentOutcome, ProcessError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let stdin_handle = child.stdin.take();
    let stdout_handle = child.stdout.take();
    let stderr_handle = child.stderr.take();

    let stdin_task = async move {
        if let Some(mut stdin_handle) = stdin_handle {
            if let Some(payload) = stdin_payload {
                if let Err(error) = stdin_handle.write_all(payload.as_bytes()).await {
                    classify_stdin_write_error(error, command_name)?;
                }
            }
            // Dropping `stdin_handle` here (end of scope) closes the write
            // half, signalling EOF — required even with no payload to
            // write.
        }
        Ok::<(), std::io::Error>(())
    };
    let stdout_task = async move {
        let mut buf = Vec::new();
        if let Some(mut stdout_handle) = stdout_handle {
            if let Err(error) = stdout_handle.read_to_end(&mut buf).await {
                tracing::warn!(command = command_name, %error, "failed to read agent stdout to completion");
            }
        }
        buf
    };
    let stderr_task = async move {
        let mut buf = Vec::new();
        if let Some(mut stderr_handle) = stderr_handle {
            if let Err(error) = stderr_handle.read_to_end(&mut buf).await {
                tracing::warn!(command = command_name, %error, "failed to read agent stderr to completion");
            }
        }
        buf
    };

    tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            let _ = child.kill().await;
            Err(ProcessError::Cancelled { command: command_name.to_string() })
        }
        result = async {
            let (stdin_result, stdout_buf, stderr_buf, status_result) =
                tokio::join!(stdin_task, stdout_task, stderr_task, child.wait());
            let status = status_result.map_err(|source| ProcessError::Wait {
                command: command_name.to_string(),
                source,
            })?;
            // H1: a non-broken-pipe stdin write failure fails the
            // invocation outright rather than silently running the agent
            // with a partial (unparsable) or absent payload — the child has
            // already been awaited above via the same `join!`, so nothing
            // is left running when this returns.
            stdin_result.map_err(|source| ProcessError::StdinWrite {
                command: command_name.to_string(),
                source,
            })?;
            Ok(AgentOutcome {
                exit_code: status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&stdout_buf).into_owned(),
                stderr: String::from_utf8_lossy(&stderr_buf).into_owned(),
            })
        } => result,
    }
}

/// Classifies a stdin write failure (H1, issue #20 review): a broken pipe
/// means the agent closed or never opened its read side of stdin — e.g. it
/// exited before reading everything, or ignores stdin entirely and exits on
/// its own — which is a legitimate agent behaviour, logged at `warn` (never
/// silently dropped) but not fatal to the run. Any other error (disk full on
/// a buffered pipe implementation, permission error, etc.) is fatal: the
/// payload is a single JSON object, so a partial write is unparsable by the
/// agent by construction, and continuing would mean the agent runs with no
/// intent/context at all — exactly the silent fallback code-standards.md
/// forbids.
fn classify_stdin_write_error(
    error: std::io::Error,
    command_name: &str,
) -> Result<(), std::io::Error> {
    if error.kind() == std::io::ErrorKind::BrokenPipe {
        tracing::warn!(
            command = command_name,
            %error,
            "agent closed stdin before the full payload was written; continuing without a \
             guarantee it read the payload"
        );
        Ok(())
    } else {
        Err(error)
    }
}

/// Spawns `<tui_binary> attach --run-id <run_id> --warden-home <warden_home>`
/// in the foreground (issue #32, `warden run --tui`), attaching it to the run
/// that just started. Unlike [`spawn`], stdio is **inherited** rather than
/// piped -- the whole point is for this child to take over the launch
/// terminal exactly as if the user had typed the `warden-tui attach` command
/// themselves -- and the environment is inherited rather than cleared:
/// `warden-tui` is a trusted first-party binary from this same install, not
/// an agent under the Agent Subprocess Protocol (code-standards.md), so none
/// of that isolation applies to it.
///
/// `warden run --tui` treats this child's exit -- for *any* reason (the user
/// quit with `q`/`Esc`/Ctrl-C, or it was killed/crashed) -- as cancelling the
/// run (issue #32 decision: "la sortie de la TUI annule le run"). Ctrl-C
/// specifically needs `warden_tui`'s own `is_quit` to treat it as a quit key
/// first: raw mode disables the terminal's `SIGINT`-on-Ctrl-C generation
/// entirely (`cfmakeraw` clears `ISIG`), so relying on the signal reaching
/// this process's own group would not work while `warden-tui` holds the tty.
pub fn spawn_tui_attach(
    tui_binary: &Path,
    run_id: &str,
    warden_home: &Path,
) -> Result<Child, ProcessError> {
    Command::new(tui_binary)
        .arg("attach")
        .arg("--run-id")
        .arg(run_id)
        .arg("--warden-home")
        .arg(warden_home)
        .spawn()
        .map_err(|source| ProcessError::Spawn {
            command: tui_binary.display().to_string(),
            source,
        })
}

/// Convenience wrapper over [`spawn`] + [`wait`] for callers that don't
/// need the PID before completion (e.g. tests) or a stdin payload (e.g. the
/// Evidence Capture Adapter's `playwright`/`asciinema` invocations, which
/// aren't agents in the coder/reviewer/tester sense and receive no
/// intent/findings context).
pub async fn spawn_and_wait(
    command: &AgentCommand,
    cwd: &Path,
    cancel: CancellationToken,
) -> Result<AgentOutcome, ProcessError> {
    let child = spawn(command, cwd)?;
    wait(child, &command.program, None, cancel).await
}

/// Sentinel meaning "no process start time was recorded for this row" —
/// used for historical rows written before start-time tracking existed.
/// `0` is never a real Unix start time in practice (that would be 1970).
pub const UNKNOWN_START_TIME: i64 = 0;

/// Returns the OS-reported start time (seconds since the Unix epoch) of
/// `pid`, or `None` if no such process exists right now.
///
/// This is what lets [`is_process_alive`] tell a still-running process
/// apart from an unrelated process that happens to have reused the same PID
/// after a reboot: PIDs are a small, wrapping namespace recycled by the OS,
/// so a bare "does this PID exist" check is not sufficient for correctness
/// over the lifetime of a persisted run (H1 / crash recovery, Architecture
/// §9). A process's start time is immutable for its whole lifetime, so
/// re-reading it later and comparing against a value captured right after
/// spawn reliably detects PID reuse.
pub fn process_start_time(pid: u32) -> Option<i64> {
    if pid == 0 {
        return None;
    }
    // A single-PID refresh (not `ProcessesToUpdate::All`) keeps this cheap
    // enough to call synchronously from an async context — it's invoked at
    // most once per agent invocation, never in a hot loop.
    let mut system = sysinfo::System::new();
    system.refresh_processes(
        sysinfo::ProcessesToUpdate::Some(&[sysinfo::Pid::from_u32(pid)]),
        true,
    );
    system
        .process(sysinfo::Pid::from_u32(pid))
        .map(|process| process.start_time() as i64)
}

/// Checks whether `pid` still refers to the *same* process that was
/// recorded with `expected_start_time` (seconds since epoch), not merely
/// whether some process with that PID currently exists.
///
/// `pid == 0` is always reported not-alive: POSIX `kill(0, ...)` signals
/// the caller's entire process group rather than a single process, so a
/// naive `kill(pid, None)` liveness check against a pid-0 sentinel always
/// (mis)reports "alive" regardless of whether an agent is actually running
/// — this was H1, a real bug in the previous implementation.
///
/// If `expected_start_time` is [`UNKNOWN_START_TIME`] (no start time was
/// ever recorded for this row), falls back to a plain existence check —
/// strictly less safe against PID reuse, and logged as such.
pub fn is_process_alive(pid: u32, expected_start_time: i64) -> bool {
    if pid == 0 {
        return false;
    }

    let Some(actual_start_time) = process_start_time(pid) else {
        return false;
    };

    if expected_start_time == UNKNOWN_START_TIME {
        tracing::warn!(
            pid,
            "checking process liveness without a recorded start time; cannot rule out PID reuse"
        );
        return true;
    }

    actual_start_time == expected_start_time
}

/// Terminates `pid`, but only if it is still the exact process recorded at
/// `expected_start_time` (H1: PID-reuse hardening).
///
/// Deliberately does *not* call [`is_process_alive`] first and then act on
/// that answer: two separate `sysinfo` refreshes (one to check liveness,
/// another later to obtain a handle to kill) would leave a race window
/// between them where the OS could reuse `pid` for an unrelated process,
/// which this function would then signal by mistake. Instead, a *single*
/// refresh produces the exact process handle that is both fingerprint-
/// checked and killed, so there is no gap in which the PID can change
/// identity out from under this call.
///
/// Returns `Ok(())` if the process is already gone, or is no longer the one
/// recorded (fingerprint mismatch) — neither is an error, both just mean
/// there is nothing left to kill. `pid == 0` is always treated as
/// already-gone: see [`is_process_alive`] for why a pid-0 sentinel must
/// never be signalled.
pub fn kill_pid(pid: u32, expected_start_time: i64) -> Result<(), ProcessError> {
    if pid == 0 {
        return Ok(());
    }

    let mut system = sysinfo::System::new();
    system.refresh_processes(
        sysinfo::ProcessesToUpdate::Some(&[sysinfo::Pid::from_u32(pid)]),
        true,
    );

    let Some(process) = system.process(sysinfo::Pid::from_u32(pid)) else {
        // Nothing at this pid right now — already gone, nothing to do.
        return Ok(());
    };

    let actual_start_time = process.start_time() as i64;
    if expected_start_time == UNKNOWN_START_TIME {
        // Degraded case, same as `is_process_alive`: no fingerprint was ever
        // recorded for this row, so PID reuse can't be ruled out. Logged,
        // not refused — a historical row shouldn't be permanently
        // unreclaimable just because it predates start-time tracking.
        tracing::warn!(
            pid,
            "killing a process without a recorded start time; cannot rule out PID reuse"
        );
    } else if actual_start_time != expected_start_time {
        // The PID has been reused by an unrelated process since it was
        // recorded (H1) — on the very same handle we're about to kill, not
        // a separate, earlier check. Never signal it.
        return Ok(());
    }

    if process.kill() {
        Ok(())
    } else {
        Err(ProcessError::KillFailed { pid })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    // -----------------------------------------------------------------
    // `validate_agent_program` (issue #26, belt-and-braces)
    // -----------------------------------------------------------------

    /// A dedicated `<run_worktrees_root>/<role>` layout, mirroring what
    /// `WorktreeManager::create` actually produces
    /// (`<warden_home>/worktrees/<run_id>/<role>`) -- used by every test
    /// below instead of an unrelated bare `TempDir` for `worktree_path`, so
    /// the MEDIUM (issue #26 review) coverage of *other* roles' worktrees
    /// under the same `run_worktrees_root` has something real to check.
    struct WorktreeLayout {
        run_worktrees_root: TempDir,
    }

    impl WorktreeLayout {
        fn new() -> Self {
            Self {
                run_worktrees_root: TempDir::new().unwrap(),
            }
        }

        fn role_worktree(&self, role: &str) -> PathBuf {
            let path = self.run_worktrees_root.path().join(role);
            std::fs::create_dir_all(&path).unwrap();
            path
        }
    }

    #[test]
    fn a_bare_program_name_with_no_separator_is_always_allowed_for_reviewer_and_tester() {
        let layout = WorktreeLayout::new();
        let worktree = layout.role_worktree("reviewer");
        let repo = TempDir::new().unwrap();
        for role in ["reviewer", "tester"] {
            assert!(validate_agent_program(
                role,
                false,
                "claude",
                &[],
                &worktree,
                repo.path(),
                layout.run_worktrees_root.path(),
                &[],
            )
            .is_ok());
        }
    }

    #[test]
    fn a_relative_path_is_refused_for_reviewer_and_tester() {
        let layout = WorktreeLayout::new();
        let worktree = layout.role_worktree("reviewer");
        let repo = TempDir::new().unwrap();
        for role in ["reviewer", "tester"] {
            let error = validate_agent_program(
                role,
                false,
                "./reviewer.sh",
                &[],
                &worktree,
                repo.path(),
                layout.run_worktrees_root.path(),
                &[],
            )
            .unwrap_err();
            assert!(matches!(error, ProcessError::UntrustedAgentProgram { .. }));
            assert!(error.to_string().contains("./reviewer.sh"), "{error}");
        }
    }

    #[test]
    fn an_absolute_path_inside_the_role_worktree_is_refused() {
        let layout = WorktreeLayout::new();
        let worktree = layout.role_worktree("reviewer");
        let repo = TempDir::new().unwrap();
        let program = worktree.join("reviewer.sh");
        std::fs::write(&program, "#!/bin/sh\n").unwrap();

        let error = validate_agent_program(
            "reviewer",
            false,
            program.to_str().unwrap(),
            &[],
            &worktree,
            repo.path(),
            layout.run_worktrees_root.path(),
            &[],
        )
        .unwrap_err();
        assert!(matches!(error, ProcessError::UntrustedAgentProgram { .. }));
    }

    #[test]
    fn an_absolute_path_inside_the_run_base_repo_is_refused() {
        let layout = WorktreeLayout::new();
        let worktree = layout.role_worktree("tester");
        let repo = TempDir::new().unwrap();
        let program = repo.path().join(".warden/agents/reviewer.sh");
        std::fs::create_dir_all(program.parent().unwrap()).unwrap();
        std::fs::write(&program, "#!/bin/sh\n").unwrap();

        let error = validate_agent_program(
            "tester",
            false,
            program.to_str().unwrap(),
            &[],
            &worktree,
            repo.path(),
            layout.run_worktrees_root.path(),
            &[],
        )
        .unwrap_err();
        assert!(matches!(error, ProcessError::UntrustedAgentProgram { .. }));
    }

    /// Issue #26 review, MEDIUM: the original guard only checked the
    /// checked role's own worktree and the base repo -- leaving the
    /// *coder's* own worktree, under the same `run_worktrees_root`, entirely
    /// unchecked even though it is the most coder-controllable directory on
    /// disk (the coder runs with `Bash` there and writes freely, including
    /// files it never commits). A reviewer `program` naming a script under
    /// the coder's worktree must now be refused too.
    #[test]
    fn an_absolute_path_inside_the_coders_own_worktree_for_this_run_is_refused() {
        let layout = WorktreeLayout::new();
        let reviewer_worktree = layout.role_worktree("reviewer");
        let coder_worktree = layout.role_worktree("coder");
        let repo = TempDir::new().unwrap();
        let program = coder_worktree.join("tool.sh");
        std::fs::write(&program, "#!/bin/sh\n").unwrap();

        let error = validate_agent_program(
            "reviewer",
            false,
            program.to_str().unwrap(),
            &[],
            &reviewer_worktree,
            repo.path(),
            layout.run_worktrees_root.path(),
            &[],
        )
        .unwrap_err();
        assert!(matches!(error, ProcessError::UntrustedAgentProgram { .. }));
        assert!(error.to_string().contains("run's own worktrees"), "{error}");
    }

    #[test]
    fn an_absolute_path_outside_the_worktree_the_repo_and_the_run_worktrees_root_is_allowed() {
        let layout = WorktreeLayout::new();
        let worktree = layout.role_worktree("reviewer");
        let repo = TempDir::new().unwrap();
        let elsewhere = TempDir::new().unwrap();
        let program = elsewhere.path().join("some-tool");
        std::fs::write(&program, "#!/bin/sh\n").unwrap();

        assert!(validate_agent_program(
            "reviewer",
            false,
            program.to_str().unwrap(),
            &[],
            &worktree,
            repo.path(),
            layout.run_worktrees_root.path(),
            &[],
        )
        .is_ok());
    }

    /// The whole point of this guard: it must never apply to the producer
    /// step (the coder in the built-in default workflow), which already has
    /// full repo access and is the repo's own untrusted step in the first
    /// place -- even a program that would be refused for a gated step must
    /// pass unchanged for the producer.
    #[test]
    fn the_producer_step_is_never_subject_to_this_guard() {
        let layout = WorktreeLayout::new();
        let worktree = layout.role_worktree("coder");
        let repo = TempDir::new().unwrap();
        let program = repo.path().join(".warden/agents/coder.sh");
        std::fs::create_dir_all(program.parent().unwrap()).unwrap();
        std::fs::write(&program, "#!/bin/sh\n").unwrap();

        assert!(validate_agent_program(
            "coder",
            true,
            program.to_str().unwrap(),
            &[],
            &worktree,
            repo.path(),
            layout.run_worktrees_root.path(),
            &[],
        )
        .is_ok());
        assert!(validate_agent_program(
            "coder",
            true,
            "./coder.sh",
            &[],
            &worktree,
            repo.path(),
            layout.run_worktrees_root.path(),
            &[],
        )
        .is_ok());
    }

    /// A `program` that doesn't exist on disk at all must still be checked
    /// against the containment rule (via `canonicalize_best_effort`'s
    /// ancestor walk), not silently allowed just because it can't be
    /// canonicalized outright.
    #[test]
    fn a_nonexistent_absolute_path_inside_the_worktree_is_still_refused() {
        let layout = WorktreeLayout::new();
        let worktree = layout.role_worktree("reviewer");
        let repo = TempDir::new().unwrap();
        let program = worktree.join("does-not-exist-yet.sh");

        let error = validate_agent_program(
            "reviewer",
            false,
            program.to_str().unwrap(),
            &[],
            &worktree,
            repo.path(),
            layout.run_worktrees_root.path(),
            &[],
        )
        .unwrap_err();
        assert!(matches!(error, ProcessError::UntrustedAgentProgram { .. }));
    }

    // -----------------------------------------------------------------
    // `validate_agent_program`, `args` coverage (issue #59)
    // -----------------------------------------------------------------

    #[test]
    fn a_relative_path_arg_is_refused_for_reviewer_and_tester() {
        let layout = WorktreeLayout::new();
        let worktree = layout.role_worktree("reviewer");
        let repo = TempDir::new().unwrap();
        for role in ["reviewer", "tester"] {
            let error = validate_agent_program(
                role,
                false,
                "claude",
                &["--wrapper".to_string(), "./reviewer.sh".to_string()],
                &worktree,
                repo.path(),
                layout.run_worktrees_root.path(),
                &[],
            )
            .unwrap_err();
            assert!(matches!(error, ProcessError::UntrustedAgentArg { .. }));
            assert!(error.to_string().contains("./reviewer.sh"), "{error}");
        }
    }

    #[test]
    fn an_absolute_arg_inside_the_role_worktree_is_refused() {
        let layout = WorktreeLayout::new();
        let worktree = layout.role_worktree("reviewer");
        let repo = TempDir::new().unwrap();
        let wrapper = worktree.join("reviewer.sh");
        std::fs::write(&wrapper, "#!/bin/sh\n").unwrap();

        let error = validate_agent_program(
            "reviewer",
            false,
            "claude",
            &[
                "--wrapper".to_string(),
                wrapper.to_str().unwrap().to_string(),
            ],
            &worktree,
            repo.path(),
            layout.run_worktrees_root.path(),
            &[],
        )
        .unwrap_err();
        assert!(matches!(error, ProcessError::UntrustedAgentArg { .. }));
    }

    #[test]
    fn an_absolute_arg_inside_the_run_base_repo_is_refused() {
        let layout = WorktreeLayout::new();
        let worktree = layout.role_worktree("tester");
        let repo = TempDir::new().unwrap();
        let wrapper = repo.path().join(".warden/agents/reviewer.sh");
        std::fs::create_dir_all(wrapper.parent().unwrap()).unwrap();
        std::fs::write(&wrapper, "#!/bin/sh\n").unwrap();

        let error = validate_agent_program(
            "tester",
            false,
            "claude",
            &[
                "--wrapper".to_string(),
                wrapper.to_str().unwrap().to_string(),
            ],
            &worktree,
            repo.path(),
            layout.run_worktrees_root.path(),
            &[],
        )
        .unwrap_err();
        assert!(matches!(error, ProcessError::UntrustedAgentArg { .. }));
    }

    /// Mirrors the `program` coverage (issue #26 review, MEDIUM): an `args`
    /// entry resolving inside *another* role's worktree for this run --
    /// most importantly the coder's own -- must be refused too, not just
    /// the checked role's own worktree.
    #[test]
    fn an_absolute_arg_inside_the_coders_own_worktree_for_this_run_is_refused() {
        let layout = WorktreeLayout::new();
        let reviewer_worktree = layout.role_worktree("reviewer");
        let coder_worktree = layout.role_worktree("coder");
        let repo = TempDir::new().unwrap();
        let wrapper = coder_worktree.join("tool.sh");
        std::fs::write(&wrapper, "#!/bin/sh\n").unwrap();

        let error = validate_agent_program(
            "reviewer",
            false,
            "claude",
            &[
                "--wrapper".to_string(),
                wrapper.to_str().unwrap().to_string(),
            ],
            &reviewer_worktree,
            repo.path(),
            layout.run_worktrees_root.path(),
            &[],
        )
        .unwrap_err();
        assert!(matches!(error, ProcessError::UntrustedAgentArg { .. }));
        assert!(error.to_string().contains("run's own worktrees"), "{error}");
    }

    #[test]
    fn an_absolute_arg_outside_the_worktree_the_repo_and_the_run_worktrees_root_is_allowed() {
        let layout = WorktreeLayout::new();
        let worktree = layout.role_worktree("reviewer");
        let repo = TempDir::new().unwrap();
        let elsewhere = TempDir::new().unwrap();
        let wrapper = elsewhere.path().join("some-tool");
        std::fs::write(&wrapper, "#!/bin/sh\n").unwrap();

        assert!(validate_agent_program(
            "reviewer",
            false,
            "claude",
            &[
                "--wrapper".to_string(),
                wrapper.to_str().unwrap().to_string()
            ],
            &worktree,
            repo.path(),
            layout.run_worktrees_root.path(),
            &[],
        )
        .is_ok());
    }

    /// The exact false positive the issue calls out by name: an ordinary
    /// `--flag value` pair (no path separator anywhere) must never be
    /// treated as path-like.
    #[test]
    fn an_ordinary_non_path_arg_is_allowed() {
        let layout = WorktreeLayout::new();
        let worktree = layout.role_worktree("reviewer");
        let repo = TempDir::new().unwrap();

        assert!(validate_agent_program(
            "reviewer",
            false,
            "claude",
            &["--model".to_string(), "sonnet".to_string()],
            &worktree,
            repo.path(),
            layout.run_worktrees_root.path(),
            &[],
        )
        .is_ok());
    }

    /// The other false positive the issue calls out by name: a URL contains
    /// a path separator (`://`) but is not a filesystem path.
    #[test]
    fn a_url_arg_is_allowed() {
        let layout = WorktreeLayout::new();
        let worktree = layout.role_worktree("reviewer");
        let repo = TempDir::new().unwrap();

        assert!(validate_agent_program(
            "reviewer",
            false,
            "claude",
            &[
                "--endpoint".to_string(),
                "https://example.com/reviewer.sh".to_string()
            ],
            &worktree,
            repo.path(),
            layout.run_worktrees_root.path(),
            &[],
        )
        .is_ok());
    }

    /// The false positive verified live against this codebase's own shipped
    /// adapters: every one of them passes its role's entire system prompt as
    /// a single argv entry, and all three built-in default prompts contain
    /// at least one `/` -- without the whitespace exception, this would
    /// refuse every reviewer/tester invocation using a shipped adapter's
    /// default prompt.
    #[test]
    fn a_multi_word_value_containing_a_separator_is_allowed() {
        let layout = WorktreeLayout::new();
        let worktree = layout.role_worktree("reviewer");
        let repo = TempDir::new().unwrap();

        assert!(validate_agent_program(
            "reviewer",
            false,
            "claude",
            &[
                "--append-system-prompt".to_string(),
                "issues a prior reviewer/tester/CI raised".to_string()
            ],
            &worktree,
            repo.path(),
            layout.run_worktrees_root.path(),
            &[],
        )
        .is_ok());
    }

    /// `--flag=value` packs the flag and its value into one argv entry --
    /// the value after the first `=` must still be checked.
    #[test]
    fn a_flag_equals_relative_path_form_is_refused() {
        let layout = WorktreeLayout::new();
        let worktree = layout.role_worktree("reviewer");
        let repo = TempDir::new().unwrap();

        let error = validate_agent_program(
            "reviewer",
            false,
            "claude",
            &["--wrapper=./reviewer.sh".to_string()],
            &worktree,
            repo.path(),
            layout.run_worktrees_root.path(),
            &[],
        )
        .unwrap_err();
        assert!(matches!(error, ProcessError::UntrustedAgentArg { .. }));
        assert!(error.to_string().contains("./reviewer.sh"), "{error}");
    }

    /// The `args` check is subject to the exact same `is_producer` exemption
    /// as `program` -- the coder must never be refused an argument that
    /// would be refused for a gated step.
    #[test]
    fn the_producer_step_is_never_subject_to_the_args_guard() {
        let layout = WorktreeLayout::new();
        let worktree = layout.role_worktree("coder");
        let repo = TempDir::new().unwrap();

        assert!(validate_agent_program(
            "coder",
            true,
            "claude",
            &["--wrapper".to_string(), "./coder.sh".to_string()],
            &worktree,
            repo.path(),
            layout.run_worktrees_root.path(),
            &[],
        )
        .is_ok());
    }

    /// End-to-end regression for the false positive this guard must never
    /// reintroduce (issue #59): every shipped `ToolAdapter`'s *real*
    /// `build_command`, fed its own default reviewer/tester prompt (not a
    /// hand-picked string), must still pass `validate_agent_program`. This
    /// is what actually caught the system-prompt false positive during
    /// implementation -- `path_like_candidate`'s whitespace exception exists
    /// because this test failed without it.
    #[test]
    fn every_shipped_adapters_default_command_for_reviewer_and_tester_passes_the_guard() {
        use crate::tool_adapter::{ClaudeAdapter, CodexAdapter, MistralAdapter, ToolAdapter};
        use warden_core::{AgentDefinition, AgentRole};

        let layout = WorktreeLayout::new();
        let repo = TempDir::new().unwrap();

        fn check(
            adapter: &impl ToolAdapter,
            role: AgentRole,
            role_name: &str,
            layout: &WorktreeLayout,
            repo: &TempDir,
        ) {
            let worktree = layout.role_worktree(role_name);
            let definition = AgentDefinition::new(
                None,
                None,
                adapter.default_tools(role).map(str::to_string),
                None,
                adapter.default_prompt(role),
            )
            .unwrap();
            let command = adapter.build_command(&definition).unwrap();

            assert!(
                validate_agent_program(
                    role_name,
                    false,
                    &command.program,
                    &command.args,
                    &worktree,
                    repo.path(),
                    layout.run_worktrees_root.path(),
                    &[],
                )
                .is_ok(),
                "{role_name} via {} was refused for its own default command: {:?}",
                std::any::type_name_of_val(adapter),
                command.args
            );
        }

        for (role, role_name) in [
            (AgentRole::Reviewer, "reviewer"),
            (AgentRole::Tester, "tester"),
        ] {
            check(&ClaudeAdapter, role, role_name, &layout, &repo);
            check(&CodexAdapter, role, role_name, &layout, &repo);
            check(&MistralAdapter, role, role_name, &layout, &repo);
        }
    }

    // -----------------------------------------------------------------
    // `path_like_candidate` heuristic hardening (issue #59 review)
    // -----------------------------------------------------------------

    /// Issue #59 review, HIGH: a whitespace-containing value that is
    /// otherwise unambiguous evidence of a path (absolute, inside the
    /// coder's own worktree) must still be refused -- POSIX paths can
    /// contain spaces, and the coder can write a file literally named
    /// `my tool.sh` inside its own worktree via its `Bash` grant. Before
    /// the fix this was a one-character bypass of the whole guard.
    #[test]
    fn an_absolute_arg_with_a_space_inside_the_coders_worktree_is_still_refused() {
        let layout = WorktreeLayout::new();
        let reviewer_worktree = layout.role_worktree("reviewer");
        let coder_worktree = layout.role_worktree("coder");
        let repo = TempDir::new().unwrap();
        let wrapper = coder_worktree.join("my tool.sh");
        std::fs::write(&wrapper, "#!/bin/sh\n").unwrap();

        let error = validate_agent_program(
            "reviewer",
            false,
            "claude",
            &[
                "--wrapper".to_string(),
                wrapper.to_str().unwrap().to_string(),
            ],
            &reviewer_worktree,
            repo.path(),
            layout.run_worktrees_root.path(),
            &[],
        )
        .unwrap_err();
        assert!(matches!(error, ProcessError::UntrustedAgentArg { .. }));
    }

    /// Issue #59 review, HIGH: the relative-path counterpart -- an
    /// unambiguous `./`-prefixed value with a space in it must be refused
    /// exactly like one without a space.
    #[test]
    fn a_relative_arg_with_a_space_is_still_refused() {
        let layout = WorktreeLayout::new();
        let worktree = layout.role_worktree("reviewer");
        let repo = TempDir::new().unwrap();

        let error = validate_agent_program(
            "reviewer",
            false,
            "claude",
            &["--wrapper".to_string(), "./sub dir/tool.sh".to_string()],
            &worktree,
            repo.path(),
            layout.run_worktrees_root.path(),
            &[],
        )
        .unwrap_err();
        assert!(matches!(error, ProcessError::UntrustedAgentArg { .. }));
        assert!(error.to_string().contains("./sub dir/tool.sh"), "{error}");
    }

    /// Issue #59 review, MEDIUM 2: a `file://` URI names a real filesystem
    /// path and must be resolved and refused exactly like the equivalent
    /// bare absolute path -- it must never be laundered through the
    /// URL-scheme exemption just because it looks like a URL.
    #[test]
    fn a_file_url_onto_the_coders_worktree_is_refused() {
        let layout = WorktreeLayout::new();
        let reviewer_worktree = layout.role_worktree("reviewer");
        let coder_worktree = layout.role_worktree("coder");
        let repo = TempDir::new().unwrap();
        let wrapper = coder_worktree.join("tool.sh");
        std::fs::write(&wrapper, "#!/bin/sh\n").unwrap();
        let file_url = format!("file://{}", wrapper.to_str().unwrap());

        let error = validate_agent_program(
            "reviewer",
            false,
            "claude",
            &["--wrapper".to_string(), file_url],
            &reviewer_worktree,
            repo.path(),
            layout.run_worktrees_root.path(),
            &[],
        )
        .unwrap_err();
        assert!(matches!(error, ProcessError::UntrustedAgentArg { .. }));
    }

    /// Issue #59 review, MEDIUM 2: an invented scheme in front of a
    /// relative path must not be treated as a URL -- `://` alone is not
    /// evidence of a genuine network-protocol URL (`has_url_scheme`'s
    /// previous, over-broad RFC 3986 grammar check accepted any
    /// syntactically valid scheme, including one the coder can `mkdir`
    /// literally as a directory name, e.g. `sh:`).
    #[test]
    fn an_invented_scheme_in_front_of_a_relative_path_does_not_bypass_the_guard() {
        let layout = WorktreeLayout::new();
        let reviewer_worktree = layout.role_worktree("reviewer");
        let coder_worktree = layout.role_worktree("coder");
        let repo = TempDir::new().unwrap();
        // Mirrors the coder's own worktree in the arg text, the same way a
        // real relative-path wrapper would resolve against it -- the exact
        // value doesn't need to exist on disk for the relative-path branch.
        let arg = format!(
            "sh://../{}/tool.sh",
            coder_worktree.file_name().unwrap().to_str().unwrap()
        );

        let error = validate_agent_program(
            "reviewer",
            false,
            "claude",
            &["--wrapper".to_string(), arg],
            &reviewer_worktree,
            repo.path(),
            layout.run_worktrees_root.path(),
            &[],
        )
        .unwrap_err();
        assert!(matches!(error, ProcessError::UntrustedAgentArg { .. }));
    }

    /// A genuine network-protocol URL (on the allowlist) must still be
    /// allowed -- the fix for finding 2 must not regress the original
    /// false-positive fix it's built on top of.
    #[test]
    fn a_genuine_https_url_is_still_allowed() {
        let layout = WorktreeLayout::new();
        let worktree = layout.role_worktree("reviewer");
        let repo = TempDir::new().unwrap();

        assert!(validate_agent_program(
            "reviewer",
            false,
            "claude",
            &[
                "--endpoint".to_string(),
                "https://example.com/reviewer.sh".to_string()
            ],
            &worktree,
            repo.path(),
            layout.run_worktrees_root.path(),
            &[],
        )
        .is_ok());
    }

    /// Independent verification (issue #59 QA pass): a symlink whose *link
    /// text* lives well outside every forbidden root, but that resolves to a
    /// real file inside the coder's own worktree, must still be refused --
    /// the coder can create a symlink anywhere it has write access (its own
    /// worktree, e.g. via a checked-in symlink or something a build step
    /// produces) pointing back at a script it controls; if the guard only
    /// compared the *lexical* candidate string against the forbidden roots,
    /// a symlink like this would sail through. `canonicalize_best_effort`
    /// resolving via `std::fs::canonicalize` (which follows symlinks) is
    /// what closes this -- this pins that behaviour down as a guarantee, not
    /// an incidental property.
    #[cfg(unix)]
    #[test]
    fn a_symlink_outside_every_forbidden_root_that_resolves_into_the_coders_worktree_is_refused() {
        use std::os::unix::fs::symlink;

        let layout = WorktreeLayout::new();
        let reviewer_worktree = layout.role_worktree("reviewer");
        let coder_worktree = layout.role_worktree("coder");
        let repo = TempDir::new().unwrap();
        let real_target = coder_worktree.join("real-tool.sh");
        std::fs::write(&real_target, "#!/bin/sh\n").unwrap();
        // The symlink itself lives in a directory unrelated to any of the
        // three forbidden roots -- only its *target* is coder-controlled.
        let elsewhere = TempDir::new().unwrap();
        let link = elsewhere.path().join("innocuous-name.sh");
        symlink(&real_target, &link).unwrap();

        let error = validate_agent_program(
            "reviewer",
            false,
            "claude",
            &["--wrapper".to_string(), link.to_str().unwrap().to_string()],
            &reviewer_worktree,
            repo.path(),
            layout.run_worktrees_root.path(),
            &[],
        )
        .unwrap_err();
        assert!(matches!(error, ProcessError::UntrustedAgentArg { .. }));
    }

    /// The mirror image, and not a security concern: a symlink that lives
    /// *inside* the role's own worktree but resolves to a real file genuinely
    /// outside every forbidden root must still be allowed -- exactly as a
    /// bare absolute path to that same external location already is
    /// (`an_absolute_arg_outside_the_worktree_the_repo_and_the_run_worktrees_root_is_allowed`).
    /// This exists to pin down that the containment check compares the
    /// symlink's *resolved* location, not merely whether its literal path
    /// string happens to start with `worktree_path` -- a lexical-only
    /// comparison would wrongly refuse this.
    #[cfg(unix)]
    #[test]
    fn a_symlink_inside_the_worktree_that_resolves_outside_every_forbidden_root_is_allowed() {
        use std::os::unix::fs::symlink;

        let layout = WorktreeLayout::new();
        let reviewer_worktree = layout.role_worktree("reviewer");
        let repo = TempDir::new().unwrap();
        let elsewhere = TempDir::new().unwrap();
        let real_target = elsewhere.path().join("legit-tool.sh");
        std::fs::write(&real_target, "#!/bin/sh\n").unwrap();
        let link = reviewer_worktree.join("link-out.sh");
        symlink(&real_target, &link).unwrap();

        assert!(validate_agent_program(
            "reviewer",
            false,
            "claude",
            &["--wrapper".to_string(), link.to_str().unwrap().to_string(),],
            &reviewer_worktree,
            repo.path(),
            layout.run_worktrees_root.path(),
            &[],
        )
        .is_ok());
    }

    /// Independent verification (issue #59 QA pass): `..` traversal must be
    /// resolved before the containment check runs, not compared lexically --
    /// an absolute candidate whose *literal* prefix names the reviewer's own
    /// (already-permitted) worktree, but that walks back out via `..` into
    /// the coder's, must still be refused. Every segment here genuinely
    /// exists on disk, so `canonicalize_best_effort` resolves the whole
    /// thing via a single real `std::fs::canonicalize` call (the OS's own
    /// `..` handling), not the best-effort ancestor-walking fallback --
    /// see `path_util::canonicalize_best_effort`'s own docs for why a
    /// not-yet-existing path is a materially different (and, for this
    /// guard's purposes, inert -- see this crate's own test-derived
    /// findings) case.
    #[test]
    fn a_dotdot_traversal_from_the_role_worktree_into_the_coders_worktree_is_refused() {
        let layout = WorktreeLayout::new();
        let reviewer_worktree = layout.role_worktree("reviewer");
        let coder_worktree = layout.role_worktree("coder");
        let repo = TempDir::new().unwrap();
        let tool = coder_worktree.join("tool.sh");
        std::fs::write(&tool, "#!/bin/sh\n").unwrap();
        let traversal_arg = format!("{}/../coder/tool.sh", reviewer_worktree.display());

        let error = validate_agent_program(
            "reviewer",
            false,
            "claude",
            &["--wrapper".to_string(), traversal_arg],
            &reviewer_worktree,
            repo.path(),
            layout.run_worktrees_root.path(),
            &[],
        )
        .unwrap_err();
        assert!(matches!(error, ProcessError::UntrustedAgentArg { .. }));
    }

    /// Independent verification (issue #59 QA pass): [`strip_file_scheme`]'s
    /// own docs claim case-insensitivity (`FILE://`) but no existing test
    /// actually exercised anything but a lowercase `file://` -- a mixed-case
    /// scheme must not launder a coder-controlled path past the
    /// containment check.
    #[test]
    fn a_mixed_case_file_scheme_onto_the_coders_worktree_is_refused() {
        let layout = WorktreeLayout::new();
        let reviewer_worktree = layout.role_worktree("reviewer");
        let coder_worktree = layout.role_worktree("coder");
        let repo = TempDir::new().unwrap();
        let tool = coder_worktree.join("tool.sh");
        std::fs::write(&tool, "#!/bin/sh\n").unwrap();
        let arg = format!("FiLe://{}", tool.to_str().unwrap());

        let error = validate_agent_program(
            "reviewer",
            false,
            "claude",
            &["--wrapper".to_string(), arg],
            &reviewer_worktree,
            repo.path(),
            layout.run_worktrees_root.path(),
            &[],
        )
        .unwrap_err();
        assert!(matches!(error, ProcessError::UntrustedAgentArg { .. }));
    }

    /// **Independent verification, real defect found (issue #59 QA pass):**
    /// `path_like_candidate`'s weak tier exempts *any* whitespace-containing
    /// value unconditionally -- but the "strong evidence" tier that survives
    /// whitespace only recognises `./`, `../`, `~`, or an absolute prefix
    /// (see that function's own docs). A **relative** path that contains a
    /// separator *and* whitespace but does not start with `./`/`../`/`~`
    /// (e.g. `agents/evil script.sh`, mirroring `./sub dir/tool.sh` from
    /// `a_relative_arg_with_a_space_is_still_refused` minus only the leading
    /// `./`) falls into neither tier's protection: it is weak evidence
    /// (has a separator, no unambiguous prefix) *and* contains whitespace,
    /// so [`path_like_candidate`] returns `None` for it and it is never
    /// containment-checked at all -- even though it is exactly the same
    /// coder-controlled-relative-path hazard `check_containment`'s own docs
    /// describe ("resolves against `worktree_path`, the role's own
    /// worktree... which the coder can write to"). The coder can create a
    /// file with a literal space in its own worktree via its `Bash` grant
    /// (the same premise the whitespace exemption itself relies on), so a
    /// future adapter emitting `--wrapper agents/evil script.sh` as a single
    /// argv value would defeat this guard exactly as issue #59 set out to
    /// prevent -- it merely needs to omit the `./` prefix.
    ///
    /// This assertion is what issue #59's intent actually demands (refusal);
    /// it currently fails against `path_like_candidate`
    /// (`crates/warden/src/process.rs`), which returns `Some`/`None` on
    /// whitespace alone regardless of whether the value is a bare relative
    /// path shape. Left failing deliberately (code-standards.md: never
    /// weaken a test to make it pass) -- see the QA report for the fix this
    /// is the acceptance criterion for.
    #[test]
    fn a_relative_path_with_a_separator_and_whitespace_but_no_dot_prefix_is_refused() {
        let layout = WorktreeLayout::new();
        let reviewer_worktree = layout.role_worktree("reviewer");
        let coder_worktree = layout.role_worktree("coder");
        let repo = TempDir::new().unwrap();
        std::fs::create_dir_all(coder_worktree.join("agents")).unwrap();
        std::fs::write(
            coder_worktree.join("agents").join("evil script.sh"),
            "#!/bin/sh\n",
        )
        .unwrap();

        let error = validate_agent_program(
            "reviewer",
            false,
            "claude",
            &["--wrapper".to_string(), "agents/evil script.sh".to_string()],
            &reviewer_worktree,
            repo.path(),
            layout.run_worktrees_root.path(),
            &[],
        )
        .unwrap_err();
        assert!(matches!(error, ProcessError::UntrustedAgentArg { .. }));
    }

    // -----------------------------------------------------------------
    // `trusted_arg_values` escape hatch (issue #59 review, MEDIUM 4)
    // -----------------------------------------------------------------

    /// The concrete false positive the review demonstrated: a
    /// vendor-prefixed model id looks exactly like a relative path to the
    /// separator-based heuristic. Refused without the hatch, allowed once
    /// the caller vouches for that exact value.
    #[test]
    fn a_vendor_prefixed_model_value_is_refused_without_the_hatch_and_allowed_with_it() {
        let layout = WorktreeLayout::new();
        let worktree = layout.role_worktree("reviewer");
        let repo = TempDir::new().unwrap();
        let args = vec!["--model".to_string(), "anthropic/claude-3-opus".to_string()];

        let error = validate_agent_program(
            "reviewer",
            false,
            "claude",
            &args,
            &worktree,
            repo.path(),
            layout.run_worktrees_root.path(),
            &[],
        )
        .unwrap_err();
        assert!(matches!(error, ProcessError::UntrustedAgentArg { .. }));

        assert!(validate_agent_program(
            "reviewer",
            false,
            "claude",
            &args,
            &worktree,
            repo.path(),
            layout.run_worktrees_root.path(),
            &["anthropic/claude-3-opus".to_string()],
        )
        .is_ok());
    }

    /// End-to-end regression using the *real* `ClaudeAdapter::build_command`
    /// (issue #59 review, MEDIUM 4's own ask): `model:
    /// mistralai/mistral-large` in a reviewer's `AgentDefinition` must work
    /// once the caller vouches for it, exactly as it would coming from
    /// `orchestrator::mod::trusted_arg_values_for_step`.
    #[test]
    fn a_vendor_prefixed_model_from_a_real_adapter_command_works_with_the_hatch() {
        use crate::tool_adapter::{ClaudeAdapter, ToolAdapter};
        use warden_core::AgentDefinition;

        let layout = WorktreeLayout::new();
        let worktree = layout.role_worktree("reviewer");
        let repo = TempDir::new().unwrap();
        let definition = AgentDefinition::new(
            None,
            None,
            None,
            Some("mistralai/mistral-large".to_string()),
            "be a reviewer",
        )
        .unwrap();
        let command = ClaudeAdapter.build_command(&definition).unwrap();

        let error = validate_agent_program(
            "reviewer",
            false,
            &command.program,
            &command.args,
            &worktree,
            repo.path(),
            layout.run_worktrees_root.path(),
            &[],
        )
        .unwrap_err();
        assert!(matches!(error, ProcessError::UntrustedAgentArg { .. }));

        assert!(validate_agent_program(
            "reviewer",
            false,
            &command.program,
            &command.args,
            &worktree,
            repo.path(),
            layout.run_worktrees_root.path(),
            &["mistralai/mistral-large".to_string()],
        )
        .is_ok());
    }

    /// Issue #59 review, MEDIUM 4's own explicit ask: vouching for one
    /// literal value must never smuggle a *different*, genuinely
    /// coder-controlled path through. `trusted_arg_values` is compared by
    /// exact value equality only -- an unrelated trusted entry must have no
    /// effect on an actual containment violation.
    #[test]
    fn a_trusted_value_does_not_smuggle_an_unrelated_coder_controlled_path() {
        let layout = WorktreeLayout::new();
        let reviewer_worktree = layout.role_worktree("reviewer");
        let coder_worktree = layout.role_worktree("coder");
        let repo = TempDir::new().unwrap();
        let wrapper = coder_worktree.join("tool.sh");
        std::fs::write(&wrapper, "#!/bin/sh\n").unwrap();

        let error = validate_agent_program(
            "reviewer",
            false,
            "claude",
            &[
                "--wrapper".to_string(),
                wrapper.to_str().unwrap().to_string(),
            ],
            &reviewer_worktree,
            repo.path(),
            layout.run_worktrees_root.path(),
            // Vouches for an unrelated model string -- must not affect the
            // unlisted, actually-malicious `--wrapper` value above.
            &["anthropic/claude-3-opus".to_string()],
        )
        .unwrap_err();
        assert!(matches!(error, ProcessError::UntrustedAgentArg { .. }));
    }

    #[tokio::test]
    async fn captures_stdout_and_exit_code_of_a_successful_command() {
        let dir = TempDir::new().unwrap();
        let cmd = AgentCommand::new("sh", ["-c", "echo hello"]);
        let outcome = spawn_and_wait(&cmd, dir.path(), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout.trim(), "hello");
    }

    #[tokio::test]
    async fn reports_a_non_zero_exit_code_as_a_normal_outcome_not_an_error() {
        let dir = TempDir::new().unwrap();
        let cmd = AgentCommand::new("sh", ["-c", "exit 7"]);
        let outcome = spawn_and_wait(&cmd, dir.path(), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(outcome.exit_code, 7);
    }

    #[tokio::test]
    async fn spawn_exposes_the_pid_before_completion() {
        let dir = TempDir::new().unwrap();
        let cmd = AgentCommand::new("sh", ["-c", "sleep 0.2"]);
        let child = spawn(&cmd, dir.path()).unwrap();
        let pid = child
            .id()
            .expect("pid available for a freshly spawned child");
        let start_time = process_start_time(pid).expect("start time available for a live process");
        assert!(is_process_alive(pid, start_time));
        wait(child, "sh", None, CancellationToken::new())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn cancellation_kills_the_child_and_returns_cancelled_error() {
        let dir = TempDir::new().unwrap();
        let cmd = AgentCommand::new("sh", ["-c", "sleep 30"]);
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();

        let handle =
            tokio::spawn(async move { spawn_and_wait(&cmd, dir.path(), cancel_clone).await });
        cancel.cancel();

        let result = handle.await.unwrap();
        assert!(matches!(result, Err(ProcessError::Cancelled { .. })));
    }

    /// ADR-0012 (issue #20 Scope B): a payload written to stdin must reach
    /// the child, and the write half must be closed afterwards so a child
    /// that reads until EOF (`cat` with no arguments) actually sees one and
    /// exits, rather than hanging forever waiting for more input.
    #[tokio::test]
    async fn stdin_payload_is_written_and_closed_so_the_child_sees_it_and_exits() {
        let dir = TempDir::new().unwrap();
        let cmd = AgentCommand::new("cat", Vec::<String>::new());
        let child = spawn(&cmd, dir.path()).unwrap();
        let outcome = wait(
            child,
            "cat",
            Some("hello from warden".to_string()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout, "hello from warden");
    }

    /// ADR-0012 regression test: writing a large stdin payload while the
    /// child also produces enough stdout to fill an OS pipe buffer *before*
    /// it finishes reading stdin must not deadlock. Sequenced deliberately
    /// (write >64KiB of stdout first, only then drain stdin) so a naive
    /// "write the whole payload, then read stdout" implementation would
    /// hang: the child blocks on its own full stdout pipe (nobody's
    /// draining it yet) while we block on the child's full stdin pipe (it
    /// isn't reading yet either). Bounded by a timeout so a regression fails
    /// the test instead of hanging the suite.
    #[tokio::test]
    async fn writing_a_large_stdin_payload_does_not_deadlock_on_large_stdout() {
        let dir = TempDir::new().unwrap();
        // Emits 200_000 bytes of stdout first (well past a typical 64KiB
        // pipe buffer), then only afterwards drains stdin to completion.
        let cmd = AgentCommand::new(
            "sh",
            ["-c", "head -c 200000 /dev/zero; cat > /dev/null; exit 0"],
        );
        let child = spawn(&cmd, dir.path()).unwrap();
        let large_payload = "x".repeat(200_000);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            wait(child, "sh", Some(large_payload), CancellationToken::new()),
        )
        .await
        .expect("wait must not hang when both stdin and stdout exceed the pipe buffer size");

        assert_eq!(result.unwrap().exit_code, 0);
    }

    // Issue #50 review, MEDIUM 3: the `on_stdout_line` callback tests that
    // used to live here (`wait_with_progress_*`) moved to
    // `warden_sandbox::local`'s own test module -- that per-line callback is
    // now dead code on this side (every remaining `wait` caller passes no
    // callback at all; only `warden_sandbox::LocalSandbox::execute` still
    // offers one, to the sandbox seam's own caller). See
    // `warden_sandbox::local::tests::on_stdout_line_skips_blank_lines` and
    // its neighbours for that coverage, unchanged in substance.

    /// H1 (issue #20 review): an agent that exits immediately without ever
    /// reading stdin at all must not fail the invocation — a broken pipe is
    /// a legitimate outcome (logged, not silently swallowed), not a reason
    /// to fail the run. The payload is deliberately larger than a typical
    /// OS pipe buffer (64KiB) so the write is guaranteed to still be in
    /// progress when the child exits and closes its read end, forcing a
    /// genuine `ErrorKind::BrokenPipe` rather than racing a write that might
    /// complete before the child even schedules to exit.
    #[tokio::test]
    async fn an_agent_that_never_reads_stdin_and_exits_immediately_does_not_fail_the_invocation() {
        let dir = TempDir::new().unwrap();
        let cmd = AgentCommand::new("sh", ["-c", "exit 0"]);
        let child = spawn(&cmd, dir.path()).unwrap();
        let large_payload = "x".repeat(200_000);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            wait(child, "sh", Some(large_payload), CancellationToken::new()),
        )
        .await
        .expect("wait must not hang on a broken pipe");

        let outcome = result
            .expect("a broken pipe from an agent that ignores stdin must not fail the invocation");
        assert_eq!(outcome.exit_code, 0);
    }

    /// H1 unit coverage for [`classify_stdin_write_error`]'s two branches.
    /// The fatal (non-`BrokenPipe`) branch is exercised here rather than
    /// through a real subprocess: deterministically forcing a write error
    /// other than a broken pipe out of a genuine OS pipe isn't practical
    /// (`EPIPE` is by far the dominant real-world case, already covered
    /// end-to-end by `an_agent_that_never_reads_stdin_and_exits_immediately_does_not_fail_the_invocation`
    /// above), so this isolates the classification decision itself.
    #[test]
    fn classify_stdin_write_error_treats_broken_pipe_as_non_fatal_and_anything_else_as_fatal() {
        let broken_pipe = std::io::Error::from(std::io::ErrorKind::BrokenPipe);
        assert!(classify_stdin_write_error(broken_pipe, "agent").is_ok());

        let other = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let result = classify_stdin_write_error(other, "agent");
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }

    // -----------------------------------------------------------------
    // Re-test cycle (issue #20 review fix, fdcaa4e): adversarial stdin
    // write-failure angles beyond the coder's own "never reads at all"
    // case, derived from the task's intent independent of the coder's
    // tests above.
    // -----------------------------------------------------------------

    /// Adversarial angle: an agent that reads only *part* of a large
    /// payload before exiting (not "never reads at all") must still be a
    /// non-fatal, logged outcome -- the broken pipe fires once the agent's
    /// read end closes regardless of how much it already consumed.
    #[tokio::test]
    async fn an_agent_that_reads_only_part_of_the_payload_then_exits_does_not_fail_the_invocation()
    {
        let dir = TempDir::new().unwrap();
        let cmd = AgentCommand::new("sh", ["-c", "head -c 100 > /dev/null; exit 0"]);
        let child = spawn(&cmd, dir.path()).unwrap();
        let large_payload = "x".repeat(200_000);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            wait(child, "sh", Some(large_payload), CancellationToken::new()),
        )
        .await
        .expect("wait must not hang when the agent only partially reads stdin");

        let outcome = result.expect(
            "an agent reading only part of the payload before exiting must not fail the run",
        );
        assert_eq!(outcome.exit_code, 0);
    }

    /// Adversarial angle: an agent that explicitly closes its stdin file
    /// descriptor mid-run (rather than exiting outright) must still see the
    /// write fail as a non-fatal broken pipe -- and `wait` must not hang
    /// waiting for the write to somehow complete once the read side is
    /// gone, even though the process itself keeps running for a while
    /// afterwards.
    #[tokio::test]
    async fn an_agent_that_closes_stdin_mid_write_while_continuing_to_run_does_not_fail_the_invocation(
    ) {
        let dir = TempDir::new().unwrap();
        let cmd = AgentCommand::new("sh", ["-c", "exec 0<&-; sleep 0.3; exit 0"]);
        let child = spawn(&cmd, dir.path()).unwrap();
        let large_payload = "x".repeat(200_000);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            wait(child, "sh", Some(large_payload), CancellationToken::new()),
        )
        .await
        .expect("wait must not hang when the agent closes stdin mid-write and keeps running");

        let outcome = result.expect(
            "an agent that closes stdin mid-write but keeps running must not fail the invocation",
        );
        assert_eq!(outcome.exit_code, 0);
    }

    #[test]
    fn current_process_is_reported_alive() {
        let pid = std::process::id();
        let start_time =
            process_start_time(pid).expect("start time available for the current process");
        assert!(is_process_alive(pid, start_time));
    }

    #[test]
    fn a_pid_that_almost_certainly_does_not_exist_is_reported_not_alive() {
        // Real PIDs are far smaller than this on both Linux (< 2^22 by
        // default) and macOS (< 100_000); used purely as a deterministic
        // "not alive" fixture, well within the valid positive pid_t range.
        assert!(!is_process_alive(999_999_999, UNKNOWN_START_TIME));
    }

    #[test]
    fn a_wrong_start_time_is_reported_not_alive_even_though_the_pid_exists() {
        // The core PID-reuse defence (H1): a PID that genuinely exists
        // right now must still be reported not-alive if the start time we
        // recorded for it doesn't match the process currently holding that
        // PID — that mismatch is exactly what happens when the original
        // process died and the OS handed its PID to something else later.
        let pid = std::process::id();
        let real_start_time = process_start_time(pid).unwrap();
        let bogus_start_time = real_start_time + 1_000_000;
        assert!(!is_process_alive(pid, bogus_start_time));
    }

    #[test]
    fn no_recorded_start_time_falls_back_to_plain_existence_check() {
        // Historical/degraded case: UNKNOWN_START_TIME means we never
        // captured a fingerprint for this row, so we can't rule out PID
        // reuse — but we also shouldn't refuse to ever recover such rows,
        // so we fall back to "does a process with this PID exist at all".
        let pid = std::process::id();
        assert!(is_process_alive(pid, UNKNOWN_START_TIME));
    }

    /// Regression test for H1: POSIX `kill(pid=0, ...)` signals every
    /// process in the caller's own process group, so a naive liveness check
    /// against a pid-0 sentinel always misreported "alive" regardless of
    /// whether pid 0 referred to a real agent — silently defeating the
    /// crash-detection acceptance criterion in issue #1. `pid == 0` is now
    /// an explicit sentinel that is never alive, and
    /// `orchestrator::run_agent` no longer persists pid 0 at all (a missing
    /// `Child::id()` is a typed `ProcessError::MissingPid`, not a silent
    /// fallback to 0).
    #[test]
    fn pid_zero_is_never_reported_alive() {
        assert!(!is_process_alive(0, UNKNOWN_START_TIME));
        assert!(!is_process_alive(0, 12345));
    }

    #[tokio::test]
    async fn kill_pid_terminates_a_live_process_with_a_matching_fingerprint() {
        let dir = TempDir::new().unwrap();
        let cmd = AgentCommand::new("sh", ["-c", "sleep 30"]);
        let mut child = spawn(&cmd, dir.path()).unwrap();
        let pid = child.id().unwrap();
        let start_time = process_start_time(pid).unwrap();

        kill_pid(pid, start_time).unwrap();

        // `wait()` blocks until the OS has reaped it — proves the signal
        // actually landed, not just that `kill_pid` returned `Ok`.
        let status = child.wait().await.unwrap();
        assert!(!status.success());
        assert!(!is_process_alive(pid, start_time));
    }

    #[tokio::test]
    async fn kill_pid_is_a_noop_when_the_fingerprint_no_longer_matches() {
        // H1 regression: a live process that genuinely exists at `pid` must
        // never be signalled if its recorded start time doesn't match —
        // that mismatch is exactly the PID-reuse case this guards against.
        let dir = TempDir::new().unwrap();
        let cmd = AgentCommand::new("sh", ["-c", "sleep 30"]);
        let mut child = spawn(&cmd, dir.path()).unwrap();
        let pid = child.id().unwrap();
        let real_start_time = process_start_time(pid).unwrap();
        let bogus_start_time = real_start_time + 1_000_000;

        kill_pid(pid, bogus_start_time).unwrap();

        // Still alive: the mismatched fingerprint must have stopped
        // `kill_pid` from touching it.
        assert!(is_process_alive(pid, real_start_time));
        child.kill().await.unwrap();
    }

    #[test]
    fn kill_pid_on_pid_zero_is_a_noop_not_a_signal_to_the_process_group() {
        assert!(kill_pid(0, UNKNOWN_START_TIME).is_ok());
    }

    #[test]
    fn kill_pid_on_an_already_dead_pid_is_a_noop() {
        assert!(kill_pid(999_999_999, UNKNOWN_START_TIME).is_ok());
    }

    /// Issue #32: `spawn_tui_attach` must invoke `<binary> attach --run-id
    /// <id> --warden-home <path>` verbatim. Captures argv to a file instead
    /// of stdout, since [`spawn_tui_attach`]'s whole point is inheriting
    /// stdio (the real `warden-tui` must take over the launch terminal), not
    /// piping it for a test to capture.
    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_tui_attach_passes_the_expected_attach_subcommand_and_flags() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().unwrap();
        let out_file = dir.path().join("captured-args.txt");
        let script_path = dir.path().join("fake-warden-tui");
        std::fs::write(
            &script_path,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{}\"\n",
                out_file.display()
            ),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();

        let warden_home = dir.path().join("home");
        let mut child = spawn_tui_attach(&script_path, "run-123", &warden_home).unwrap();
        let status = child.wait().await.unwrap();
        assert!(status.success());

        let captured = std::fs::read_to_string(&out_file).unwrap();
        assert_eq!(
            captured.lines().collect::<Vec<_>>(),
            vec![
                "attach",
                "--run-id",
                "run-123",
                "--warden-home",
                warden_home.to_str().unwrap(),
            ]
        );
    }

    /// Unlike [`spawn`] (which `env_clear()`s for agent isolation),
    /// `spawn_tui_attach` must inherit the full parent environment --
    /// `warden-tui` is a trusted first-party binary, not an agent under the
    /// Agent Subprocess Protocol. Checked against `PATH`, whatever it
    /// already is in the test process, rather than mutating global process
    /// environment state (which `std::env::set_var` would, unsafely and with
    /// cross-test interference risk under a parallel test runner).
    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_tui_attach_inherits_the_full_parent_environment() {
        use std::os::unix::fs::PermissionsExt;

        let expected_path = std::env::var("PATH").expect("PATH is set in the test process");

        let dir = TempDir::new().unwrap();
        let out_file = dir.path().join("captured-env.txt");
        let script_path = dir.path().join("fake-warden-tui");
        std::fs::write(
            &script_path,
            format!(
                "#!/bin/sh\nprintf '%s' \"$PATH\" > \"{}\"\n",
                out_file.display()
            ),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();

        let mut child =
            spawn_tui_attach(&script_path, "run-123", &dir.path().join("home")).unwrap();
        let status = child.wait().await.unwrap();
        assert!(status.success());

        assert_eq!(std::fs::read_to_string(&out_file).unwrap(), expected_path);
    }

    #[tokio::test]
    async fn spawn_tui_attach_reports_a_typed_error_when_the_binary_does_not_exist() {
        let dir = TempDir::new().unwrap();
        let missing_binary = dir.path().join("does-not-exist");
        let result = spawn_tui_attach(&missing_binary, "run-123", &dir.path().join("home"));
        assert!(matches!(result, Err(ProcessError::Spawn { .. })));
    }
}
