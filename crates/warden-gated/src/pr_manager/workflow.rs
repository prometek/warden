use super::*;

/// `OpenDraft`: pushes only the branch skeleton to `origin`, then opens a draft PR linked to the
/// issue the intent references (or titled from the intent otherwise).
pub async fn open_draft<P: PrProvider>(
    request: &OpenDraftRequest<'_>,
    provider: &P,
) -> Result<PrHandle> {
    let linked_issue = detect_linked_issue(request.intent);
    let title = generate_pr_title(request.intent)?;
    let body = open_draft_pr_body(request.intent, linked_issue.as_ref());

    let offending_files = skeleton_diff_against_base(
        request.bare_repo_path,
        request.base_branch,
        request.skeleton_commit_sha,
    )
    .await?;
    if !offending_files.is_empty() {
        return Err(GatedError::SkeletonNotContentFree {
            commit_sha: request.skeleton_commit_sha.to_string(),
            base_branch: request.base_branch.to_string(),
            files: offending_files,
        });
    }

    push::push_to_origin(
        request.bare_repo_path,
        request.skeleton_commit_sha,
        request.branch,
    )
    .await?;

    provider
        .open_draft(&OpenDraftParams {
            branch: request.branch,
            base_branch: request.base_branch,
            title: &title,
            body: &body,
        })
        .await
}

/// `PostCycleUpdate`: posts one cycle's findings as a PR comment.
pub async fn post_cycle_update<P: PrProvider>(
    pr: &PrHandle,
    summary: &CycleSummary,
    provider: &P,
) -> Result<()> {
    let body = format_cycle_comment(summary);
    provider.post_comment(pr, &body).await
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinalizeOutcome {
    Finalized { commit_sha: String },
    Blocked(GateBlockReason),
}

/// Everything `finalize` needs beyond the read-only database pool (bundled into a struct rather
/// than passed as separate arguments.
pub struct FinalizeRequest<'a> {
    /// The local bare gate repo to push the final content from.
    pub bare_repo_path: &'a Path,
    pub branch: &'a str,
    pub run_id: &'a str,
    /// The commit that was actually written into the bare gate repo -- checked against
    /// `runs.converged_commit_sha`, never trusted as-is.
    pub pushed_commit_sha: &'a str,
    pub pr: &'a PrHandle,
    pub summary_body: &'a str,
    /// Evidence captured across the run's cycles, already committed into the repo -- rendered as
    /// this PR's Evidence section by `finalize_pr_body`.
    pub evidence: &'a [warden_core::EvidenceRow],
    /// `"<owner>/<repo>"` -- needed alongside `branch` to build the evidence section's
    /// `raw.githubusercontent.com` URLs.
    pub repo_slug: &'a str,
}

/// Composes the PR body `finalize` actually writes: `summary_body` verbatim, plus an Evidence
/// section appended when `evidence` is non-empty.
pub(super) fn finalize_pr_body(request: &FinalizeRequest<'_>) -> String {
    if request.evidence.is_empty() {
        return request.summary_body.to_string();
    }
    let evidence_section =
        warden_core::format_evidence_section(request.evidence, request.repo_slug, request.branch);
    format!("{}\n\n{evidence_section}", request.summary_body)
}

pub async fn finalize<P: PrProvider>(
    pool: &SqlitePool,
    request: &FinalizeRequest<'_>,
    provider: &P,
) -> Result<FinalizeOutcome> {
    let decision = verify_and_authorize(pool, request.run_id, request.pushed_commit_sha).await?;

    match decision {
        GateDecision::Blocked(reason) => Ok(FinalizeOutcome::Blocked(reason)),
        GateDecision::Allow { commit_sha } => {
            push::push_to_origin(request.bare_repo_path, &commit_sha, request.branch).await?;
            let body = finalize_pr_body(request);
            provider.update_body(request.pr, &body).await?;
            provider.mark_ready(request.pr).await?;
            Ok(FinalizeOutcome::Finalized { commit_sha })
        }
    }
}
