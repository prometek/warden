//! Error type for the `warden-policy` crate.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum PolicyError {
    /// `.warden/policy.yaml` (or any other source string handed to
    /// [`crate::RuleSet::from_yaml`]) is not well-formed YAML, or has an
    /// unknown top-level key. Per code-standards.md ("no silent fallback"), a
    /// present-but-broken policy file must fail to load, never be silently
    /// treated as an empty rule set.
    #[error("invalid policy YAML: {0}")]
    InvalidYaml(String),

    /// Rule `#index` (0-based, in file order) names an `action` this crate
    /// does not know how to evaluate. Closed on purpose (code-standards.md:
    /// no magic strings reaching the evaluator un-validated) -- a typo in
    /// `.warden/policy.yaml` must fail to load, not silently never match
    /// anything.
    #[error(
        "rule #{index} names an unknown action {action:?} (expected \"git_push\" or \"shell\")"
    )]
    UnknownAction { index: usize, action: String },
}

pub type Result<T> = std::result::Result<T, PolicyError>;
