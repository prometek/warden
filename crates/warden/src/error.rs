//! Error types for the `warden` binary/library.

use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum WorktreeError {
    #[error("{0} is not a git repository (no .git found)")]
    NotAGitRepo(PathBuf),

    #[error(
        "worktrees root {worktrees_root} must not be inside the main repository's working tree {main_repo}"
    )]
    UnsafeWorktreesRoot {
        main_repo: PathBuf,
        worktrees_root: PathBuf,
    },

    #[error("git command `{command}` failed (exit {exit_code:?}): {stderr}")]
    GitCommandFailed {
        command: String,
        exit_code: Option<i32>,
        stderr: String,
    },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Error)]
pub enum ProcessError {
    #[error("failed to spawn `{command}`: {source}")]
    Spawn {
        command: String,
        #[source]
        source: std::io::Error,
    },

    #[error("process for `{command}` was cancelled")]
    Cancelled { command: String },

    #[error("failed to wait on `{command}`: {source}")]
    Wait {
        command: String,
        #[source]
        source: std::io::Error,
    },

    /// `Child::id()` returned `None` right after spawn. This must never be
    /// silently treated as pid 0 — POSIX `kill(0, ...)` signals the
    /// caller's entire process group, so a pid-0 sentinel would make crash
    /// recovery misreport that process as permanently alive (see
    /// `process::is_process_alive`).
    #[error(
        "child process for `{command}` has no PID (already reaped before it could be observed)"
    )]
    MissingPid { command: String },

    /// The OS reported the process still exists (fingerprint matched) but
    /// refused to signal it — e.g. a permissions error, or it exited in the
    /// instant between the liveness check and the kill attempt. Surfaced
    /// explicitly rather than assumed-dead, so crash recovery logs it
    /// instead of silently believing an orphan agent process was cleaned up.
    #[error("failed to terminate orphan process (pid {pid})")]
    KillFailed { pid: u32 },
}

/// Errors specific to the Evidence Capture Adapter (ADR-0009, issue #7).
/// Spawn/wait failures for the underlying `npx`/`asciinema` subprocess reuse
/// [`ProcessError`] (via `process::spawn_and_wait`) rather than duplicating
/// that handling here -- these variants only cover outcomes that are valid
/// from a subprocess point of view (it ran, it exited) but still mean no
/// usable evidence was produced.
#[derive(Debug, Error)]
pub enum EvidenceError {
    #[error("evidence tool `{tool}` exited with status {exit_code:?}: {stderr}")]
    CommandFailed {
        tool: &'static str,
        exit_code: Option<i32>,
        stderr: String,
    },

    #[error("evidence tool `{tool}` produced no artifacts in {path}")]
    NoArtifactsProduced { tool: &'static str, path: PathBuf },

    /// A `evidence.file_path` column value that isn't of the
    /// `.warden/evidence/<cycle>/<filename>` shape `evidence::repo_relative_path`
    /// always writes -- a row written by something other than this code, or a
    /// corrupted database (code-standards.md: "no silent fallback").
    #[error("stored evidence file_path {file_path:?} has no file name component")]
    InvalidStoredEvidencePath { file_path: String },
}

#[derive(Debug, Error)]
pub enum WardenError {
    #[error(transparent)]
    Worktree(#[from] WorktreeError),

    #[error(transparent)]
    Process(#[from] ProcessError),

    #[error(transparent)]
    Evidence(#[from] EvidenceError),

    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("migration error: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),

    #[error(transparent)]
    Core(#[from] warden_core::CoreError),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("run {run_id} exceeded its cycle budget ({max_cycles} cycles) without converging")]
    MaxCyclesExceeded { run_id: String, max_cycles: u32 },

    #[error("row column `{column}` = {value} does not fit in the expected numeric type")]
    InvalidStoredValue { column: &'static str, value: i64 },

    #[error("run {run_id} not found")]
    RunNotFound { run_id: String },

    #[error("coder for run {run_id} (cycle {cycle_id}) exited with status {exit_code}: {stderr}")]
    CoderFailed {
        run_id: String,
        cycle_id: String,
        exit_code: i32,
        stderr: String,
    },

    /// A pre-migration backup of the SQLite database file failed. Per
    /// code-standards.md ("no silent fallback"), this must abort the
    /// migration rather than proceed without a safety net.
    #[error("failed to back up database to {path} before applying migrations: {source}")]
    Backup {
        path: PathBuf,
        #[source]
        source: sqlx::Error,
    },

    /// The target repo's root `package.json` exists but isn't valid JSON --
    /// evidence project-type detection (ADR-0009) must not silently treat
    /// this as "no dependencies" (code-standards.md: "no silent fallback").
    #[error("malformed package.json at {path}: {source}")]
    InvalidPackageJson {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

pub type Result<T> = std::result::Result<T, WardenError>;
