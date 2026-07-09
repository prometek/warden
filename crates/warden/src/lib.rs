//! Warden orchestrator library: I/O layer (worktrees, subprocesses, SQLite)
//! plus the coordination that drives `warden-core`'s pure state machine.

pub mod db;
pub mod error;
pub mod orchestrator;
pub mod process;
pub mod worktree;
