//! SQLite persistence (ADR-0004). `warden` is the only writer; schema
//! covers `runs`, `cycles`, `findings`, `agent_processes`, `evidence`
//! (Phase 7, ADR-0009, issue #7), and (Phase 8, ADR-0008) `events`. Every
//! row read back is reparsed into a strongly-typed Rust value before
//! leaving this module — callers never see raw strings for
//! `state`/`role`/`source`/`severity`/`event_type`.

use std::path::Path;
use std::time::Duration;

use chrono::Utc;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::SqlitePool;
use warden_core::{
    EventKind, EvidenceType, Finding, FindingSource, RateLimitState, RateLimitStatus,
    RateLimitWindow, RunEvent, RunEventHistoryEntry, RunEventRecord, RunState, Severity,
    TokenUsage, UndecodableEvent, UndecodableReason,
};

use crate::error::{Result, WardenError};

mod connection;
mod cycles;
mod processes;
mod quota;
mod runs;
mod timeline;

pub use connection::connect;
pub use cycles::*;
pub use processes::*;
pub use quota::*;
pub use runs::*;
pub use timeline::*;

#[cfg(test)]
use connection::{backup_before_migration, unique_backup_path, MIGRATOR};

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339()
}

/// Converts a `INTEGER` column value into a `u32`, returning a typed error
/// instead of silently clamping/defaulting on overflow (code-standards.md:
/// "no silent fallback"). Every row written by this module comes from a
/// `u32` in the first place, so failure here means the stored value was
/// corrupted or written by something other than this code — worth
/// surfacing, not hiding.
fn checked_u32(value: i64, column: &'static str) -> Result<u32> {
    u32::try_from(value).map_err(|_| WardenError::InvalidStoredValue { column, value })
}

/// Same contract as [`checked_u32`], for the wider token-count columns
/// (issue #53) -- a single agent invocation's usage comfortably fits `u32`,
/// but a run's accumulated total (`add_run_token_usage`) is not bounded the
/// same way over an arbitrarily long-running convergence loop.
fn checked_u64(value: i64, column: &'static str) -> Result<u64> {
    u64::try_from(value).map_err(|_| WardenError::InvalidStoredValue { column, value })
}

/// Converts a `u64` [`TokenUsage`] field into SQLite's native `i64` integer.
fn checked_i64(value: u64, column: &'static str) -> Result<i64> {
    i64::try_from(value).map_err(|_| WardenError::TokenCountOverflow { column, value })
}

/// Converts a possibly-`NULL` `TokenUsage` column group read back from
/// `cycles`/`runs` (issue #53) into `Option<TokenUsage>` -- `None` only when
/// *every* one of the four columns is `NULL` (no usage was ever recorded for
/// this role/run), never when just the two cache columns are (a tool that
/// reports input/output but never caching is still a real, known usage
/// report, not "n/a" -- see `warden_core::TokenUsage`'s own docs on this
/// distinction).
fn row_to_token_usage(
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cache_read_tokens: Option<i64>,
    cache_creation_tokens: Option<i64>,
) -> Result<Option<TokenUsage>> {
    if input_tokens.is_none()
        && output_tokens.is_none()
        && cache_read_tokens.is_none()
        && cache_creation_tokens.is_none()
    {
        return Ok(None);
    }
    Ok(Some(TokenUsage::new(
        checked_u64(input_tokens.unwrap_or(0), "input_tokens")?,
        checked_u64(output_tokens.unwrap_or(0), "output_tokens")?,
        cache_read_tokens
            .map(|value| checked_u64(value, "cache_read_tokens"))
            .transpose()?,
        cache_creation_tokens
            .map(|value| checked_u64(value, "cache_creation_tokens"))
            .transpose()?,
    )))
}

#[cfg(test)]
mod tests;
