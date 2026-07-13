//! `warden-tui`: a strictly read-only run monitor (ADR-0008, issue #8).
//!
//! No module in this crate ever writes to `warden`'s SQLite database, spawns
//! an agent, or touches git (code-standards.md, "TUI (ratatui)"). The only
//! two inputs are: the `events` table, read-only, for history replay
//! ([`db`]); and the run's Event Bus Unix socket, subscribed to read-only,
//! for the live stream ([`subscriber`]). [`model`] is the pure projection of
//! those two sources into what the UI renders ([`ui`]) -- testable without a
//! terminal, per code-standards.md's explicit requirement for this crate.

pub mod attach;
pub mod capabilities;
pub mod db;
pub mod error;
pub mod evidence;
pub mod model;
pub mod subscriber;
pub mod ui;
