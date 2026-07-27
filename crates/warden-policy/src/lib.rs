//! **`warden-policy`** (issue #51, ADR-0016) -- the explicit **decision**
//! layer governance was missing before this crate: which actions an agent
//! (or a deterministic hook Warden runs on its behalf) may perform, which
//! require human approval, which are forbidden outright.
//!
//! Three pieces, one per module:
//! - [`rules`] -- the declarative shape of `.warden/policy.yaml` ([`RuleSet`]/
//!   [`Rule`]) and the [`Action`] vocabulary a rule can govern. Pure parsing
//!   only, no filesystem access (mirrors `warden_core::workflow::Workflow`'s
//!   own "pure parse, caller does I/O" split).
//! - [`evaluator`] -- [`Evaluator::evaluate`], which reduces every rule
//!   matching one [`Action`] to a single [`Decision`].
//! - [`decision`] -- [`Decision`] itself: `Allow` / `Deny` / `RequireApproval`.
//!
//! # Boundary with `warden-gated` (ADR-0016)
//!
//! This crate is an **upstream governance layer**, not an enforcement
//! barrier. It informs the orchestrator (`warden::policy_gate`, in the
//! `warden` crate) which action to allow, block, or hold for a human -- it
//! never itself pushes anything, holds any credential, or talks to a git
//! remote. The **final** barrier on `git push origin` stays exactly what
//! ADR-0002/0006 already established: `warden-gated`, a separate binary,
//! independently re-verifying `RunState == Converged` against SQLite before
//! ever pushing to the real remote. This crate is entirely unreachable from
//! that path -- `warden-gated` does not, and must never, depend on it.
//!
//! # Dependency direction
//!
//! This crate depends on nothing from `warden-core` or `warden` -- `Action`/
//! `Decision`/`Rule` are this crate's own, minimal types, not a reuse of
//! `warden_core`'s. `warden-core` therefore has no path to ever depend on
//! this crate (code-standards.md: strict separation of pure logic from
//! everything layered on top of it), and neither does `warden-gated`.

mod decision;
mod evaluator;
mod rules;

pub mod error;

pub use decision::Decision;
pub use evaluator::Evaluator;
pub use rules::{Action, Rule, RuleSet};
