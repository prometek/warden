//! Error types for the `warden-gated` binary/library.

use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum GatedError {
    /// `warden-gated` must never create the database itself -- only
    /// `warden` does, via its migrations (ADR-0006: the gate is a read-only
    /// consumer). A missing file here means a misconfigured path or a
    /// `warden` that has never run, not something to paper over.
    #[error("database not found at {0} -- warden-gated never creates it, only warden does")]
    DatabaseNotFound(PathBuf),

    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error(transparent)]
    Core(#[from] warden_core::CoreError),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// A `post-receive` payload that isn't exactly `<old-sha> <new-sha>
    /// <ref-name>`, or whose ref doesn't match the gate's naming convention
    /// -- untrusted input at the process boundary, never silently dropped.
    #[error("malformed post-receive notification line: {0:?}")]
    MalformedPushNotification(String),

    #[error("git push to origin failed (exit {exit_code:?}): {stderr}\ncommand: {command}")]
    PushFailed {
        command: String,
        exit_code: Option<i32>,
        stderr: String,
    },

    #[error("git command `{command}` failed (exit {exit_code:?}): {stderr}")]
    GitCommandFailed {
        command: String,
        exit_code: Option<i32>,
        stderr: String,
    },

    /// A run's intent has no non-blank content to derive a PR title from --
    /// the orchestrator should never hand the gate a blank intent; surfaced
    /// as a typed error rather than inventing a placeholder title
    /// (code-standards.md: no silent fallback).
    #[error("cannot generate a PR title: intent is blank")]
    EmptyIntent,

    #[error("gh command `{command}` failed (exit {exit_code:?}): {stderr}")]
    GhCommandFailed {
        command: String,
        exit_code: Option<i32>,
        stderr: String,
    },

    /// The bare gate repo's `origin` remote isn't a recognizable GitHub URL
    /// -- most likely a misconfigured `origin`, or a non-GitHub remote with
    /// no explicit repo slug override supplied.
    #[error("could not determine a GitHub owner/repo from origin remote url {0:?}")]
    UnknownOriginRemote(String),

    /// `gh pr create`'s stdout didn't look like a PR URL ending in
    /// `/pull/<number>` -- an unexpected `gh` output format, surfaced
    /// rather than silently treated as "no PR number".
    #[error("could not parse a PR number from gh's output: {0:?}")]
    UnparsablePrUrl(String),

    /// `OpenDraft`'s independent content-free check (issue #4 review,
    /// finding #1): the caller-supplied skeleton commit changes files
    /// relative to `base_branch` (or the empty tree, if `base_branch`
    /// doesn't exist on `origin` yet) -- refused rather than trusted, the
    /// same way `gate::verify_and_authorize` never trusts a caller's claim
    /// of convergence.
    #[error(
        "refusing to push skeleton commit {commit_sha} for base branch {base_branch:?}: it \
         changes {files:?} relative to base -- OpenDraft must never push business code"
    )]
    SkeletonNotContentFree {
        commit_sha: String,
        base_branch: String,
        files: Vec<String>,
    },

    /// `gh pr view --json state,statusCheckRollup`'s stdout didn't
    /// deserialize into the shape the CI watcher (issue #5) expects --
    /// an unexpected `gh` output format, surfaced rather than silently
    /// treated as "no status yet".
    #[error("could not parse gh pr view output as PR status ({reason}): {json:?}")]
    UnparsablePrStatusJson { json: String, reason: String },

    /// `gh`'s `state` field for a PR wasn't one of `OPEN`/`CLOSED`/`MERGED`
    /// -- a closed set per GitHub's own API, so anything else is a boundary
    /// error, not a guess.
    #[error("unknown PR lifecycle state from gh: {0:?}")]
    UnknownPrLifecycle(String),

    /// A `statusCheckRollup` entry's `status`/`conclusion` (Checks API) or
    /// `state` (legacy Statuses API) combination wasn't one this module
    /// recognizes.
    #[error("unknown CI check status/conclusion from gh: {0}")]
    UnknownCheckConclusion(String),

    /// A `statusCheckRollup` entry had neither the Checks API shape
    /// (`status`/`conclusion`) nor the legacy Statuses API shape (`state`) --
    /// unrecognized as either, so its outcome can't be classified.
    #[error("CI check {0:?} has neither a Checks API nor a Statuses API shape")]
    MalformedCheckEntry(String),

    /// Delivering the terminal CI result (issue #15/ADR-0011) to `warden`'s
    /// reverse socket failed -- surfaced loudly rather than swallowed, per
    /// ADR-0011's "channel failure semantics": exactly like the forward
    /// relay already surfaces an undelivered push notification in the
    /// hook's exit code.
    #[error("failed to deliver CI result to warden's socket at {socket_path}: {source}")]
    CiResultDeliveryFailed {
        socket_path: PathBuf,
        #[source]
        source: Box<GatedError>,
    },

    /// `pr_manager::finalize`'s own independent re-verification refused the
    /// push (state drifted, hash mismatch, ...) -- issue #15's `run-tail`
    /// composition surfaces this as part of the run's terminal
    /// `CiWatchOutcome::GateFailed` rather than leaving the caller to
    /// distinguish a `Blocked` outcome from every other kind of failure.
    #[error("Finalize was blocked by re-verification: {reason}")]
    FinalizeBlocked { reason: String },

    /// A `runs.pr_number` value that doesn't fit in a `u64` -- a row written
    /// by something other than `warden::db::set_run_pr_number`, or a
    /// corrupted database (code-standards.md: "no silent fallback").
    #[error("row column `{column}` = {value} does not fit in the expected numeric type")]
    InvalidStoredValue { column: &'static str, value: i64 },
}

pub type Result<T> = std::result::Result<T, GatedError>;
