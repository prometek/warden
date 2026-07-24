//! Evidence capture around a cycle, and folding captured evidence into
//! the converged commit (ADR-0009). Both are best-effort: a failure here
//! must never abort an otherwise-successful/converging run.

use super::*;

impl Orchestrator {
    /// Best-effort evidence commit at convergence (ADR-0009 / code-review
    /// MEDIUM finding #1, issue #7): mirrors `capture_evidence_for_cycle`'s
    /// philosophy -- a git failure while folding captured evidence into the
    /// repo (disk full, permissions, an evidence worktree collision, ...)
    /// must not abort an otherwise-converged run. Falls back to
    /// `base_commit` (i.e. "converge without evidence attached") and logs
    /// loudly rather than swallowing the error silently (code-standards.md:
    /// "catch-and-ignore ... qui jette l'erreur sans la logger").
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

    /// Best-effort evidence capture (ADR-0009): logs and continues on
    /// failure rather than failing the run. A missing/misconfigured
    /// evidence tool (Playwright/asciinema not installed, no artifacts
    /// produced, ...) is an environment issue, not a defect in the code
    /// under test -- it must not abort an otherwise-converging run over a
    /// "nice to have" proof. Still logged loudly (`tracing::warn!` with the
    /// full error), never swallowed silently (code-standards.md:
    /// "catch-and-ignore ... qui jette l'erreur sans la logger").
    pub(super) async fn capture_evidence_for_cycle(&self, capture: EvidenceCapture<'_>) {
        // Copied out before `capture` is consumed below -- both are `&str`,
        // and the log line needs them on the failure path.
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

        // Code-review LOW finding (issue #7): when `evidence_store_in_repo`
        // is false, these `EVIDENCE.file_path` values name a
        // `.warden/evidence/<cycle>/...` repo path that never gets created
        // (`commit_evidence_into_repo` doesn't run -- see the convergence
        // branch above), so any future PR-body Evidence section built
        // straight off this table would need to skip rows it can't safely
        // link to. NOT changed here: `e2e_evidence_store_in_repo_false_...`
        // (crates/warden/tests/cli.rs) already asserts, as a deliberate
        // product decision, that evidence rows are recorded regardless of
        // `store_in_repo` ("still captured locally") -- only the git commit
        // is skipped. Suppressing the insert would contradict that existing,
        // intentional behaviour; reconciling the two is a product call, not
        // a mechanical fix, so left as-is pending that decision.
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

    /// Acceptance criterion 7 (issue #7, ADR-0009): "a missing/failing
    /// evidence tool is non-fatal -- a converging run still converges".
    /// Exercised directly against `Orchestrator::run_convergence_loop`
    /// (see `tests/cli.rs` for the same behaviour driven through the real
    /// `warden` binary): the tester's own project has no web markers, so
    /// asciinema is selected, and asciinema is genuinely not on `PATH` in
    /// this test environment -- the run must still converge, and no
    /// evidence row must have been recorded for it.
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

        // With no evidence captured, the converged commit is just the
        // coder's own commit -- no evidence-only commit is created on top.
        let run = db::get_run(&pool, &run_id).await.unwrap().unwrap();
        assert!(run.converged_commit_sha.is_some());
    }
}
