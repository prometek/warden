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

    #[error(
        "child process for `{command}` has no PID (already reaped before it could be observed)"
    )]
    MissingPid { command: String },

    #[error("failed to terminate orphan process (pid {pid})")]
    KillFailed { pid: u32 },

    #[error("failed to write payload to `{command}` stdin: {source}")]
    StdinWrite {
        command: String,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "refusing to spawn {role} agent program {program:?}: {reason} -- this would let the \
         repository changes control what an independent agent executes"
    )]
    UntrustedAgentProgram {
        role: String,
        program: String,
        reason: String,
    },

    #[error(
        "refusing to spawn {role} agent with argument {arg:?}: {reason} -- this would let the \
         repository changes control what an independent agent executes"
    )]
    UntrustedAgentArg {
        role: String,
        arg: String,
        reason: String,
    },
}

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

    #[error("stored evidence file_path {file_path:?} has no file name component")]
    InvalidStoredEvidencePath { file_path: String },

    #[error("cannot shell-quote the recorded command for asciinema (part {part:?}): {source}")]
    UnshellableRecordCommand {
        part: String,
        #[source]
        source: shlex::QuoteError,
    },
}

#[derive(Debug, Error)]
pub enum AgentDefinitionError {
    #[error("failed to read agent definition {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The file was read but isn't a valid definition (no frontmatter fence, malformed YAML, an
    /// unknown key, a blank-but-present optional field, a blank system prompt,...).
    #[error("invalid agent definition {path}: {source}")]
    Invalid {
        path: PathBuf,
        #[source]
        source: warden_core::CoreError,
    },

    #[error(
        "cannot resolve the user config directory for agent definitions: {reason} (checked \
         XDG_CONFIG_HOME, then HOME)"
    )]
    UserConfigDirUnresolvable { reason: String },

    #[error(
        "cannot verify agent definition source {path} is outside the repo under review: {source}"
    )]
    PathResolutionFailed {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// A workflow step names an agent with no matching `.warden/agents/<agent>.md` file.
    #[error(
        "no agent definition found for custom workflow role {role:?}: expected {expected_path}"
    )]
    CustomStepAgentNotFound {
        role: String,
        expected_path: PathBuf,
    },
}

#[derive(Debug, Error)]
pub enum WardenError {
    #[error(transparent)]
    Worktree(#[from] WorktreeError),

    #[error(transparent)]
    AgentDefinition(#[from] AgentDefinitionError),

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

    /// An agent invocation was stopped by its CLI quota.
    #[error("agent invocation suspended until quota resets at {resets_at}")]
    QuotaSuspended { resets_at: i64 },

    /// A durable issue-#86 continuation could not be encoded.
    #[error("failed to encode quota continuation checkpoint: {source}")]
    QuotaContinuationEncode {
        #[source]
        source: serde_json::Error,
    },

    /// A checkpoint row is persisted input and is validated at the database boundary before it can
    /// reconstruct workflow state.
    #[error("failed to decode quota continuation checkpoint for run {run_id}: {source}")]
    QuotaContinuationDecode {
        run_id: String,
        #[source]
        source: serde_json::Error,
    },

    #[error("invalid quota continuation checkpoint for run {run_id}: {reason}")]
    InvalidQuotaContinuation { run_id: String, reason: String },

    #[error("cannot claim quota continuation: Warden process {pid} has no start-time fingerprint")]
    MissingQuotaResumeLeaseFingerprint { pid: u32 },

    #[error(
        "cannot suspend run {run_id} for durable quota resumption: the original tool and \
         execution/security context were not configured"
    )]
    MissingQuotaExecutionContext { run_id: String },

    #[error(
        "cannot checkpoint non-UTF-8 path in `{field}` ({path}); exact path bytes are required \
         for deterministic quota resumption"
    )]
    NonUtf8QuotaContinuationPath { field: &'static str, path: PathBuf },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// The repo's `.warden/hooks.toml` could not be read as a hook config -- malformed TOML, or an
    /// entry naming a `point` that is not a known [`warden_core::HookPoint`].
    #[error("invalid hook config {path}: {reason}")]
    HookConfig { path: PathBuf, reason: String },

    /// The repo's `.warden/policy.yaml` could not be read as a policy rule set -- malformed YAML,
    /// an unknown top-level key, or a rule naming an unknown `action`.
    #[error("invalid policy config {path}: {reason}")]
    PolicyConfig { path: PathBuf, reason: String },

    #[error("row column `{column}` = {value} does not fit in the expected numeric type")]
    InvalidStoredValue { column: &'static str, value: i64 },

    #[error("run {run_id} not found")]
    RunNotFound { run_id: String },

    /// A pre-migration backup of the SQLite database file failed.
    #[error("failed to back up database to {path} before applying migrations: {source}")]
    Backup {
        path: PathBuf,
        #[source]
        source: sqlx::Error,
    },

    /// `RunEvent` serialization to `events.payload_json` failed on `insert_event` (encode
    /// direction).
    #[error("event payload serialization failed: {0}")]
    EventPayload(#[from] serde_json::Error),

    /// [`crate::orchestrator::Orchestrator::run_convergence_loop`] sets up its Event Bus / run
    /// context exactly once per instance.
    #[error("this orchestrator instance already has an active run in progress")]
    RunAlreadyInProgress,
    /// The target repo's root `package.json` exists but isn't valid JSON.
    #[error("malformed package.json at {path}: {source}")]
    InvalidPackageJson {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("timed out after {timeout_secs}s waiting for a CI result on run {run_id}")]
    CiResultTimedOut { run_id: String, timeout_secs: u64 },

    #[error("warden-gated for run {run_id} exited without delivering a CI result")]
    GateChildDiedWithoutResult { run_id: String },

    #[error("CI result payload exceeded the {max_bytes}-byte cap")]
    CiResultPayloadTooLarge { max_bytes: usize },

    #[error("CI result message run_id {actual:?} does not match the expected run {expected:?}")]
    CiResultRunIdMismatch { expected: String, actual: String },

    #[error("PR number {pr_number} does not fit in the column's numeric type")]
    PrNumberOverflow { pr_number: u64 },

    #[error(transparent)]
    Sandbox(#[from] warden_sandbox::SandboxError),

    /// a `warden_core::TokenUsage` field too large for `i64` (SQLite's native integer type) to hold
    /// -- surfaces the real `u64` value that failed to convert, not a placeholder.
    #[error("token count {value} for column `{column}` does not fit in the column's numeric type")]
    TokenCountOverflow { column: &'static str, value: u64 },

    #[error("run {run_id} has a partially-populated rate_limit_* row (expected all six columns NULL or all six present)")]
    CorruptRateLimitStatusRow { run_id: String },

    #[error(
        "workflow declares {agent_steps} type: agent step(s) but {step_agents} agent \
         definition(s) were resolved for it -- every type: agent step must have exactly one \
         resolved agent definition"
    )]
    MismatchedStepAgentCount {
        agent_steps: usize,
        step_agents: usize,
    },
}

pub type Result<T> = std::result::Result<T, WardenError>;
