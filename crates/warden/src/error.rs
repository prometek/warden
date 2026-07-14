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

    /// `RunEvent` (de)serialization to/from `events.payload_json` failed --
    /// covers both directions (encode on `insert_event`, decode on
    /// `list_events_for_run`), since only one `#[from] serde_json::Error`
    /// variant can exist per enum.
    #[error("event payload (de)serialization failed: {0}")]
    EventPayload(#[from] serde_json::Error),

    /// An `events` row's `event_type` column disagrees with the
    /// discriminant carried by its own `payload_json` (see
    /// `warden_core::RunEvent::kind`) -- a corrupted row, or one written by
    /// something other than `db::insert_event`, never silently trusted
    /// (code-standards.md: "toute ligne relue est reparsée en type Rust
    /// fort").
    #[error(
        "event {id} has event_type {event_type:?} but its payload's own kind is {payload_kind:?}"
    )]
    EventKindMismatch {
        id: String,
        event_type: String,
        payload_kind: &'static str,
    },

    /// [`crate::orchestrator::Orchestrator::run_convergence_loop`] sets up
    /// its Event Bus / run context exactly once per instance -- an
    /// orchestrator is one-run-per-instance in this codebase (a fresh one is
    /// constructed per CLI invocation). A second call on the same instance
    /// is a programming error, not a runtime condition to paper over.
    #[error("this orchestrator instance already has an active run in progress")]
    RunAlreadyInProgress,
    /// The target repo's root `package.json` exists but isn't valid JSON --
    /// evidence project-type detection (ADR-0009) must not silently treat
    /// this as "no dependencies" (code-standards.md: "no silent fallback").
    #[error("malformed package.json at {path}: {source}")]
    InvalidPackageJson {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    /// Issue #15 review, H1(b): `CiResultListener::receive` waited longer
    /// than `timeout` for `warden-gated` to deliver a run's terminal CI
    /// result. Never an infinite await -- the caller maps this to failing
    /// the run outright rather than leaving it stuck in `AwaitingCi`.
    #[error("timed out after {timeout_secs}s waiting for a CI result on run {run_id}")]
    CiResultTimedOut { run_id: String, timeout_secs: u64 },

    /// Issue #15 review, M-new-1: the triggered `warden-gated` subprocess for
    /// this run exited without ever delivering a terminal CI result (a hard
    /// crash/kill before it could send even a `GateFailed`). The caller maps
    /// this to failing the run outright -- unlike a still-live child, whose
    /// wait is bounded only by its own watch inactivity timeout, a dead child
    /// will never deliver, so waiting further would hang the run forever.
    #[error("warden-gated for run {run_id} exited without delivering a CI result")]
    GateChildDiedWithoutResult { run_id: String },

    /// Issue #15 review, L1: a reverse-channel payload exceeded the size
    /// cap before `warden-gated` ever closed its write half -- refused
    /// outright rather than buffered without bound (OOM risk).
    #[error("CI result payload exceeded the {max_bytes}-byte cap")]
    CiResultPayloadTooLarge { max_bytes: usize },

    /// Issue #15 review, M5: a `CiResultMessage` delivered over a run's
    /// *own* reverse socket named a different `run_id` than the run that
    /// socket was bound for -- untrusted input at a process boundary, never
    /// applied to the wrong run.
    #[error("CI result message run_id {actual:?} does not match the expected run {expected:?}")]
    CiResultRunIdMismatch { expected: String, actual: String },

    /// Issue #15 review, L2: a `runs.pr_number` value too large for `i64`
    /// (SQLite's native integer type) to hold -- surfaces the real `u64`
    /// value that failed to convert, not a placeholder.
    #[error("PR number {pr_number} does not fit in the column's numeric type")]
    PrNumberOverflow { pr_number: u64 },
}

pub type Result<T> = std::result::Result<T, WardenError>;
