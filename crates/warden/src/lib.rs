//! Warden orchestrator library: I/O layer (worktrees, subprocesses, SQLite)
//! plus the coordination that drives `warden-core`'s pure state machine.

pub mod db;
pub mod error;
pub mod event_bus;
pub mod evidence;
pub mod orchestrator;
pub mod pr_summary;
pub mod process;
pub mod worktree;
