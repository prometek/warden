use super::super::diff::read_head_commit;
use super::super::tampering::{AgentDefinitionSnapshot, SNAPSHOT_WORKTREE_ROLE};
use super::super::*;
use super::select_prior_findings;

impl Orchestrator {
    pub async fn run_convergence_loop<R: ToolAdapter>(
        &self,
        config: RunConfig,
        runner: R,
        cancel: CancellationToken,
    ) -> Result<(String, RunState)> {
        self.run_convergence_continuation(config, &runner, cancel, None)
            .await
    }

    pub(in crate::orchestrator) async fn resume_convergence_loop<R: ToolAdapter>(
        &self,
        run_id: String,
        config: RunConfig,
        runner: &R,
        cancel: CancellationToken,
        continuation: ConvergenceContinuation,
    ) -> Result<(String, RunState)> {
        self.run_convergence_continuation(config, runner, cancel, Some((run_id, continuation)))
            .await
    }

    async fn run_convergence_continuation<R: ToolAdapter>(
        &self,
        config: RunConfig,
        runner: &R,
        cancel: CancellationToken,
        restored: Option<(String, ConvergenceContinuation)>,
    ) -> Result<(String, RunState)> {
        let run_id = restored
            .as_ref()
            .map(|(run_id, _)| run_id.clone())
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let agents = ResolvedAgents::resolve(runner, &config)?;
        let worktree_manager =
            WorktreeManager::new(&config.repo_path, config.warden_home.join("worktrees"))?;
        let event_bus = EventBus::bind(&run_id, &config.warden_home.join("runs")).await?;
        self.run_context
            .set(RunContext {
                run_id: run_id.clone(),
                event_bus,
            })
            .map_err(|_| WardenError::RunAlreadyInProgress)?;

        if restored.is_none() {
            db::insert_run(
                &self.pool,
                &run_id,
                &config.repo_path.display().to_string(),
                &config.branch,
                &config.intent,
                config.max_cycles,
                config.max_cycles,
                config.workflow.steps.len() as u32,
                config.max_cycles,
            )
            .await?;
            db::set_run_workflow_entry(&self.pool, &run_id, config.workflow.entry()).await?;
            self.publish_event(RunEvent::RunStarted {
                intent: config.intent.clone(),
                branch: config.branch.clone(),
                max_cycles: config.max_cycles,
            })
            .await?;
            if let Some(callback) = &self.on_run_started {
                callback(&run_id);
            }
            match self
                .dispatch_run_hooks(
                    &run_id,
                    &config.repo_path,
                    RunState::Pending,
                    HookPoint::OnRunStart,
                )
                .await?
            {
                HookOutcome::Continue => {}
                HookOutcome::Block { reason } => {
                    self.fail_run_on_block(&run_id, HookPoint::OnRunStart, &reason)
                        .await?;
                    self.run_teardown(&run_id, &config.repo_path, RunState::Failed)
                        .await;
                    return Ok((run_id, RunState::Failed));
                }
                HookOutcome::EmitFindings(findings) => {
                    log_unrouted_findings(&run_id, HookPoint::OnRunStart, &findings)
                }
            }
            if let Some(final_state) = self
                .transition_or_fail_run(
                    &run_id,
                    RunState::RunningStep(config.workflow.entry()),
                    &config.repo_path,
                )
                .await?
            {
                return Ok((run_id, final_state));
            }
        }

        let run_base_commit_sha = match &restored {
            Some((_, continuation)) => continuation.run_base_commit_sha.clone(),
            None => read_head_commit(&config.repo_path).await?,
        };
        let mut agent_names = config
            .workflow
            .steps
            .iter()
            .filter_map(|step| step.agent.clone())
            .collect::<Vec<_>>();
        agent_names.sort();
        agent_names.dedup();
        let definition_snapshot = if config.repository_agent_definitions {
            Some(
                AgentDefinitionSnapshot::capture(
                    &worktree_manager,
                    &run_id,
                    SNAPSHOT_WORKTREE_ROLE,
                    &run_base_commit_sha,
                    &agent_names,
                )
                .await?,
            )
        } else {
            None
        };

        if let Some((_, continuation)) = &restored {
            db::clear_run_rate_limit_status(&self.pool, &run_id).await?;
            if let Some(final_state) = self
                .transition_or_fail_run(&run_id, continuation.next_run_state(), &config.repo_path)
                .await?
            {
                return Ok((run_id, final_state));
            }
        }
        let mut continuation = restored
            .map(|(_, continuation)| continuation)
            .unwrap_or_else(|| ConvergenceContinuation::new(run_base_commit_sha, &config.workflow));

        let final_state = loop {
            let step_index = continuation.next_step_index;
            let step = &config.workflow.steps[step_index as usize];
            if self.suspend_for_anticipated_quota(&run_id).await? {
                let resets_at = db::get_run_rate_limit_status(&self.pool, &run_id)
                    .await?
                    .expect("quota suspension requires stored status")
                    .resets_at;
                break self
                    .persist_quota_suspension(&run_id, &config, &continuation, resets_at)
                    .await?;
            }

            let cycle_id = Uuid::new_v4().to_string();
            db::insert_cycle(&self.pool, &cycle_id, &run_id, continuation.cycle_number).await?;
            self.publish_event(RunEvent::CycleStarted {
                cycle_number: continuation.cycle_number,
            })
            .await?;
            let seeded = std::mem::take(&mut continuation.pending_ci_findings);
            let prior_findings = select_prior_findings(
                &self.pool,
                seeded,
                continuation.previous_cycle_id.as_deref(),
            )
            .await?;

            let invocation = StepInvocation {
                run_id: &run_id,
                cycle_id: &cycle_id,
                cycle_number: continuation.cycle_number,
                step_index,
                config: &config,
                role: &step.role,
                kind: step.kind,
                agent: agents.steps[step_index as usize].as_ref(),
                run: step.run.as_deref(),
                env_allowlist: agents.env_allowlist,
                worktree_manager: &worktree_manager,
                commit: &continuation.base_commit,
                run_base_commit: &continuation.run_base_commit_sha,
                run_agent_definition_snapshot: definition_snapshot.as_ref(),
                prior_findings: &prior_findings,
                cancel: cancel.clone(),
            };
            let result = match self.run_step(runner, invocation).await {
                Ok(result) => result,
                Err(WardenError::QuotaSuspended { resets_at }) => {
                    break self
                        .persist_quota_suspension(&run_id, &config, &continuation, resets_at)
                        .await?;
                }
                Err(error) => {
                    tracing::warn!(step = %step.role, %error, "workflow step infrastructure error");
                    StepResult {
                        commit: continuation.base_commit.clone(),
                        findings: vec![Finding {
                            source: warden_core::FindingSource::Warden,
                            severity: warden_core::Severity::Blocking,
                            file: None,
                            description: error.to_string(),
                            action: None,
                        }],
                        outcome: warden_core::StepOutcome::Error,
                    }
                }
            };
            let commit_changed = continuation.base_commit != result.commit;
            continuation.base_commit = result.commit;
            for finding in &result.findings {
                db::insert_finding(&self.pool, &Uuid::new_v4().to_string(), &cycle_id, finding)
                    .await?;
                self.publish_event(RunEvent::FindingRaised {
                    cycle_number: continuation.cycle_number,
                    source: finding.source.as_str().to_string(),
                    severity: finding.severity.as_str().to_string(),
                    file: finding.file.clone(),
                    description: finding.description.clone(),
                    action: finding.action.clone(),
                })
                .await?;
            }
            continuation.step_cycle_numbers[step_index as usize] += 1;
            let mut next_state = match result.outcome {
                warden_core::StepOutcome::Error => warden_core::state_for_target(
                    config
                        .workflow
                        .target_for(step_index, warden_core::StepOutcome::Error),
                ),
                _ => decide_next_state_for_step(
                    &result.findings,
                    &config.workflow,
                    step_index,
                    continuation.step_cycle_numbers[step_index as usize],
                    config.max_cycles,
                ),
            };
            db::close_cycle(&self.pool, &cycle_id).await?;
            let mut all_findings = result.findings.clone();
            let mut blocked = false;
            for point in [HookPoint::AfterStep]
                .into_iter()
                .chain(commit_changed.then_some(HookPoint::OnCommit))
            {
                match self
                    .dispatch_run_hooks(
                        &run_id,
                        &config.repo_path,
                        RunState::RunningStep(step_index),
                        point,
                    )
                    .await?
                {
                    HookOutcome::Continue => {}
                    HookOutcome::Block { reason } => {
                        tracing::warn!(%reason, point = point.as_str(), "hook blocked workflow");
                        next_state = RunState::Failed;
                        blocked = true;
                        break;
                    }
                    HookOutcome::EmitFindings(findings) => {
                        for finding in &findings {
                            db::insert_finding(
                                &self.pool,
                                &Uuid::new_v4().to_string(),
                                &cycle_id,
                                finding,
                            )
                            .await?;
                            self.publish_event(RunEvent::FindingRaised {
                                cycle_number: continuation.cycle_number,
                                source: finding.source.as_str().to_string(),
                                severity: finding.severity.as_str().to_string(),
                                file: finding.file.clone(),
                                description: finding.description.clone(),
                                action: finding.action.clone(),
                            })
                            .await?;
                        }
                        all_findings.extend(findings);
                    }
                }
            }
            // A hook's findings aggregate exactly like the step's own -- reboucle via the same
            // step's `on_blocking` edge -- unless the step itself already errored (its `on_error`
            // target stands) or a hook already blocked outright above.
            if !blocked
                && !matches!(result.outcome, warden_core::StepOutcome::Error)
                && all_findings.len() != result.findings.len()
            {
                next_state = decide_next_state_for_step(
                    &all_findings,
                    &config.workflow,
                    step_index,
                    continuation.step_cycle_numbers[step_index as usize],
                    config.max_cycles,
                );
            }
            continuation.previous_cycle_id = Some(cycle_id);
            continuation.cycle_number += 1;

            let mut converged_commit = None;
            if next_state == RunState::Converged {
                let commit = if config.evidence_store_in_repo {
                    let evidence = db::list_evidence_for_run(&self.pool, &run_id).await?;
                    self.commit_evidence_for_convergence(
                        &worktree_manager,
                        &config,
                        &run_id,
                        &continuation.base_commit,
                        &evidence,
                    )
                    .await
                } else {
                    continuation.base_commit.clone()
                };
                db::set_run_converged_commit(&self.pool, &run_id, &commit).await?;
                converged_commit = Some(commit);
            }
            match self.transition(&run_id, next_state).await? {
                HookOutcome::Continue => {}
                HookOutcome::Block { reason } => {
                    let point = HookPoint::on_entering(next_state)
                        .expect("Block only fires for a hook-mapped `to`");
                    self.fail_run_on_block(&run_id, point, &reason).await?;
                    next_state = RunState::Failed;
                }
                HookOutcome::EmitFindings(findings) => {
                    let point = HookPoint::on_entering(next_state)
                        .expect("EmitFindings only fires for a hook-mapped `to`");
                    log_unrouted_findings(&run_id, point, &findings);
                }
            }
            match next_state {
                RunState::RunningStep(next) => {
                    continuation.next_step_index = next;
                }
                RunState::Converged => match &config.gate {
                    None => break RunState::Converged,
                    Some(gate_config) => {
                        let trigger = crate::gate_trigger::SubprocessGateTrigger {
                            gated_bin: gate_config.gated_bin.clone(),
                            db_path: config.warden_home.join("state.db"),
                            bare_repo_path: gate_config.bare_repo_path.clone(),
                            repo_slug: gate_config.repo_slug.clone(),
                            poll_interval_secs: gate_config.poll_interval_secs,
                            inactivity_timeout_secs: gate_config.inactivity_timeout_secs,
                        };
                        match self
                            .drive_post_convergence_tail(
                                &run_id,
                                &config,
                                &converged_commit.expect("converged commit stored"),
                                &trigger,
                            )
                            .await?
                        {
                            PostConvergenceOutcome::Terminal(state) => break state,
                            PostConvergenceOutcome::Reboucle { findings } => {
                                continuation.pending_ci_findings = findings;
                                continuation.next_step_index = config.workflow.entry();
                            }
                        }
                    }
                },
                state => break state,
            }
        };

        self.run_teardown(&run_id, &config.repo_path, final_state)
            .await;
        self.publish_event(RunEvent::RunFinished {
            final_state: final_state.as_str().to_string(),
        })
        .await?;
        Ok((run_id, final_state))
    }
}
