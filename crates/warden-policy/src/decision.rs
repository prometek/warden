//! [`Decision`]: the verdict [`crate::Evaluator::evaluate`] returns for one [`crate::Action`].

/// What the policy engine decided about one evaluated [`crate::Action`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// No matching rule objects to this action (or every matching rule allows it outright) -- the
    /// action may proceed.
    Allow,
    /// A matching rule forbids this action outright.
    Deny { reason: String },
    /// A matching rule allows this action only with a human's explicit sign-off.
    RequireApproval { reason: String },
}
