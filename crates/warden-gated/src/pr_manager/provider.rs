use super::*;

/// Provider-agnostic handle to an already-opened PR: everything `post_cycle_update`/`finalize` need
/// to address it again.
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

/// Thin seam over a PR provider's CLI.
#[allow(async_fn_in_trait)]
pub trait PrProvider {
    /// Opens a **draft** PR for `params.branch` against `params.base_branch`.
    async fn open_draft(&self, params: &OpenDraftParams<'_>) -> Result<PrHandle>;
    /// Posts an informational comment. Must never change draft status or body.
    async fn post_comment(&self, pr: &PrHandle, body: &str) -> Result<()>;
    async fn mark_ready(&self, pr: &PrHandle) -> Result<()>;
    /// Replaces the PR's body.
    async fn update_body(&self, pr: &PrHandle, body: &str) -> Result<()>;
}

/// Everything `open_draft` needs: the skeleton commit to push and the metadata to open the draft PR
/// from.
pub struct OpenDraftRequest<'a> {
    /// The local bare gate repo to push the skeleton branch from (the same repo
    /// `push::push_to_origin` always pushes from).
    pub bare_repo_path: &'a Path,
    /// The branch-skeleton commit -- **must** contain no business code.
    pub skeleton_commit_sha: &'a str,
    pub branch: &'a str,
    pub base_branch: &'a str,
    pub intent: &'a str,
}
