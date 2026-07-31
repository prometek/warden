use super::*;

/// `OpenDraft` (ADR-0007): pushes only the branch skeleton to `origin`, then
/// opens a draft PR linked to the issue the intent references (or titled
/// from the intent otherwise). Triggered at coder start -- before any
/// business code exists, so this is the earliest point metadata is allowed
/// to reach `origin` under ADR-0002/0007.
///
/// Order matters here (issue #4 review, finding #2): all fallible *pure*
/// validation/generation (title, body) runs first, so a caller mistake
/// (e.g. a blank intent) surfaces before anything irreversible happens.
/// Only once that's settled does this reach for I/O -- first the
/// independent content-free re-verification (finding #1), then the push
/// itself, and only if both succeed does it ask the provider to open the PR
/// (which itself needs the branch already on `origin` to set `--head`).
///
/// Trigger wiring: how `warden` actually invokes this action (CLI
/// subcommand vs. some other channel) is a separate architectural decision
/// deferred out of this module's scope -- see issue #4.
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

// ---------------------------------------------------------------------------
// PostCycleUpdate
// ---------------------------------------------------------------------------

/// `PostCycleUpdate` (ADR-0007): posts one cycle's findings as a PR comment.
/// Purely informational -- only ever calls `PrProvider::post_comment`, never
/// touches the PR's draft status or body.
pub async fn post_cycle_update<P: PrProvider>(
    pr: &PrHandle,
    summary: &CycleSummary,
    provider: &P,
) -> Result<()> {
    let body = format_cycle_comment(summary);
    provider.post_comment(pr, &body).await
}

// ---------------------------------------------------------------------------
// Finalize
// ---------------------------------------------------------------------------

/// Outcome of a `Finalize` attempt -- mirrors [`GateDecision`] (the exact
/// same authorization result `finalize` re-derives), but named for this call
/// site so a blocked finalize reads as "blocked", not "should have pushed
/// but silently didn't".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinalizeOutcome {
    Finalized { commit_sha: String },
    Blocked(GateBlockReason),
}

/// Everything `finalize` needs beyond the read-only database pool (bundled
/// into a struct rather than passed as separate arguments -- mirrors
/// `OpenDraftRequest`, and keeps the call site readable).
pub struct FinalizeRequest<'a> {
    /// The local bare gate repo to push the final content from.
    pub bare_repo_path: &'a Path,
    pub branch: &'a str,
    pub run_id: &'a str,
    /// The commit that was actually written into the bare gate repo --
    /// checked against `runs.converged_commit_sha`, never trusted as-is.
    pub pushed_commit_sha: &'a str,
    pub pr: &'a PrHandle,
    /// The cycles/findings recap to write into the PR body -- composed by
    /// the caller (the orchestrator, which has access to the run's
    /// cycles/findings history, `warden::pr_summary::pr_body_from_run`);
    /// this module only knows how to post it, the same way
    /// `post_cycle_update` doesn't re-derive findings itself. Must **not**
    /// already contain an Evidence section -- `finalize` appends its own
    /// (see `evidence`/`repo_slug` below) so there's exactly one renderer
    /// for it, reachable from the one place that actually posts a PR body
    /// in production.
    ///
    /// TODO(#4): when the real orchestrator -> gate Finalize trigger is
    /// wired, whatever composes this value (today only
    /// `warden::pr_summary::pr_body_from_run` in tests) must stop rendering
    /// its own Evidence section (`evidence` parameter of
    /// `pr_body_from_run`) -- otherwise a run with captured evidence gets
    /// the section twice once `finalize_pr_body` appends its own below.
    pub summary_body: &'a str,
    /// Evidence captured across the run's cycles, already committed into
    /// the repo -- rendered as this PR's Evidence section (ADR-0009) by
    /// `finalize_pr_body`. Empty means "no Evidence section", exactly like
    /// `warden::pr_summary::pr_body_from_run`.
    ///
    /// TODO(#4): populate this from the gate's own read-only re-read of the
    /// `evidence` table (mirrors `gate::verify_and_authorize` re-deriving
    /// authorization from SQLite rather than trusting the push
    /// notification, code-standards.md "warden-gated ... revérifie l'état
    /// de manière indépendante") once the real trigger is wired -- not by
    /// trusting a list handed over the git-bare-remote/hook boundary.
    pub evidence: &'a [warden_core::EvidenceRow],
    /// `"<owner>/<repo>"` -- needed alongside `branch` to build the
    /// evidence section's `raw.githubusercontent.com` URLs (ADR-0009).
    pub repo_slug: &'a str,
}

/// Composes the PR body `finalize` actually writes: `summary_body` verbatim,
/// plus an Evidence section (ADR-0009) appended when `evidence` is
/// non-empty. This is the genuine, reachable call site for
/// `warden_core::format_evidence_section` in production -- `finalize`/
/// `update_body` are the only code that ever sets a PR body for real (see
/// module docs), so this is where the Evidence section must be assembled,
/// not in `warden` (which `warden-gated` can never depend on, ADR-0006).
pub(super) fn finalize_pr_body(request: &FinalizeRequest<'_>) -> String {
    if request.evidence.is_empty() {
        return request.summary_body.to_string();
    }
    let evidence_section =
        warden_core::format_evidence_section(request.evidence, request.repo_slug, request.branch);
    format!("{}\n\n{evidence_section}", request.summary_body)
}

/// `Finalize` (ADR-0007): re-verifies `state == Converged` and the committed
/// hash via the exact same path the git-push gate itself uses
/// (`gate::verify_and_authorize`, see `serve::handle_push_notification_line`
/// -- deliberately not reimplemented here), and only if authorized: pushes
/// the final content, updates the PR body with the full summary, and removes
/// draft status. Order matters -- the body is updated *before* the PR is
/// marked ready, so a reviewer can never see a "ready" PR with a stale body,
/// even momentarily.
///
/// Partial-failure note (issue #4 review, finding #3): if the push succeeds
/// but `update_body` or `mark_ready` then fails (e.g. a transient `gh`
/// error), the PR is left as a **draft carrying the previous or
/// partially-updated body** -- never "ready" with a stale body, since
/// `mark_ready` only runs after `update_body` completes. `push_to_origin`,
/// `gh pr edit`, and `gh pr ready` are all idempotent, so simply retrying
/// `finalize` with the same `FinalizeRequest` converges to the fully
/// finalized state without any special-cased recovery logic here.
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
