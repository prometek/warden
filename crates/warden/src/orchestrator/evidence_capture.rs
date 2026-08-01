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
            tester_command,
            tester_worktree_path,
            cancel,
        } = capture;

        let scratch_dir = config
            .warden_home
            .join("evidence")
            .join(run_id)
            .join(cycle_number.to_string());
        tokio::fs::create_dir_all(&scratch_dir).await?;

        let markers = evidence::scan_project_markers(tester_worktree_path).await?;
        let ctx = EvidenceCaptureContext {
            worktree_path: tester_worktree_path,
            scratch_dir: &scratch_dir,
            cycle_number,
            record_command: tester_command,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::test_support::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn evidence_capture_failure_does_not_prevent_convergence() {
        let repo = init_test_repo();
        let warden_home = TempDir::new().unwrap();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();

        let orchestrator = Orchestrator::new(pool.clone());
        let config = RunConfig {
                repo_path: repo.path().to_path_buf(),
                warden_home: warden_home.path().to_path_buf(),
                branch: "main".to_string(),
                intent: "converge even though no evidence tool is installed".to_string(),
                max_review_cycles: 3,
                max_test_cycles: 3,
                workflow: warden_core::Workflow::builtin_default(),
                max_extra_step_cycles: 5,
                                step_agents: vec![definition(AgentCommand::new(
                    "sh",
                    [
                        "-c",
                        "echo hi >> notes.txt && git add notes.txt && git -c user.email=t@w.local -c user.name=w commit -q -m cycle",
                    ],
                )), definition(AgentCommand::new("sh", ["-c", "true"])), definition(always_passing_tester())],
                evidence_tool: None,
                evidence_store_in_repo: true,
                gate: None,
                untrusted_repo_agent_definitions: Vec::new(),
            };

        let (run_id, final_state) = orchestrator
            .run_convergence_loop(config, FakeCommandAdapter, CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(
            final_state,
            RunState::Converged,
            "a missing evidence tool must not fail an otherwise-converging run"
        );

        let evidence = db::list_evidence_for_run(&pool, &run_id).await.unwrap();
        assert!(
            evidence.is_empty(),
            "no evidence row should be recorded when the capture tool is unavailable"
        );

        let run = db::get_run(&pool, &run_id).await.unwrap().unwrap();
        assert!(run.converged_commit_sha.is_some());
    }
}
