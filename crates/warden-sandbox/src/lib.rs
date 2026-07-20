//! The **sandbox seam** (issue #50, foundation for #49's `DockerSandbox`).
//!
//! # Worktree vs. sandbox
//!
//! Warden isolates two different things, and this crate is only responsible
//! for the second one:
//! - The **worktree** (`warden::worktree::Worktree`) isolates the *code*: a
//!   dedicated `git worktree` per role/run, so a coder/reviewer/tester never
//!   share a working directory.
//! - The **sandbox** (this crate) isolates the *execution environment* an
//!   agent's own process runs in: today, nothing more than the process
//!   isolation `warden::process` already applied by hand
//!   (`env_clear()`, `cwd`, `kill_on_drop`); tomorrow (#49), an actual
//!   container.
//!
//! These compose, they do not replace one another -- a sandbox always runs
//! *on top of* a worktree (its [`SandboxSpec::cwd`] names one), never
//! instead of it.
//!
//! # The `Sandbox` trait
//!
//! [`Sandbox::create`] binds a fresh [`SandboxId`] to a [`SandboxSpec`]
//! (today: just a `cwd`); [`Sandbox::execute`] runs one [`Command`] inside
//! that sandbox; [`Sandbox::destroy`] tears it down. This mirrors the
//! lifecycle a container backend needs (`docker create` -> `docker exec` (any
//! number of times) -> `docker rm`) while [`LocalSandbox`] -- the only
//! implementation this issue ships -- keeps it a no-op beyond bookkeeping:
//! "no container" is the whole point of Local, by design (see
//! [`LocalSandbox`]'s own docs).
//!
//! `execute` returns an [`Execution`] rather than an [`ExecutionResult`]
//! directly: a caller (`warden::orchestrator::Orchestrator::run_agent`) needs
//! the OS pid the *instant* the process starts, to persist it to SQLite
//! *before* awaiting completion -- that write-ahead is what makes crash
//! detection meaningful (if the orchestrator itself dies while awaiting, the
//! pid it already wrote is what recovery checks on restart). Folding
//! `execute` into a single spawn-then-wait-to-completion call, as this
//! trait's very first sketch did, would have made that ordering impossible
//! to express through the seam at all. [`Execution::pid`] is available the
//! moment [`Sandbox::execute`] returns; [`Execution::wait`] is a second,
//! separate `.await` for the caller to reach only once anything that must
//! happen before the process runs to completion (the pid write, an
//! `AgentStarted` event) is durable.
//!
//! # Dependency direction
//!
//! This crate depends on nothing from `warden` or `warden-core` -- `Command`/
//! `ExecutionResult` are this crate's own, minimal types, not a reuse of
//! `warden_core`'s. `warden-core` therefore has no path to ever depend on
//! this crate, keeping it exactly as pure as before this issue.

mod error;
mod local;

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

pub use error::{Result, SandboxError};
pub use local::LocalSandbox;

/// Opaque handle to one sandbox instance, scoped to a single
/// [`Sandbox::create`]/[`Sandbox::destroy`] pair. Backend-specific
/// (`LocalSandbox` mints a `uuid`; a future `DockerSandbox` would use the
/// container id) -- a real caller only ever receives one back from
/// [`Sandbox::create`], never constructs one to pass in.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SandboxId(String);

impl SandboxId {
    /// Mints a fresh, random id -- what every [`LocalSandbox::create`] call
    /// uses today; a future `DockerSandbox` would use the real container id
    /// returned by the Docker daemon instead.
    pub(crate) fn generate() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    /// Wraps an already-known id (issue #50 review, MEDIUM 2). The public
    /// constructor `Sandbox` itself was missing: with only
    /// [`SandboxId::generate`] (`pub(crate)`) available, no id could be
    /// produced outside this crate, which made [`Sandbox::create`]'s return
    /// type -- and therefore the whole trait -- unimplementable by anything
    /// other than [`LocalSandbox`], including a recording fake a test would
    /// install through `Orchestrator::with_sandbox`. A future `DockerSandbox`
    /// (#49) uses this to wrap the container id the Docker daemon hands
    /// back.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl std::fmt::Display for SandboxId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// What a sandbox is created against. Only a `cwd` today (the worktree an
/// agent's process runs against, "the sandbox runs on top of the worktree" --
/// see this crate's own docs); a `DockerSandbox` (#49) would extend this with
/// an image/mount configuration, never replace `cwd`'s role.
#[derive(Debug, Clone)]
pub struct SandboxSpec {
    pub cwd: PathBuf,
}

/// One command to run inside a sandbox: program, args, the environment
/// variable *names* (never values -- see [`LocalSandbox`]'s own docs on why
/// resolving them is this crate's job, not the caller's) to forward on top of
/// whatever baseline the backend applies, and an optional payload to write to
/// the child's stdin before closing it.
#[derive(Clone, Default)]
pub struct Command {
    pub program: String,
    pub args: Vec<String>,
    pub env_allowlist: Vec<String>,
    pub stdin: Option<String>,
}

/// Hand-written (issue #50 review, LOW 7): `stdin` carries the serialized
/// `warden_core::AgentInputMessage` -- the run intent plus the full diff --
/// so a derived `Debug` would leak it into any log/panic message that prints
/// a `Command`. Redacted to a byte count instead, the same shape
/// `e2e_run_intent_never_leaks_into_argv` already guards for argv.
impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Command")
            .field("program", &self.program)
            .field("args", &self.args)
            .field("env_allowlist", &self.env_allowlist)
            .field(
                "stdin",
                &self
                    .stdin
                    .as_ref()
                    .map(|payload| format!("<{} bytes redacted>", payload.len())),
            )
            .finish()
    }
}

/// Execution knobs that aren't part of the command itself: cancellation, and
/// an optional per-stdout-line callback (issue #33 parity -- see
/// [`LocalSandbox`]'s own docs for the same deadlock-avoidance and
/// must-not-block contract `warden::process::wait_with_progress` already
/// documented). Kept out of [`Command`] so that type stays a plain,
/// `Debug`/`Clone`-able description of what to run.
pub struct ExecuteOptions<'a> {
    pub cancel: CancellationToken,
    pub on_stdout_line: Option<&'a (dyn Fn(&str) + Send + Sync)>,
}

impl Default for ExecuteOptions<'_> {
    fn default() -> Self {
        Self {
            cancel: CancellationToken::new(),
            on_stdout_line: None,
        }
    }
}

/// Outcome of a completed (non-cancelled) execution.
#[derive(Debug)]
pub struct ExecutionResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// A still-running (or already-failed-to-start) execution: the pid, if one
/// is already known, plus a future a caller `.await`s separately once it has
/// done whatever must happen *before* completion (see this crate's own docs
/// on why `execute` is split this way). Borrows `'a` from the [`Sandbox`]
/// call that produced it and from [`ExecuteOptions::on_stdout_line`] --
/// never stored beyond the single `execute`/`wait` pair it belongs to.
pub struct Execution<'a> {
    pub pid: Option<u32>,
    future: Pin<Box<dyn Future<Output = Result<ExecutionResult>> + Send + 'a>>,
}

impl<'a> Execution<'a> {
    fn new(
        pid: Option<u32>,
        future: impl Future<Output = Result<ExecutionResult>> + Send + 'a,
    ) -> Self {
        Self {
            pid,
            future: Box::pin(future),
        }
    }

    /// Awaits this execution to completion (or cancellation). A separate
    /// call from [`Sandbox::execute`] on purpose -- see this crate's own
    /// module docs.
    pub async fn wait(self) -> Result<ExecutionResult> {
        self.future.await
    }
}

/// Isolates the *execution environment* one agent invocation runs in --
/// never the code (the worktree already does that, see this crate's own
/// docs). `Send + Sync` so a caller can hold one behind `Arc<dyn Sandbox>`
/// and select a backend at construction time without the call site (today:
/// `warden::orchestrator::Orchestrator::run_agent`) ever needing to change
/// again when a new backend is added (#49).
#[async_trait]
pub trait Sandbox: Send + Sync {
    /// Provisions a sandbox bound to `spec` and returns its id. For
    /// [`LocalSandbox`] this is pure bookkeeping (no container is actually
    /// created, by design); a `DockerSandbox` would create/start a real
    /// container here.
    async fn create(&self, spec: SandboxSpec) -> Result<SandboxId>;

    /// Runs `command` inside the sandbox named `id`, applying `options`.
    /// Returns as soon as the process has started (see this crate's own
    /// docs on why `execute` doesn't run straight to completion) -- the
    /// returned [`Execution`] is what the caller awaits for the actual
    /// result.
    async fn execute<'a>(
        &'a self,
        id: &'a SandboxId,
        command: Command,
        options: ExecuteOptions<'a>,
    ) -> Result<Execution<'a>>;

    /// Tears down the sandbox named `id`. Idempotent: destroying an id that
    /// is already gone (or was never created by this instance) is not an
    /// error -- there is simply nothing left to tear down, the same
    /// "already gone is not an error" convention
    /// `warden::process::kill_pid` uses for an orphan process.
    async fn destroy(&self, id: SandboxId) -> Result<()>;
}
