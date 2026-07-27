//! Error type for the `warden-policy` crate.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum PolicyError {
    /// Covers every parse failure of a `.warden/policy.yaml` document (or
    /// any other source string handed to [`crate::RuleSet::from_yaml`]):
    /// malformed YAML, an unknown top-level key, a rule naming an `action`
    /// this crate does not know how to evaluate, or a field that means
    /// nothing for the rule's own `action` (e.g. `branch` on a `shell`
    /// rule). `Rule`'s internally tagged representation (`crate::rules`'s
    /// own docs) makes `serde` itself reject all of these per-variant, so
    /// there is a single failure shape here rather than a bespoke variant
    /// per validation rule. Per code-standards.md ("no silent fallback"), a
    /// present-but-broken policy file must fail to load, never be silently
    /// treated as an empty rule set.
    #[error("invalid policy YAML: {0}")]
    InvalidYaml(String),
}

pub type Result<T> = std::result::Result<T, PolicyError>;
