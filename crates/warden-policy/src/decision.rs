//! [`Decision`] (issue #51, ADR-0016): the verdict [`crate::Evaluator::evaluate`]
//! returns for one [`crate::Action`].

/// What the policy engine decided about one evaluated [`crate::Action`].
///
/// This is a **governance** verdict, not an enforcement one: `Allow` means
/// "the policy layer raises no objection", never "this action is now
/// guaranteed to happen" -- for a `git_push` action specifically, the actual
/// push to the real `origin` remote still goes exclusively through
/// `warden-gated`'s own independent, re-verified barrier (ADR-0002/0006).
/// `warden-policy` informs the orchestrator upstream of that barrier; it
/// never replaces it (ADR-0016). Likewise, `Deny` for a `shell` action is
/// advisory defence-in-depth, not a security boundary -- see
/// `crate::rules`'s own "`deny` is not a security control" docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// No matching rule objects to this action (or every matching rule
    /// allows it outright) -- the action may proceed.
    Allow,
    /// A matching rule forbids this action outright. `reason` is
    /// human-readable and always names *why* (e.g. the denied pattern that
    /// matched), for the same "actionable error" reason
    /// `warden_core::HookOutcome::Block`'s own `reason` field exists.
    Deny { reason: String },
    /// A matching rule allows this action only with a human's explicit
    /// sign-off (e.g. `require: [tests, review]` on a `git_push` rule). The
    /// caller is responsible for suspending at a human-validation wait point
    /// before treating this as an `Allow` -- see
    /// `warden::policy_gate::PolicyGate::decide`'s own docs for the concrete
    /// wiring.
    RequireApproval { reason: String },
}
