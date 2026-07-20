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
}

pub type Result<T> = std::result::Result<T, SandboxError>;
