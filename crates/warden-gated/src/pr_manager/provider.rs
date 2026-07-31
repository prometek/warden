use super::*;

// ---------------------------------------------------------------------------
// Provider seam
// ---------------------------------------------------------------------------

/// Provider-agnostic handle to an already-opened PR: everything
/// `post_cycle_update`/`finalize` need to address it again. `GhProvider`
/// scopes itself to one `owner/repo` at construction, so this only carries
/// the provider-native PR number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrHandle {
    pub number: u64,
}

/// Everything `PrProvider::open_draft` needs to open the draft PR.
pub struct OpenDraftParams<'a> {
    pub branch: &'a str,
    pub base_branch: &'a str,
    pub title: &'a str,
    pub body: &'a str,
}

/// Thin seam over a PR provider's CLI. GitHub ships first via
/// `gh_provider::GhProvider` (Architecture.md's roadmap: "Provider CI/PR
/// prioritaire"). A `glab`-backed implementation is a drop-in second impl of
/// this same trait; nothing in this module's orchestration functions is
/// GitHub-specific.
///
/// `async fn` in this trait is intentional, not an oversight: every call
/// site (`open_draft`/`post_cycle_update`/`finalize`) awaits a `PrProvider`
/// directly on its own task rather than boxing it into a `dyn` trait object
/// or handing it to `tokio::spawn`, so the `Send`-bound future the compiler
/// would otherwise require is unnecessary here.
#[allow(async_fn_in_trait)]
pub trait PrProvider {
    /// Opens a **draft** PR for `params.branch` against `params.base_branch`.
    async fn open_draft(&self, params: &OpenDraftParams<'_>) -> Result<PrHandle>;
    /// Posts an informational comment. Must never change draft status or body.
    async fn post_comment(&self, pr: &PrHandle, body: &str) -> Result<()>;
    /// Flips a PR from draft to ready for review.
    async fn mark_ready(&self, pr: &PrHandle) -> Result<()>;
    /// Replaces the PR's body.
    async fn update_body(&self, pr: &PrHandle, body: &str) -> Result<()>;
}

// ---------------------------------------------------------------------------
// OpenDraft
// ---------------------------------------------------------------------------

/// Everything `open_draft` needs: the skeleton commit to push and the
/// metadata to open the draft PR from.
pub struct OpenDraftRequest<'a> {
    /// The local bare gate repo to push the skeleton branch from (the same
    /// repo `push::push_to_origin` always pushes from).
    pub bare_repo_path: &'a Path,
    /// The branch-skeleton commit -- **must** contain no business code.
    /// `open_draft` never takes the caller's word for that: it independently
    /// re-derives it (see `skeleton_is_content_free`) before ever pushing,
    /// the same "never trust the caller" principle `gate::verify_and_authorize`
    /// applies to convergence (ADR-0002/0006).
    pub skeleton_commit_sha: &'a str,
    pub branch: &'a str,
    pub base_branch: &'a str,
    pub intent: &'a str,
}
