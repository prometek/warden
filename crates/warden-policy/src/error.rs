//! Error type for the `warden-policy` crate.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum PolicyError {
    #[error("invalid policy YAML: {0}")]
    InvalidYaml(String),
}

pub type Result<T> = std::result::Result<T, PolicyError>;
