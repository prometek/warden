//! Warden orchestrator library: I/O layer (worktrees, subprocesses, SQLite)
//! plus the coordination that drives `warden-core`'s pure state machine.

pub mod agent_def;
pub mod ci_channel;
pub mod db;
pub mod error;
pub mod event_bus;
pub mod evidence;
pub mod gate_trigger;
pub mod hook;
pub mod orchestrator;
pub(crate) mod path_util;
pub mod pr_summary;
pub mod process;
pub mod tool_adapter;
pub mod worktree;
