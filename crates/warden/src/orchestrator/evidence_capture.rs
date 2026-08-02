//! Evidence capture around a cycle, and folding captured evidence into the converged commit.

use super::*;

impl Orchestrator {
    pub(super) async fn commit_evidence_for_convergence(
        &self,
        worktree_manager: &WorktreeManager,
        config: &RunConfig,
        run_id: &str,
        base_commit: &str,
        evidence: &[db::EvidenceWithCycle],
    ) -> String {
        match evidence::commit_evidence_into_repo(
            worktree_manager,
            &config.repo_path,
            &config.warden_home,
            run_id,
            base_commit,
            evidence,
        )
        .await
        {
            Ok(converged_commit) => converged_commit,
            Err(error) => {
                tracing::warn!(
                    %error,
                    run_id,
                    "failed to commit captured evidence into the repo; converging without evidence attached"
                );
                base_commit.to_string()
            }
        }
    }

    /// Best-effort evidence capture: logs and continues on failure rather than failing the run.
    pub(super) async fn capture_evidence_for_cycle(&self, capture: EvidenceCapture<'_>) {
        let (run_id, cycle_id) = (capture.run_id, capture.cycle_id);
        if let Err(error) = self.try_capture_evidence_for_cycle(capture).await {
            tracing::warn!(
                %error,
                run_id,
                cycle_id,
                "evidence capture failed; continuing without evidence for this cycle"
            );
        }
    }

    async fn try_capture_evidence_for_cycle(&self, capture: EvidenceCapture<'_>) -> Result<()> {
        let EvidenceCapture {
            run_id,
            cycle_id,
            cycle_number,
            config,
            command,
            worktree_path,
            cancel,
        } = capture;

        let scratch_dir = config
            .warden_home
            .join("evidence")
            .join(run_id)
            .join(cycle_number.to_string());
        tokio::fs::create_dir_all(&scratch_dir).await?;

        let markers = evidence::scan_project_markers(worktree_path).await?;
        let ctx = EvidenceCaptureContext {
            worktree_path,
            scratch_dir: &scratch_dir,
            cycle_number,
            record_command: command,
            cancel,
        };
        let captured = evidence::capture_evidence(&markers, config.evidence_tool, &ctx).await?;

        for item in captured {
            db::insert_evidence(
                &self.pool,
                &Uuid::new_v4().to_string(),
                cycle_id,
                None,
                item.evidence_type,
                &item.repo_relative_path,
                &item.description,
            )
            .await?;
        }
        Ok(())
    }
}
