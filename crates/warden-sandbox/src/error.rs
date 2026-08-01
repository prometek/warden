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

    #[error("failed to write payload to `{program}` stdin: {source}")]
    StdinWrite {
        program: String,
        #[source]
        source: std::io::Error,
    },

    #[error("unknown sandbox id {id}")]
    UnknownSandbox { id: SandboxId },

    #[error("docker sandbox misconfigured: {reason}")]
    DockerUnavailable { reason: String },
}

pub type Result<T> = std::result::Result<T, SandboxError>;
