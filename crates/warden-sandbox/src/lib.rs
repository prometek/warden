//! The **sandbox seam** plus its second backend, [`DockerSandbox`].

mod docker;
mod drain;
mod error;
mod local;

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

pub use docker::{
    reclaim_run_containers, DockerConfig, DockerEgressConfig, DockerRunOptions, DockerSandbox,
};
pub use error::{Result, SandboxError};
pub use local::LocalSandbox;

/// Opaque handle to one sandbox instance, scoped to a single
/// [`Sandbox::create`]/[`Sandbox::destroy`] pair.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SandboxId(String);

impl SandboxId {
    pub(crate) fn generate() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl std::fmt::Display for SandboxId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone)]
pub struct SandboxSpec {
    pub cwd: PathBuf,
}

#[derive(Clone, Default)]
pub struct Command {
    pub program: String,
    pub args: Vec<String>,
    pub env_allowlist: Vec<String>,
    pub stdin: Option<String>,
}

/// Hand-written: `stdin` carries the serialized `warden_core::AgentInputMessage`.
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

/// Execution knobs that aren't part of the command itself: cancellation, and an optional per-
/// stdout-line callback.
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

pub struct Execution<'a> {
    pub pid: Option<u32>,
    future: Pin<Box<dyn Future<Output = Result<ExecutionResult>> + Send + 'a>>,
}

impl<'a> Execution<'a> {
    /// Public so a [`Sandbox`] implementation built entirely outside this crate.
    pub fn new(
        pid: Option<u32>,
        future: impl Future<Output = Result<ExecutionResult>> + Send + 'a,
    ) -> Self {
        Self {
            pid,
            future: Box::pin(future),
        }
    }

    /// Awaits this execution to completion (or cancellation).
    pub async fn wait(self) -> Result<ExecutionResult> {
        self.future.await
    }
}

#[async_trait]
pub trait Sandbox: Send + Sync {
    /// Provisions a sandbox bound to `spec` and returns its id.
    async fn create(&self, spec: SandboxSpec) -> Result<SandboxId>;

    /// Provisions a sandbox recoverable by its owning run after a process crash.
    async fn create_for_run(&self, spec: SandboxSpec, _run_id: &str) -> Result<SandboxId> {
        self.create(spec).await
    }

    /// Runs `command` inside the sandbox named `id`, applying `options`.
    async fn execute<'a>(
        &'a self,
        id: &'a SandboxId,
        command: Command,
        options: ExecuteOptions<'a>,
    ) -> Result<Execution<'a>>;

    async fn destroy(&self, id: SandboxId) -> Result<()>;
}
