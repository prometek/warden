//! [`DockerSandbox`]: containerised execution isolation (issue #49,
//! ADR-0015/ADR-0019) -- the second [`crate::Sandbox`] backend, alongside
//! [`crate::LocalSandbox`]'s host-process parity default. Where `LocalSandbox`
//! is "the same isolation warden always applied, given a name", `DockerSandbox`
//! actually changes what an agent's own process can reach: it runs inside a
//! container, with nothing of the host mounted in except what this module
//! documents below.
//!
//! # What is mounted, and why (locked decisions, not open questions)
//!
//! - **The role's own worktree** (`SandboxSpec::cwd`), host-absolute path,
//!   bind-mounted **at the same absolute path** inside the container, `-w`
//!   pointed at it. Read-write: it is the one place this invocation is
//!   meant to write.
//! - **The base repo's `.git` directory** (`DockerConfig::repo_path` joined
//!   with `.git`), also bind-mounted at its own host-absolute path,
//!   **read-write**. A git *worktree*'s own `.git` is a one-line file, not a
//!   directory -- `gitdir: <absolute path into the parent repo's
//!   .git/worktrees/<id>>` -- so every git operation inside the worktree
//!   resolves that absolute pointer first. Mounting only the worktree and
//!   leaving that pointer dangling would make `git status`/`git diff`/`git
//!   commit` fail the moment they touch anything beyond the worktree's own
//!   checked-out files. Mounting the parent `.git` at the *identical* host
//!   path makes the pointer resolve inside the container exactly as it does
//!   on the host, with no path rewriting anywhere.
//! - **The host's `~/.claude` directory, read-only**, at
//!   `<container_home>/.claude` -- the *only* host path mounted for
//!   authentication. This is a deliberate narrowing of issue #49's literal
//!   "no HOME mount at all": `claude` needs to find its own login state
//!   somewhere, and the alternative (no auth at all, `docker run` failing
//!   every invocation) is a worse outcome than mounting the one directory
//!   that carries it, read-only. `~/.ssh`, `~/.aws`, `~/.config/gh`, any
//!   `.env`, or the rest of the host's real `$HOME` are **never** mounted --
//!   issue #25's guarantee (no ambient git/cloud credentials reachable by an
//!   agent) holds for anything reachable *from inside the container*, since
//!   none of those paths exist there by any name.
//!
//! # The rw `.git` mount and host-side git hooks (issue #49 review, HIGH)
//!
//! Hooks live in the *common* git dir, shared by the main repo and every one
//! of its worktrees. Because the base repo's `.git` above is mounted
//! **read-write**, a contained agent could write `.git/hooks/pre-push`,
//! `post-checkout`, `reference-transaction`, etc. and have it run **on the
//! host** -- as the host user, with real credentials -- the next time
//! `warden` itself (never the agent) runs a git command against that same
//! repo (a converged run's `git push` to the local gate repo, a
//! `git worktree add` for the next role, an evidence commit). That would
//! defeat issue #25's guarantee entirely on the host side, no matter how
//! tight this module's own container-side mounts are. Every such host-side
//! invocation passes `warden::git_util::NO_HOST_HOOKS` -- see that module's
//! own docs (crate `warden`, not this one: the host-side git invocations
//! this vector is about are `warden`'s, not `warden-sandbox`'s) for the full
//! reasoning and the exact call sites.
//!
//! # Network: default bridge, no egress filtering yet (accepted v1 limit)
//!
//! No `--network` flag is passed -- the container gets Docker's normal
//! default bridge network, so the Anthropic API `claude` itself calls stays
//! reachable. `git push origin` still fails by construction (issue #28's
//! guarantee): no ssh key, no `~/.config/gh`, no git credential helper is
//! ever mounted or configured, so there is simply nothing for `git` to
//! authenticate a push with, regardless of network reachability. Filtering
//! *egress* itself (so a compromised/malicious agent process couldn't reach
//! arbitrary hosts even if it somehow obtained credentials some other way)
//! is explicitly deferred past v1 -- see ADR-0019.
//!
//! # Container home: fixed at `/root`
//!
//! [`CONTAINER_HOME`] is `/root` (the default `docker run` user for an
//! unmodified base image) -- used consistently for `-e HOME=`, the `.claude`
//! mount target, and (indirectly) for whichever allowlisted `USER` value is
//! forwarded. `HOME` is **always** set to this, overriding whatever the
//! adapter's `env_allowlist` would otherwise forward for it (a host `HOME`
//! value would point nowhere useful inside the container, and would not be
//! where `.claude` is mounted) -- see [`resolve_forwarded_env`].
//!
//! # Crash recovery still pid-based (known, pre-existing limit)
//!
//! [`Execution::pid`] is the *docker client's* own host pid, not the
//! container's -- `ADR-0015` already flagged this as the one seam #49 would
//! need to reopen properly: recovering a crashed run currently kills a host
//! pid, which cannot reach a container. This issue does not fix that; it
//! only makes sure [`Sandbox::destroy`] (the mechanism a real fix would sit
//! on top of) reliably removes the container by name, on every teardown
//! path this crate controls.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;

use async_trait::async_trait;
use tokio::process::Child;
use tokio_util::sync::CancellationToken;

use crate::drain::drain_and_wait;
use crate::error::{Result, SandboxError};
use crate::{Command, ExecuteOptions, Execution, ExecutionResult, Sandbox, SandboxId, SandboxSpec};

/// The `docker` CLI binary, resolved via `PATH` -- same convention
/// [`crate::LocalSandbox`] uses for the agent's own `command.program`.
const DOCKER_BIN: &str = "docker";

/// Fixed container home -- see this module's own docs for why this is a
/// single constant rather than something derived per-invocation.
const CONTAINER_HOME: &str = "/root";

/// What every [`DockerSandbox::execute`] call needs beyond the per-command
/// [`Command`] itself: the image to run, and the two host paths this
/// backend's mounts are built from. Constructed once per `warden run`
/// invocation (issue #49's `--isolation docker`) -- `repo_path` is the run's
/// *base* repository (never a role's own worktree, which arrives per-call
/// via [`SandboxSpec::cwd`]), so its `.git` directory can be resolved once
/// here rather than threaded through every [`Sandbox::execute`] call.
pub struct DockerConfig {
    /// The image every container runs -- must already contain `git` and
    /// whatever CLI the run's `--tool` adapter execs (e.g. `claude`); see
    /// `crates/warden-sandbox/docker/Dockerfile` for the reference image
    /// this crate is validated against.
    pub image: String,
    /// The run's base repository (`RunConfig::repo_path`) -- its `.git`
    /// directory is what gets bind-mounted alongside each role's own
    /// worktree, so worktree-relative git pointers resolve inside the
    /// container (see this module's own docs).
    pub repo_path: PathBuf,
    /// Host path to the Claude Code login/config directory (normally
    /// `~/.claude`) -- bind-mounted read-only into every container as the
    /// sole source of authentication (see this module's own docs). Resolved
    /// by the caller (`warden`'s CLI), not by this crate, so a missing
    /// `HOME` is the caller's own `--warden-home`-style error, not a
    /// sandbox-layer one.
    pub claude_config_dir: PathBuf,
}

/// [`Sandbox`] backed by `docker run --rm` -- see this module's own docs for
/// the exact mount/network/auth shape and the limits accepted for v1.
/// Bookkeeping-only `create`/`destroy` bind an id to a `cwd` exactly like
/// [`crate::LocalSandbox`]; unlike Local, the id doubles as the actual
/// `docker run --name` this backend passes, since there is no separate
/// `docker create` step to mint a daemon-assigned container id from (each
/// [`DockerSandbox::execute`] is a single, self-contained `docker run --rm`).
pub struct DockerSandbox {
    config: DockerConfig,
    sandboxes: Mutex<HashMap<SandboxId, PathBuf>>,
}

impl DockerSandbox {
    pub fn new(config: DockerConfig) -> Self {
        Self {
            config,
            sandboxes: Mutex::new(HashMap::new()),
        }
    }

    fn cwd_for(&self, id: &SandboxId) -> Result<PathBuf> {
        self.sandboxes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned()
            .ok_or_else(|| SandboxError::UnknownSandbox { id: id.clone() })
    }
}

#[async_trait]
impl Sandbox for DockerSandbox {
    async fn create(&self, spec: SandboxSpec) -> Result<SandboxId> {
        // Issue #49: the id *is* the `docker run --name` this backend uses
        // (see this type's own docs) -- prefixed so `docker ps -a`/`docker
        // rm -f` output is identifiable as warden's own, the same
        // expectation the issue's own acceptance criteria (no leftover
        // `warden-*` containers) are phrased against.
        let id = SandboxId::new(format!("warden-{}", uuid::Uuid::new_v4()));
        self.sandboxes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.clone(), spec.cwd);
        Ok(id)
    }

    async fn execute<'a>(
        &'a self,
        id: &'a SandboxId,
        command: Command,
        options: ExecuteOptions<'a>,
    ) -> Result<Execution<'a>> {
        let cwd = self.cwd_for(id)?;
        let container_name = id.to_string();

        let host_worktree = canonicalize_host_path(&cwd)?;
        let host_repo_git = canonicalize_host_path(&self.config.repo_path.join(".git"))?;
        let host_claude_dir = self.config.claude_config_dir.canonicalize().map_err(|_| {
            SandboxError::DockerUnavailable {
                reason: format!(
                    "host Claude config directory {} does not exist; `--isolation docker` \
                     requires the host to already be logged into `claude` (run `claude` at \
                     least once outside docker first)",
                    self.config.claude_config_dir.display()
                ),
            }
        })?;

        let forwarded_env = resolve_forwarded_env(&command.env_allowlist, &command.program);
        let argv = build_docker_run_argv(
            &container_name,
            &self.config.image,
            &host_worktree,
            &host_repo_git,
            &host_claude_dir,
            CONTAINER_HOME,
            &forwarded_env,
            &command.program,
            &command.args,
        );

        let mut cmd = tokio::process::Command::new(DOCKER_BIN);
        cmd.args(&argv)
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let child = cmd.spawn().map_err(|source| SandboxError::Spawn {
            program: DOCKER_BIN.to_string(),
            source,
        })?;
        let pid = child.id();

        // Issue #49: `pid` is `docker run`'s own client process, the host
        // pid `Execution::pid` has always meant for `LocalSandbox` too --
        // see this module's own docs on why crash recovery against it
        // cannot reach the container this client is proxying, and why that
        // is an accepted, pre-existing limit rather than something this
        // issue fixes.
        let program_label = format!("docker (container {container_name})");
        let stdin_payload = command.stdin;
        let cancel = options.cancel;
        let on_stdout_line = options.on_stdout_line;

        Ok(Execution::new(
            pid,
            drain_and_wait_with_container_cleanup(
                child,
                program_label,
                stdin_payload,
                cancel,
                on_stdout_line,
                container_name,
            ),
        ))
    }

    async fn destroy(&self, id: SandboxId) -> Result<()> {
        let removed = self
            .sandboxes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id)
            .is_some();
        if !removed {
            // Same "already gone is not an error" convention
            // `warden::process::kill_pid`/`LocalSandbox::destroy` use --
            // nothing left to tear down.
            return Ok(());
        }
        remove_container(&id.to_string()).await
    }
}

/// Wraps the shared [`drain_and_wait`] with the one piece of cleanup only a
/// container backend needs: killing the local `docker run` client
/// (`drain_and_wait`'s own cancel branch, via `child.kill()`) does **not**
/// stop the daemon-side container -- `--rm` only removes a container once it
/// exits on its own, and a killed client leaves it running, detached, on the
/// daemon. On the cancelled path (and only that path -- a normal exit is
/// already reaped by `--rm`), this best-effort force-removes the container
/// by name right away, rather than relying solely on
/// `warden::orchestrator::SandboxGuard`'s own, always-run `Sandbox::destroy`
/// call to catch it later (issue #49: a cancelled execution must never leave
/// a container running even for the window before that guard's `destroy`
/// resolves).
async fn drain_and_wait_with_container_cleanup(
    child: Child,
    program: String,
    stdin_payload: Option<String>,
    cancel: CancellationToken,
    on_stdout_line: Option<&(dyn Fn(&str) + Send + Sync)>,
    container_name: String,
) -> Result<ExecutionResult> {
    let result = drain_and_wait(child, program, stdin_payload, cancel, on_stdout_line).await;
    if matches!(result, Err(SandboxError::Cancelled { .. })) {
        force_remove_container(&container_name).await;
    }
    let result = result?;

    // Issue #49 review, MEDIUM: `docker run` reserves exit code 125 for a
    // failure of the `docker` client/daemon itself (daemon unreachable,
    // image missing, invalid flags) -- never for whatever ran *inside* the
    // container, which exits through this same path with its own code. Left
    // unclassified, this surfaced downstream as an ordinary (if puzzling)
    // agent failure -- "produced no parseable output", with the real reason
    // buried in a stderr nobody inspected -- instead of the actionable,
    // typed error a daemon-down/image-missing case deserves (image-missing
    // in particular is the most likely first-run error, since the image is
    // hand-built, not published anywhere). See
    // [`classify_docker_startup_failure`]'s own docs for why exit code
    // alone is not enough to make this call.
    if let Some(reason) = classify_docker_startup_failure(result.exit_code, &result.stderr) {
        return Err(SandboxError::DockerUnavailable { reason });
    }

    Ok(result)
}

/// Distinguishes a `docker run` startup failure (daemon down, image
/// missing) from a normal -- if non-zero -- exit of whatever ran *inside*
/// the container. Exit code 125 alone is not a reliable enough signal on
/// its own: nothing stops a contained process from legitimately choosing to
/// exit with 125 for its own reasons, and that outcome must still reach the
/// caller as a normal [`ExecutionResult`], not a fabricated
/// [`SandboxError::DockerUnavailable`]. Corroborating against `docker`'s own
/// stderr markers (verified directly against real `docker run`, issue #49
/// review) keeps this specific to an actual client/daemon-level failure.
fn classify_docker_startup_failure(exit_code: i32, stderr: &str) -> Option<String> {
    if exit_code != 125 {
        return None;
    }
    if stderr.contains("Cannot connect to the Docker daemon") {
        return Some(format!(
            "the docker daemon is not reachable -- start Docker and retry. docker's own stderr: {}",
            stderr.trim()
        ));
    }
    if stderr.contains("Unable to find image") || stderr.contains("No such image") {
        return Some(format!(
            "the docker image was not found -- build it via \
             crates/warden-sandbox/docker/Dockerfile (see \
             crates/warden-sandbox/docker/README.md) and retry. docker's own stderr: {}",
            stderr.trim()
        ));
    }
    None
}

/// Best-effort `docker rm -f` for the cancellation cleanup path above --
/// deliberately never fails the execution it's called from (the caller is
/// already returning `SandboxError::Cancelled`; a cleanup failure on top of
/// that is logged, not compounded into a second error).
async fn force_remove_container(container_name: &str) {
    let output = tokio::process::Command::new(DOCKER_BIN)
        .args(["rm", "-f", container_name])
        .stdin(Stdio::null())
        .output()
        .await;
    match output {
        Ok(output) if output.status.success() => {}
        // Same benign race as `remove_container` -- see
        // `is_benign_removal_race`'s own docs. Not worth even a `debug!`
        // here: this is the expected shape on a clean cancellation.
        Ok(output) if is_benign_removal_race(&String::from_utf8_lossy(&output.stderr)) => {}
        Ok(output) => {
            tracing::warn!(
                container_name,
                status = ?output.status,
                stderr = %String::from_utf8_lossy(&output.stderr),
                "docker rm -f exited non-zero during cancellation cleanup"
            );
        }
        Err(error) => {
            tracing::warn!(container_name, %error, "failed to run docker rm -f during cancellation cleanup");
        }
    }
}

/// `docker rm -f <name>`, idempotent: "No such container" (the container
/// already exited and `--rm` already reaped it, or was already destroyed) is
/// not an error -- the same "already gone is not an error" convention
/// [`crate::error::SandboxError::UnknownSandbox`]'s own docs and
/// `warden::process::kill_pid` both use. Any other failure (daemon
/// unreachable, permission denied) is a real, typed
/// [`SandboxError::DockerUnavailable`] -- surfaced to
/// `warden::orchestrator::SandboxGuard::destroy`'s own caller, which already
/// logs (rather than fails the run on) a teardown failure.
async fn remove_container(container_name: &str) -> Result<()> {
    let output = tokio::process::Command::new(DOCKER_BIN)
        .args(["rm", "-f", container_name])
        .output()
        .await
        .map_err(|source| SandboxError::Spawn {
            program: DOCKER_BIN.to_string(),
            source,
        })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if is_benign_removal_race(&stderr) {
        return Ok(());
    }
    Err(SandboxError::DockerUnavailable {
        reason: format!("`docker rm -f {container_name}` failed: {}", stderr.trim()),
    })
}

/// Whether `docker rm -f`'s stderr indicates there is nothing left to do,
/// rather than a real failure -- the same "already gone is not an error"
/// convention [`crate::error::SandboxError::UnknownSandbox`]'s own docs and
/// `warden::process::kill_pid` both use. Two distinct benign cases (issue
/// #49 review, LOW): "No such container" (already fully gone -- `--rm`
/// already reaped it, or a prior `destroy` already ran), and "already in
/// progress"/"is being removed" (`--rm`'s own async removal is racing this
/// exact call on the happy path -- `docker rm -f` reliably reports this
/// verbatim wording when it loses that race, verified directly against real
/// `docker`, issue #49 review). Both mean the container is going away (or
/// gone) regardless of what this call does.
fn is_benign_removal_race(stderr: &str) -> bool {
    stderr.contains("No such container")
        || stderr.contains("already in progress")
        || stderr.contains("is being removed")
}

/// Resolves `env_allowlist` against this process's own environment,
/// mirroring `LocalSandbox::execute`'s identical resolution loop one-for-one
/// (a missing variable is `tracing::warn!`ed, not fatal -- the tool's own
/// error downstream is more actionable than a fabricated one here). `HOME`
/// is always skipped: [`CONTAINER_HOME`] is what `-e HOME=` is unconditionally
/// set to (see this module's own docs), regardless of whether `HOME` is even
/// in `env_allowlist` or what this process's own `HOME` resolves to.
fn resolve_forwarded_env(env_allowlist: &[String], program: &str) -> Vec<(String, String)> {
    env_allowlist
        .iter()
        .filter(|name| name.as_str() != "HOME")
        .filter_map(|name| match std::env::var(name) {
            Ok(value) => Some((name.clone(), value)),
            Err(_) => {
                tracing::warn!(
                    var = name,
                    program,
                    "adapter-requested environment variable is not set in warden's own \
                     process environment; the container will run without it"
                );
                None
            }
        })
        .collect()
}

/// Resolves a host path this backend needs to bind-mount to its canonical,
/// absolute form -- a relative or symlinked path would otherwise produce a
/// `--mount` whose host side does not match what git/the worktree actually
/// resolve to. Any failure here (path does not exist, permission denied) is
/// a configuration problem, not a docker daemon one -- typed as
/// [`SandboxError::DockerUnavailable`] rather than `Spawn`/`Wait`, neither of
/// which fits a failure that happens before anything is even spawned.
fn canonicalize_host_path(path: &Path) -> Result<PathBuf> {
    path.canonicalize()
        .map_err(|source| SandboxError::DockerUnavailable {
            reason: format!(
                "cannot resolve host path {} for a docker bind mount: {source}",
                path.display()
            ),
        })
}

/// Pure `docker run` argv construction, split out of
/// [`DockerSandbox::execute`] so the exact flag/mount shape is unit-testable
/// without a daemon (this crate's own testing convention -- see
/// [`crate::LocalSandbox`]'s tests, which need no daemon either).
/// `forwarded_env` is already resolved (host env lookups are
/// [`resolve_forwarded_env`]'s job) -- this function only ever formats
/// strings, no I/O of its own.
#[allow(clippy::too_many_arguments)]
fn build_docker_run_argv(
    container_name: &str,
    image: &str,
    host_worktree: &Path,
    host_repo_git: &Path,
    host_claude_dir: &Path,
    container_home: &str,
    forwarded_env: &[(String, String)],
    program: &str,
    args: &[String],
) -> Vec<String> {
    let worktree = host_worktree.display().to_string();
    let repo_git = host_repo_git.display().to_string();
    let claude_dir = host_claude_dir.display().to_string();

    let mut argv = vec![
        "run".to_string(),
        "--rm".to_string(),
        "--name".to_string(),
        container_name.to_string(),
        "-i".to_string(),
        "--mount".to_string(),
        format!("type=bind,source={worktree},target={worktree}"),
        "--mount".to_string(),
        format!("type=bind,source={repo_git},target={repo_git}"),
        "--mount".to_string(),
        format!("type=bind,source={claude_dir},target={container_home}/.claude,readonly"),
        "-e".to_string(),
        format!("HOME={container_home}"),
    ];
    for (name, value) in forwarded_env {
        argv.push("-e".to_string());
        argv.push(format!("{name}={value}"));
    }
    argv.push("-w".to_string());
    argv.push(worktree);
    argv.push(image.to_string());
    argv.push(program.to_string());
    argv.extend(args.iter().cloned());
    argv
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Plain `alpine`, deliberately *not* `alpine/git` -- that image sets
    /// `ENTRYPOINT ["git"]`, which would silently prepend `git` in front of
    /// whatever `program`/`args` this backend passes as `CMD` (this crate's
    /// contract is a plain `<image> <program> <args...>` invocation, with no
    /// entrypoint override -- see [`build_docker_run_argv`]'s own docs),
    /// breaking every behavioural test below in a way that has nothing to do
    /// with what they're actually asserting. `git` itself is installed
    /// inline (`apk add`) only in the one test that needs it.
    const TEST_IMAGE: &str = "alpine:latest";

    // -----------------------------------------------------------------
    // Pure argv construction -- no daemon required.
    // -----------------------------------------------------------------

    #[test]
    fn argv_contains_the_worktree_and_repo_git_mounts_at_identical_host_paths() {
        let argv = build_docker_run_argv(
            "warden-test",
            "warden-agent:latest",
            Path::new("/host/worktrees/coder"),
            Path::new("/host/repo/.git"),
            Path::new("/host/home/.claude"),
            "/root",
            &[],
            "claude",
            &[],
        );

        assert!(argv.contains(
            &"type=bind,source=/host/worktrees/coder,target=/host/worktrees/coder".to_string()
        ));
        assert!(
            argv.contains(&"type=bind,source=/host/repo/.git,target=/host/repo/.git".to_string())
        );
    }

    /// Issue #49 review, LOW: the legacy `-v {p}:{p}` field-separator syntax
    /// is ambiguous the moment a host path contains a colon (legal on Linux,
    /// and `--repo`/a role's own worktree path are external input) --
    /// `--mount` has no such ambiguity (`source=`/`target=` are exact
    /// strings, not colon-split fields).
    #[test]
    fn argv_uses_mount_not_the_colon_ambiguous_v_flag() {
        let argv = build_docker_run_argv(
            "warden-test",
            "warden-agent:latest",
            Path::new("/host/worktrees/coder"),
            Path::new("/host/repo/.git"),
            Path::new("/host/home/.claude"),
            "/root",
            &[],
            "claude",
            &[],
        );

        assert!(!argv.contains(&"-v".to_string()));
        assert_eq!(argv.iter().filter(|arg| *arg == "--mount").count(), 3);
    }

    #[test]
    fn argv_mounts_claude_config_read_only_under_the_container_home() {
        let argv = build_docker_run_argv(
            "warden-test",
            "warden-agent:latest",
            Path::new("/host/worktrees/coder"),
            Path::new("/host/repo/.git"),
            Path::new("/host/home/.claude"),
            "/root",
            &[],
            "claude",
            &[],
        );

        assert!(argv.contains(
            &"type=bind,source=/host/home/.claude,target=/root/.claude,readonly".to_string()
        ));
    }

    #[test]
    fn argv_always_sets_home_to_the_container_home() {
        let argv = build_docker_run_argv(
            "warden-test",
            "warden-agent:latest",
            Path::new("/host/worktrees/coder"),
            Path::new("/host/repo/.git"),
            Path::new("/host/home/.claude"),
            "/root",
            // Real callers never pass a `HOME` entry here at all --
            // `resolve_forwarded_env` strips it before `forwarded_env`
            // reaches this function (see that function's own test coverage)
            // -- so this pins down the one entry a real invocation actually
            // produces: exactly the container's own baseline `HOME=`.
            &[],
            "claude",
            &[],
        );

        let home_entries: Vec<&String> = argv
            .iter()
            .zip(argv.iter().skip(1))
            .filter(|(flag, _)| flag.as_str() == "-e")
            .map(|(_, value)| value)
            .filter(|value| value.starts_with("HOME="))
            .collect();
        assert_eq!(home_entries, vec!["HOME=/root"]);
    }

    #[test]
    fn argv_never_mounts_ssh_aws_or_gh_config() {
        let argv = build_docker_run_argv(
            "warden-test",
            "warden-agent:latest",
            Path::new("/host/worktrees/coder"),
            Path::new("/host/repo/.git"),
            Path::new("/host/home/.claude"),
            "/root",
            &[],
            "claude",
            &[],
        );
        let joined = argv.join(" ");

        assert!(!joined.contains(".ssh"));
        assert!(!joined.contains(".aws"));
        assert!(!joined.contains(".config/gh"));
        assert!(!joined.contains(".env"));
    }

    #[test]
    fn argv_sets_no_network_flag_default_bridge() {
        let argv = build_docker_run_argv(
            "warden-test",
            "warden-agent:latest",
            Path::new("/host/worktrees/coder"),
            Path::new("/host/repo/.git"),
            Path::new("/host/home/.claude"),
            "/root",
            &[],
            "claude",
            &[],
        );

        assert!(!argv.contains(&"--network".to_string()));
    }

    #[test]
    fn argv_sets_working_directory_and_program_args_after_the_image() {
        let argv = build_docker_run_argv(
            "warden-test",
            "warden-agent:latest",
            Path::new("/host/worktrees/coder"),
            Path::new("/host/repo/.git"),
            Path::new("/host/home/.claude"),
            "/root",
            &[],
            "claude",
            &["--output-format".to_string(), "json".to_string()],
        );

        let w_index = argv.iter().position(|arg| arg == "-w").unwrap();
        assert_eq!(argv[w_index + 1], "/host/worktrees/coder");

        let image_index = argv
            .iter()
            .position(|arg| arg == "warden-agent:latest")
            .unwrap();
        assert_eq!(argv[image_index + 1], "claude");
        assert_eq!(argv[image_index + 2], "--output-format");
        assert_eq!(argv[image_index + 3], "json");
    }

    #[test]
    fn argv_forwards_only_the_resolved_env_pairs_given() {
        let argv = build_docker_run_argv(
            "warden-test",
            "warden-agent:latest",
            Path::new("/host/worktrees/coder"),
            Path::new("/host/repo/.git"),
            Path::new("/host/home/.claude"),
            "/root",
            &[("USER".to_string(), "alice".to_string())],
            "claude",
            &[],
        );

        assert!(argv.contains(&"USER=alice".to_string()));
    }

    // -----------------------------------------------------------------
    // `resolve_forwarded_env` -- no daemon required.
    // -----------------------------------------------------------------

    /// Same `CARGO_MANIFEST_DIR` technique `LocalSandbox`'s own
    /// `env_allowlist_forwards_only_the_named_variables` test uses --
    /// reliably set by `cargo test`, read-only here, avoiding the
    /// cross-test-interference hazard of mutating global process env state.
    #[test]
    fn resolve_forwarded_env_forwards_a_set_allowlisted_variable() {
        let expected = std::env::var("CARGO_MANIFEST_DIR")
            .expect("precondition: cargo test sets CARGO_MANIFEST_DIR");
        let forwarded = resolve_forwarded_env(&["CARGO_MANIFEST_DIR".to_string()], "claude");
        assert_eq!(
            forwarded,
            vec![("CARGO_MANIFEST_DIR".to_string(), expected)]
        );
    }

    #[test]
    fn resolve_forwarded_env_always_strips_home() {
        let forwarded = resolve_forwarded_env(&["HOME".to_string(), "USER".to_string()], "claude");
        assert!(forwarded.iter().all(|(name, _)| name != "HOME"));
    }

    #[test]
    fn resolve_forwarded_env_skips_a_variable_missing_from_this_process_own_environment() {
        let forwarded =
            resolve_forwarded_env(&["THIS_VAR_DOES_NOT_EXIST_ANYWHERE".to_string()], "claude");
        assert!(forwarded.is_empty());
    }

    // -----------------------------------------------------------------
    // `classify_docker_startup_failure` (issue #49 review, MEDIUM) -- no
    // daemon required, pure function.
    // -----------------------------------------------------------------

    #[test]
    fn classifies_exit_125_with_daemon_unreachable_stderr_as_docker_unavailable() {
        let reason = classify_docker_startup_failure(
            125,
            "Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker \
             daemon running?",
        );
        assert!(reason.is_some());
        assert!(reason.unwrap().contains("start Docker"));
    }

    #[test]
    fn classifies_exit_125_with_missing_image_stderr_as_docker_unavailable() {
        let reason = classify_docker_startup_failure(
            125,
            "Unable to find image 'warden-agent:latest' locally\ndocker: Error response from \
             daemon: pull access denied for warden-agent, repository does not exist or may \
             require 'docker login'",
        );
        assert!(reason.is_some());
        assert!(reason.unwrap().contains("Dockerfile"));
    }

    /// A contained process is free to legitimately exit 125 for its own
    /// reasons -- without a corroborating docker-level stderr marker, this
    /// must stay a normal (if puzzling) exit code, not a fabricated
    /// `DockerUnavailable`.
    #[test]
    fn does_not_classify_exit_125_without_a_docker_level_stderr_marker() {
        assert!(classify_docker_startup_failure(125, "some unrelated agent error").is_none());
    }

    #[test]
    fn does_not_classify_a_non_125_exit_code_even_with_a_docker_level_looking_stderr() {
        assert!(classify_docker_startup_failure(1, "Unable to find image 'x' locally").is_none());
    }

    // -----------------------------------------------------------------
    // `is_benign_removal_race` (issue #49 review, LOW) -- no daemon
    // required, pure function.
    // -----------------------------------------------------------------

    #[test]
    fn treats_already_gone_and_removal_races_as_benign() {
        assert!(is_benign_removal_race(
            "Error: No such container: warden-abc123"
        ));
        // Exact wording verified against real docker (issue #49 review): two
        // concurrent `docker rm -f` calls on the same container.
        assert!(is_benign_removal_race(
            "Error response from daemon: removal of container warden-abc123 is already in \
             progress"
        ));
        assert!(is_benign_removal_race(
            "Error response from daemon: container warden-abc123 is being removed"
        ));
    }

    #[test]
    fn does_not_treat_an_unrelated_failure_as_benign() {
        assert!(!is_benign_removal_race(
            "Error response from daemon: permission denied"
        ));
    }

    // -----------------------------------------------------------------
    // `create`/`destroy` bookkeeping -- no daemon required (these never
    // actually invoke `docker`; `destroy` on an id that was never `create`d
    // returns early).
    // -----------------------------------------------------------------

    fn config(dir: &TempDir) -> DockerConfig {
        DockerConfig {
            image: "warden-agent:latest".to_string(),
            repo_path: dir.path().to_path_buf(),
            claude_config_dir: dir.path().to_path_buf(),
        }
    }

    #[tokio::test]
    async fn create_mints_a_warden_prefixed_id() {
        let dir = TempDir::new().unwrap();
        let sandbox = DockerSandbox::new(config(&dir));
        let id = sandbox
            .create(SandboxSpec {
                cwd: dir.path().to_path_buf(),
            })
            .await
            .unwrap();
        assert!(id.to_string().starts_with("warden-"));
    }

    #[tokio::test]
    async fn destroy_is_idempotent_for_an_id_that_was_never_created() {
        let dir = TempDir::new().unwrap();
        let sandbox = DockerSandbox::new(config(&dir));
        assert!(sandbox
            .destroy(SandboxId::new("warden-never-created"))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn execute_with_an_unknown_sandbox_id_reports_a_typed_error() {
        let dir = TempDir::new().unwrap();
        let sandbox = DockerSandbox::new(config(&dir));
        let bogus_id = SandboxId::new("warden-bogus");

        let result = sandbox
            .execute(
                &bogus_id,
                Command {
                    program: "true".to_string(),
                    args: Vec::new(),
                    env_allowlist: Vec::new(),
                    stdin: None,
                },
                ExecuteOptions::default(),
            )
            .await;
        assert!(matches!(result, Err(SandboxError::UnknownSandbox { .. })));
    }

    #[tokio::test]
    async fn execute_reports_a_typed_error_when_the_claude_config_dir_is_missing() {
        let dir = TempDir::new().unwrap();
        let mut cfg = config(&dir);
        cfg.claude_config_dir = dir.path().join("does-not-exist");
        let sandbox = DockerSandbox::new(cfg);
        let id = sandbox
            .create(SandboxSpec {
                cwd: dir.path().to_path_buf(),
            })
            .await
            .unwrap();

        let result = sandbox
            .execute(
                &id,
                Command {
                    program: "true".to_string(),
                    args: Vec::new(),
                    env_allowlist: Vec::new(),
                    stdin: None,
                },
                ExecuteOptions::default(),
            )
            .await;
        assert!(matches!(
            result,
            Err(SandboxError::DockerUnavailable { .. })
        ));
    }

    // -----------------------------------------------------------------
    // Behavioural tests against a real daemon -- gated (issue #49 spec):
    // auto-skip with an eprintln when no daemon is reachable, rather than
    // failing the whole suite on a machine without Docker installed/running.
    // -----------------------------------------------------------------

    /// Probes `docker info` once -- the cheapest daemon-reachability check
    /// available, mirroring how a human would sanity-check the same thing.
    async fn docker_daemon_available() -> bool {
        tokio::process::Command::new(DOCKER_BIN)
            .arg("info")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map(|status| status.success())
            .unwrap_or(false)
    }

    /// Builds a throwaway repo (with a worktree, since that's what
    /// `DockerSandbox` actually mounts) plus a fake `~/.claude` dir --
    /// returns `(repo_dir, worktree_dir, claude_dir)`, all `TempDir`s the
    /// caller must keep alive for the test's duration.
    fn init_repo_with_worktree_and_claude_dir() -> (TempDir, PathBuf, TempDir) {
        let repo = TempDir::new().unwrap();
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .current_dir(repo.path())
                .args(args)
                .status()
                .expect("spawn git");
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["init", "--quiet"]);
        run(&["config", "user.email", "test@warden.local"]);
        run(&["config", "user.name", "warden-test"]);
        std::fs::write(
            repo.path().join("README.md"),
            "warden docker sandbox test\n",
        )
        .unwrap();
        run(&["add", "."]);
        run(&["commit", "--quiet", "-m", "initial commit"]);
        run(&[
            "remote",
            "add",
            "origin",
            "https://example.invalid/nonexistent/repo.git",
        ]);

        let worktree = repo.path().join("worktree");
        run(&["worktree", "add", "--detach", worktree.to_str().unwrap()]);

        let claude_dir = TempDir::new().unwrap();
        std::fs::write(claude_dir.path().join(".credentials.json"), "{}").unwrap();

        (repo, worktree, claude_dir)
    }

    /// Issue #49 review, MEDIUM: a missing image (the most likely first-run
    /// error, since the image is hand-built, never published anywhere) must
    /// surface as a typed, actionable `DockerUnavailable` -- not exit code
    /// 125 silently reinterpreted downstream as "the agent produced no
    /// parseable output". Requires a reachable daemon (to actually get as
    /// far as `docker`'s own "image not found" path) but mutates nothing on
    /// it -- `docker run` against a nonexistent image never touches
    /// anything persistent.
    #[tokio::test]
    async fn execute_reports_docker_unavailable_when_the_image_does_not_exist() {
        if !docker_daemon_available().await {
            eprintln!("skipping: no docker daemon reachable");
            return;
        }

        let (repo, worktree, claude_dir) = init_repo_with_worktree_and_claude_dir();
        let sandbox = DockerSandbox::new(DockerConfig {
            image: "warden-agent-image-that-does-not-exist-anywhere:latest".to_string(),
            repo_path: repo.path().to_path_buf(),
            claude_config_dir: claude_dir.path().to_path_buf(),
        });
        let id = sandbox
            .create(SandboxSpec {
                cwd: worktree.clone(),
            })
            .await
            .unwrap();

        let execution = sandbox
            .execute(
                &id,
                Command {
                    program: "true".to_string(),
                    args: Vec::new(),
                    env_allowlist: Vec::new(),
                    stdin: None,
                },
                ExecuteOptions::default(),
            )
            .await
            .unwrap();
        let result = execution.wait().await;

        assert!(
            matches!(result, Err(SandboxError::DockerUnavailable { .. })),
            "expected a typed DockerUnavailable for a missing image, got {result:?}"
        );

        sandbox.destroy(id).await.unwrap();
    }

    /// Issue #49 acceptance criterion (issue #28): from inside the
    /// container, `git push origin` must fail -- no ssh key, no
    /// `~/.config/gh`, no credential helper is ever mounted, so there is
    /// nothing for git to authenticate a push with, regardless of network
    /// reachability.
    #[tokio::test]
    async fn e2e_git_push_origin_fails_inside_the_container_no_credentials_mounted() {
        if !docker_daemon_available().await {
            eprintln!("skipping: no docker daemon reachable");
            return;
        }

        let (repo, worktree, claude_dir) = init_repo_with_worktree_and_claude_dir();
        let sandbox = DockerSandbox::new(DockerConfig {
            image: TEST_IMAGE.to_string(),
            repo_path: repo.path().to_path_buf(),
            claude_config_dir: claude_dir.path().to_path_buf(),
        });
        let id = sandbox
            .create(SandboxSpec {
                cwd: worktree.clone(),
            })
            .await
            .unwrap();

        let execution = sandbox
            .execute(
                &id,
                Command {
                    program: "sh".to_string(),
                    args: vec![
                        "-c".to_string(),
                        "apk add --no-cache git >/dev/null 2>&1 && git push origin HEAD"
                            .to_string(),
                    ],
                    env_allowlist: vec!["HOME".to_string(), "USER".to_string()],
                    stdin: None,
                },
                ExecuteOptions::default(),
            )
            .await
            .unwrap();
        let outcome = execution.wait().await.unwrap();

        assert_ne!(
            outcome.exit_code, 0,
            "git push origin must fail: stderr was {}",
            outcome.stderr
        );

        sandbox.destroy(id).await.unwrap();
    }

    /// Issue #49 acceptance criterion (issue #25): the host's real `~/.ssh`
    /// (and `~/.aws`) must not be reachable by absolute path from inside the
    /// container -- only `~/.claude` is ever mounted, nothing else of the
    /// host's real `$HOME`.
    #[tokio::test]
    async fn e2e_host_ssh_and_aws_dirs_are_not_reachable_inside_the_container() {
        if !docker_daemon_available().await {
            eprintln!("skipping: no docker daemon reachable");
            return;
        }

        let (repo, worktree, claude_dir) = init_repo_with_worktree_and_claude_dir();
        let sandbox = DockerSandbox::new(DockerConfig {
            image: TEST_IMAGE.to_string(),
            repo_path: repo.path().to_path_buf(),
            claude_config_dir: claude_dir.path().to_path_buf(),
        });
        let id = sandbox
            .create(SandboxSpec {
                cwd: worktree.clone(),
            })
            .await
            .unwrap();

        let execution = sandbox
            .execute(
                &id,
                Command {
                    program: "sh".to_string(),
                    args: vec![
                        "-c".to_string(),
                        "test -e /root/.ssh && echo SSH_FOUND; test -e /root/.aws && echo AWS_FOUND; \
                         test -e $HOME/.claude/.credentials.json && echo CLAUDE_FOUND; exit 0"
                            .to_string(),
                    ],
                    env_allowlist: vec!["HOME".to_string()],
                    stdin: None,
                },
                ExecuteOptions::default(),
            )
            .await
            .unwrap();
        let outcome = execution.wait().await.unwrap();

        assert!(
            !outcome.stdout.contains("SSH_FOUND"),
            "host ~/.ssh must not be reachable inside the container: stdout was {}",
            outcome.stdout
        );
        assert!(
            !outcome.stdout.contains("AWS_FOUND"),
            "host ~/.aws must not be reachable inside the container: stdout was {}",
            outcome.stdout
        );
        assert!(
            outcome.stdout.contains("CLAUDE_FOUND"),
            "~/.claude must be reachable (read-only) at the container HOME: stdout was {}",
            outcome.stdout
        );

        sandbox.destroy(id).await.unwrap();
    }

    /// Issue #49: after `destroy`, no `warden-*` container is left behind --
    /// `docker ps -a` must show nothing under this test's own id.
    #[tokio::test]
    async fn destroy_leaves_no_container_behind() {
        if !docker_daemon_available().await {
            eprintln!("skipping: no docker daemon reachable");
            return;
        }

        let (repo, worktree, claude_dir) = init_repo_with_worktree_and_claude_dir();
        let sandbox = DockerSandbox::new(DockerConfig {
            image: TEST_IMAGE.to_string(),
            repo_path: repo.path().to_path_buf(),
            claude_config_dir: claude_dir.path().to_path_buf(),
        });
        let id = sandbox
            .create(SandboxSpec {
                cwd: worktree.clone(),
            })
            .await
            .unwrap();
        let container_name = id.to_string();

        let execution = sandbox
            .execute(
                &id,
                Command {
                    program: "true".to_string(),
                    args: Vec::new(),
                    env_allowlist: Vec::new(),
                    stdin: None,
                },
                ExecuteOptions::default(),
            )
            .await
            .unwrap();
        execution.wait().await.unwrap();

        sandbox.destroy(id).await.unwrap();

        assert!(
            !container_exists(&container_name).await,
            "no `{container_name}` container should remain after destroy"
        );
    }

    /// Issue #49: a cancelled execution must not leak a running container --
    /// see [`drain_and_wait_with_container_cleanup`]'s own docs for why
    /// killing the `docker run` client alone is not enough.
    #[tokio::test]
    async fn cancelling_an_execution_leaves_no_container_behind() {
        if !docker_daemon_available().await {
            eprintln!("skipping: no docker daemon reachable");
            return;
        }

        let (repo, worktree, claude_dir) = init_repo_with_worktree_and_claude_dir();
        let sandbox = DockerSandbox::new(DockerConfig {
            image: TEST_IMAGE.to_string(),
            repo_path: repo.path().to_path_buf(),
            claude_config_dir: claude_dir.path().to_path_buf(),
        });
        let id = sandbox
            .create(SandboxSpec {
                cwd: worktree.clone(),
            })
            .await
            .unwrap();
        let container_name = id.to_string();
        let cancel = CancellationToken::new();

        let execution = sandbox
            .execute(
                &id,
                Command {
                    program: "sleep".to_string(),
                    args: vec!["30".to_string()],
                    env_allowlist: Vec::new(),
                    stdin: None,
                },
                ExecuteOptions {
                    cancel: cancel.clone(),
                    on_stdout_line: None,
                },
            )
            .await
            .unwrap();

        // Give the container a moment to actually start before cancelling,
        // so this test exercises "kill a running container", not "the
        // container never started at all".
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        cancel.cancel();
        let result = execution.wait().await;
        assert!(matches!(result, Err(SandboxError::Cancelled { .. })));

        // Best-effort cleanup can race the daemon actually finishing the
        // removal -- poll briefly rather than asserting immediately.
        let mut still_there = container_exists(&container_name).await;
        for _ in 0..20 {
            if !still_there {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            still_there = container_exists(&container_name).await;
        }
        assert!(
            !still_there,
            "no `{container_name}` container should remain after cancellation"
        );

        sandbox.destroy(id).await.unwrap();
    }

    /// `docker ps -a --filter name=<exact>` -- exact-name filtered so a
    /// prefix match against another test's container never produces a false
    /// positive.
    async fn container_exists(container_name: &str) -> bool {
        let output = tokio::process::Command::new(DOCKER_BIN)
            .args([
                "ps",
                "-a",
                "--filter",
                &format!("name=^{container_name}$"),
                "--format",
                "{{.Names}}",
            ])
            .output()
            .await
            .expect("spawn docker ps");
        !String::from_utf8_lossy(&output.stdout).trim().is_empty()
    }
}
