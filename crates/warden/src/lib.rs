//! Warden orchestrator library: I/O layer (worktrees, subprocesses, SQLite)
//! plus the coordination that drives `warden-core`'s pure state machine.

pub mod agent_def;
pub mod ci_channel;
pub mod db;
pub mod error;
pub mod event_bus;
pub mod evidence;
pub mod gate_trigger;
pub mod orchestrator;
pub mod pr_summary;
pub mod process;
pub mod tool_adapter;
pub mod worktree;
