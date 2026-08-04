//! SQLite persistence. `warden` is the only writer; schema covers `runs`, `cycles`, `findings`,
//! `agent_processes`, `evidence`, and `events`.

use std::path::Path;
use std::time::Duration;

use chrono::Utc;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::SqlitePool;
use warden_core::{
    EventKind, EvidenceType, Finding, FindingSource, ProgressReplay, RateLimitState,
    RateLimitStatus, RateLimitWindow, RunEvent, RunEventHistoryEntry, RunEventRecord, RunState,
    Severity, TokenUsage, UndecodableEvent, UndecodableReason,
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

/// Converts a `INTEGER` column value into a `u32`, returning a typed error instead of silently
/// clamping/defaulting on overflow (code-standards.md: "no silent fallback").
fn checked_u32(value: i64, column: &'static str) -> Result<u32> {
    u32::try_from(value).map_err(|_| WardenError::InvalidStoredValue { column, value })
}

fn checked_u64(value: i64, column: &'static str) -> Result<u64> {
    u64::try_from(value).map_err(|_| WardenError::InvalidStoredValue { column, value })
}

/// Converts a `u64` [`TokenUsage`] field into SQLite's native `i64` integer.
fn checked_i64(value: u64, column: &'static str) -> Result<i64> {
    i64::try_from(value).map_err(|_| WardenError::TokenCountOverflow { column, value })
}

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
