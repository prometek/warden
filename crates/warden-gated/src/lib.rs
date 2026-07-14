//! `warden-gated` library: the git gate daemon's I/O layer (read-only
//! SQLite, git push, Unix socket relay, hook installation) plus the pure
//! re-verification rule that decides whether a push reaches `origin`
//! (ADR-0002/ADR-0006, issue #3).
//!
//! Deliberately does **not** depend on the `warden` crate -- only on
//! `warden-core`'s shared types (`RunState`) -- so a bug in `warden`'s own
//! database or orchestration code can never leak into the gate's decision
//! (Architecture.md §13: "crate partagée `warden-core` (types seulement,
//! pas la logique de vérification — dupliquée volontairement côté gate)").

pub mod bare_repo;
pub mod ci_report;
pub mod ci_watcher;
pub mod db;
pub mod error;
pub mod gate;
pub mod gh_provider;
pub mod hook;
pub mod notification;
pub mod pr_manager;
pub mod push;
pub mod relay;
pub mod serve;
pub mod verify;
