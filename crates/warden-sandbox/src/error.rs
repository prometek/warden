//! Error type for the `warden-sandbox` crate.

use thiserror::Error;

use crate::SandboxId;

#[derive(Debug, Error)]
pub enum SandboxError {
    #[error("failed to spawn `{program}`: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },

    #[error("execution of `{program}` was cancelled")]
    Cancelled { program: String },

    #[error("failed to wait on `{program}`: {source}")]
    Wait {
        program: String,
        #[source]
        source: std::io::Error,
    },

    /// A stdin write failed with something other than a broken pipe (an
    /// agent that closes/never reads stdin is legitimate and handled
    /// separately -- see [`crate::local::LocalSandbox`]'s own docs). Mirrors
    /// `warden::error::ProcessError::StdinWrite`'s reasoning exactly: the
    /// payload is a single JSON object, so a partial write is unparsable by
    /// construction, and continuing would silently run the agent with no
    /// context at all.
    #[error("failed to write payload to `{program}` stdin: {source}")]
    StdinWrite {
        program: String,
        #[source]
        source: std::io::Error,
    },

    /// [`crate::Sandbox::execute`] or [`crate::Sandbox::destroy`] was called
    /// with a [`SandboxId`] that this backend never [`crate::Sandbox::create`]d
    /// (or already [`crate::Sandbox::destroy`]ed) -- a caller bug, never a
    /// runtime condition to paper over.
    #[error("unknown sandbox id {id}")]
    UnknownSandbox { id: SandboxId },

    /// A [`crate::docker::DockerSandbox`]-specific precondition or
    /// configuration failure that has no natural `Spawn`/`Wait`/`Cancelled`/
    /// `StdinWrite` counterpart: a host path required for a bind mount that
    /// cannot be resolved (canonicalized), the one host path this backend
    /// requires for auth (`~/.claude`) not existing at all, or `docker rm -f`
    /// itself failing for a reason other than "already gone" during
    /// [`crate::Sandbox::destroy`]. Never used for a `docker` invocation's
    /// own outcome -- a non-zero exit from whatever ran *inside* the
    /// container (including the daemon being unreachable, surfaced through
    /// `docker run`'s own stderr/exit code) is a normal
    /// [`crate::ExecutionResult`], not this variant.
    #[error("docker sandbox misconfigured: {reason}")]
    DockerUnavailable { reason: String },
}

pub type Result<T> = std::result::Result<T, SandboxError>;
