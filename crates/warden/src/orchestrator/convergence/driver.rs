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
        // Before the `runs` row exists: a definition this runner cannot honour is a configuration
        // error, and must not leave a half-started run behind.
        let agents = ResolvedAgents::resolve(runner, &config)?;
        let worktree_manager =
            WorktreeManager::new(&config.repo_path, config.warden_home.join("worktrees"))?;

        // the Event Bus must be live before anything worth publishing happens, so a `warden-tui`
        // that connects right after `RunStarted` never sees a socket that doesn't exist yet.
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
                config.max_review_cycles,
                config.max_test_cycles,
                config.workflow.steps.len() as u32,
                config.max_extra_step_cycles,
            )
            .await?;
            self.publish_event(RunEvent::RunStarted {
                intent: config.intent.clone(),
                branch: config.branch.clone(),
                max_review_cycles: config.max_review_cycles,
                max_test_cycles: config.max_test_cycles,
            })
            .await?;

            for untrusted in &config.untrusted_repo_agent_definitions {
                self.publish_event(RunEvent::UntrustedAgentDefinitionUsed {
                    role: untrusted.role.as_str().to_string(),
                    path: untrusted.path.display().to_string(),
                    canonical_path: untrusted.canonical_path.display().to_string(),
                })
                .await?;
            }
            if let Some(callback) = &self.on_run_started {
                callback(&run_id);
            }

            if let HookOutcome::Block { reason } = self
                .dispatch_run_hooks(
                    &run_id,
                    &config.repo_path,
                    RunState::Pending,
                    HookPoint::OnRunStart,
                )
                .await?
            {
                tracing::warn!(
                    run_id,
                    reason,
                    "on_run_start hook blocked the run; failing before the coder runs"
                );
                self.run_teardown(&run_id, &config.repo_path, RunState::Failed)
                    .await;
                self.transition(&run_id, RunState::Failed).await?;
                self.publish_event(RunEvent::RunFinished {
                    final_state: RunState::Failed.as_str().to_string(),
                })
                .await?;
                return Ok((run_id, RunState::Failed));
            }

            // Write-ahead: the run is about to launch the coder, so record the intent to do so
            // before actually spawning anything.
            self.transition(&run_id, RunState::CoderRunning).await?;
        }

        // the run's true original starting commit -- the fixed point every cycle's agent-
        // definition-tampering check (`run_coder` -> `agent_definition_tampering_finding`) compares
        // against.
        let run_base_commit_sha = match &restored {
            Some((_, continuation)) => continuation.run_base_commit_sha.clone(),
            None => read_head_commit(&config.repo_path).await?,
        };

        let run_agent_definition_snapshot = AgentDefinitionSnapshot::capture(
            &worktree_manager,
            &run_id,
            SNAPSHOT_WORKTREE_ROLE,
            &run_base_commit_sha,
        )
        .await?;

        if let Some((_, continuation)) = &restored {
            db::clear_run_rate_limit_status(&self.pool, &run_id).await?;
            self.transition(&run_id, continuation.next_run_state())
                .await?;
        }

        let continuation = restored
            .map(|(_, continuation)| continuation)
            .unwrap_or_else(|| {
                ConvergenceContinuation::new(
                    run_base_commit_sha.clone(),
                    config.workflow.steps.len(),
                )
            });
        let ConvergenceContinuation {
            run_base_commit_sha,
            mut base_commit,
            mut cycle_number,
            mut review_cycle_number,
            mut test_cycle_number,
            mut extra_step_cycle_number,
            mut pending_ci_findings,
            mut previous_cycle_id,
            mut step_last_reviewed_commit,
            mut own_step_cycle_numbers,
            mut active_cycle,
        } = continuation;
        macro_rules! continuation_at {
            ($active_cycle:expr) => {
                ConvergenceContinuation {
                    run_base_commit_sha: run_base_commit_sha.clone(),
                    base_commit: base_commit.clone(),
                    cycle_number,
                    review_cycle_number,
                    test_cycle_number,
                    extra_step_cycle_number,
                    pending_ci_findings: pending_ci_findings.clone(),
                    previous_cycle_id: previous_cycle_id.clone(),
                    step_last_reviewed_commit: step_last_reviewed_commit.clone(),
                    own_step_cycle_numbers: own_step_cycle_numbers.clone(),
                    active_cycle: $active_cycle,
                }
            };
        }

        let final_state = 'convergence: loop {
            // only inspect a CLI quota report at this boundary, before a new workflow step/cycle
            // starts.
            let resumed_cycle = active_cycle.take();
            if resumed_cycle.is_none() && self.suspend_for_anticipated_quota(&run_id).await? {
                let resets_at = db::get_run_rate_limit_status(&self.pool, &run_id)
                    .await?
                    .expect("quota suspension requires a stored rate-limit status")
                    .resets_at;
                let continuation = continuation_at!(None);
                break self
                    .persist_quota_suspension(&run_id, &config, &continuation, resets_at)
                    .await?;
            }

            let (cycle_id, prior_findings, producer_base_commit_this_cycle, resumed_gated_phase) =
                match resumed_cycle {
                    Some(ActiveCycleContinuation {
                        cycle_id,
                        prior_findings,
                        producer_base_commit,
                        phase,
                    }) => {
                        let gated = match phase {
                            ActiveCyclePhase::Producer => None,
                            ActiveCyclePhase::Gated {
                                producer_result,
                                findings,
                                next_step_index,
                                entered_extra_budget_this_cycle,
                            } => Some((
                                producer_result,
                                findings,
                                next_step_index,
                                entered_extra_budget_this_cycle,
                            )),
                        };
                        (cycle_id, prior_findings, producer_base_commit, gated)
                    }
                    None => {
                        let cycle_id = Uuid::new_v4().to_string();
                        db::insert_cycle(&self.pool, &cycle_id, &run_id, cycle_number).await?;
                        self.publish_event(RunEvent::CycleStarted { cycle_number })
                            .await?;

                        let ci_seeded_findings = pending_ci_findings.clone();
                        for finding in pending_ci_findings.drain(..) {
                            db::insert_finding(
                                &self.pool,
                                &Uuid::new_v4().to_string(),
                                &cycle_id,
                                &finding,
                            )
                            .await?;
                            self.publish_event(RunEvent::FindingRaised {
                                cycle_number,
                                source: finding.source.as_str().to_string(),
                                severity: finding.severity.as_str().to_string(),
                                file: finding.file.clone(),
                                description: finding.description.clone(),
                                action: finding.action.clone(),
                            })
                            .await?;
                        }
                        let prior_findings = select_prior_findings(
                            &self.pool,
                            ci_seeded_findings,
                            previous_cycle_id.as_deref(),
                        )
                        .await?;
                        (cycle_id, prior_findings, base_commit.clone(), None)
                    }
                };

            let producer_role = &config.workflow.steps[0].role;
            let mut producer_result = match resumed_gated_phase.as_ref() {
                Some((producer_result, _, _, _)) => producer_result.clone(),
                None => match self
                    .run_producer(
                        runner,
                        ProducerInvocation {
                            run_id: &run_id,
                            cycle_id: &cycle_id,
                            cycle_number,
                            config: &config,
                            role: producer_role,
                            agent: agents.steps[0].as_ref().expect(
                                "the producer step is always StepKind::Agent -- \
                             Workflow::parse_yaml enforces this",
                            ),
                            env_allowlist: agents.env_allowlist,
                            worktree_manager: &worktree_manager,
                            base_commit: &base_commit,
                            run_agent_definition_snapshot: &run_agent_definition_snapshot,
                            prior_findings: &prior_findings,
                            cancel: cancel.clone(),
                        },
                    )
                    .await
                {
                    Ok(result) => result,
                    Err(WardenError::QuotaSuspended { resets_at }) => {
                        let active = ActiveCycleContinuation {
                            cycle_id: cycle_id.clone(),
                            prior_findings: prior_findings.clone(),
                            producer_base_commit: producer_base_commit_this_cycle.clone(),
                            phase: ActiveCyclePhase::Producer,
                        };
                        let continuation = continuation_at!(Some(active));
                        break self
                            .persist_quota_suspension(&run_id, &config, &continuation, resets_at)
                            .await?;
                    }
                    Err(error) => return Err(error),
                },
            };
            base_commit = producer_result.commit.clone();

            // The producer has completed; do not start the first gated step when its report says
            // the configured quota threshold is reached.
            let mut findings = resumed_gated_phase
                .as_ref()
                .map(|(_, findings, _, _)| findings.clone())
                .unwrap_or_default();
            if resumed_gated_phase.is_none() {
                if let Some(finding) = producer_result.definition_tampering_finding.take() {
                    db::insert_finding(
                        &self.pool,
                        &Uuid::new_v4().to_string(),
                        &cycle_id,
                        &finding,
                    )
                    .await?;
                    self.publish_event(RunEvent::FindingRaised {
                        cycle_number,
                        source: finding.source.as_str().to_string(),
                        severity: finding.severity.as_str().to_string(),
                        file: finding.file.clone(),
                        description: finding.description.clone(),
                        action: finding.action.clone(),
                    })
                    .await?;
                    findings.push(finding);
                }
            }

            let total_steps = config.workflow.steps.len();
            let mut next_state = if let Some((_, _, next_step_index, _)) =
                resumed_gated_phase.as_ref()
            {
                RunState::RunningStep(*next_step_index)
            } else if total_steps <= 1 {
                extra_step_cycle_number += 1;
                db::set_run_current_extra_step_cycle(&self.pool, &run_id, extra_step_cycle_number)
                    .await?;
                decide_next_state_for_step(
                    &findings,
                    &config.workflow,
                    0,
                    extra_step_cycle_number,
                    config.max_extra_step_cycles,
                )
            } else {
                RunState::RunningStep(1)
            };
            let mut entered_extra_budget_this_cycle = resumed_gated_phase
                .as_ref()
                .is_some_and(|(_, _, _, entered)| *entered);
            let mut first_resumed_gated_step = resumed_gated_phase.is_some();
            while let RunState::RunningStep(step_index) = next_state {
                let step = &config.workflow.steps[step_index as usize];
                let step_agent = agents.steps[step_index as usize].as_ref();

                let step_is_scoped_re_reviewable =
                    step_index == 1 || step.gate == warden_core::Gate::ScopedReReview;
                let already_saw_this_cycles_base = step_last_reviewed_commit[step_index as usize]
                    .as_deref()
                    == Some(producer_base_commit_this_cycle.as_str());
                let scope = if step_is_scoped_re_reviewable && already_saw_this_cycles_base {
                    warden_core::ReviewScope::Correctif
                } else {
                    warden_core::ReviewScope::Full
                };

                if first_resumed_gated_step {
                    first_resumed_gated_step = false;
                } else {
                    self.transition(&run_id, RunState::RunningStep(step_index))
                        .await?;
                }
                if step.budget == Some(warden_core::StepBudget::Extra)
                    && !entered_extra_budget_this_cycle
                {
                    entered_extra_budget_this_cycle = true;
                    extra_step_cycle_number += 1;
                    db::set_run_current_extra_step_cycle(
                        &self.pool,
                        &run_id,
                        extra_step_cycle_number,
                    )
                    .await?;
                }

                // This is the last boundary before this particular step can create its worktree or
                // spawn its agent.
                if self.suspend_for_anticipated_quota(&run_id).await? {
                    let resets_at = db::get_run_rate_limit_status(&self.pool, &run_id)
                        .await?
                        .expect("quota suspension requires a stored rate-limit status")
                        .resets_at;
                    let active = ActiveCycleContinuation {
                        cycle_id: cycle_id.clone(),
                        prior_findings: prior_findings.clone(),
                        producer_base_commit: producer_base_commit_this_cycle.clone(),
                        phase: ActiveCyclePhase::Gated {
                            producer_result: producer_result.clone(),
                            findings: findings.clone(),
                            next_step_index: step_index,
                            entered_extra_budget_this_cycle,
                        },
                    };
                    let continuation = continuation_at!(Some(active));
                    break 'convergence self
                        .persist_quota_suspension(&run_id, &config, &continuation, resets_at)
                        .await?;
                }

                let step_findings = match self
                    .run_gated_step(
                        runner,
                        GatedStepInvocation {
                            run_id: &run_id,
                            cycle_id: &cycle_id,
                            cycle_number,
                            step_index,
                            role: &step.role,
                            kind: step.kind,
                            agent: step_agent,
                            run: step.run.as_deref(),
                            env_allowlist: agents.env_allowlist,
                            worktree_manager: &worktree_manager,
                            commit: &base_commit,
                            diff: &producer_result.diff,
                            prior_findings: &prior_findings,
                            scope,
                            captures_evidence: step.captures_evidence,
                            config: &config,
                            cancel: cancel.clone(),
                        },
                    )
                    .await
                {
                    Ok(findings) => findings,
                    Err(WardenError::QuotaSuspended { resets_at }) => {
                        let active = ActiveCycleContinuation {
                            cycle_id: cycle_id.clone(),
                            prior_findings: prior_findings.clone(),
                            producer_base_commit: producer_base_commit_this_cycle.clone(),
                            phase: ActiveCyclePhase::Gated {
                                producer_result: producer_result.clone(),
                                findings: findings.clone(),
                                next_step_index: step_index,
                                entered_extra_budget_this_cycle,
                            },
                        };
                        let continuation = continuation_at!(Some(active));
                        break 'convergence self
                            .persist_quota_suspension(&run_id, &config, &continuation, resets_at)
                            .await?;
                    }
                    Err(error) => return Err(error),
                };
                if step_is_scoped_re_reviewable {
                    step_last_reviewed_commit[step_index as usize] = Some(base_commit.clone());
                }

                for finding in &step_findings {
                    db::insert_finding(&self.pool, &Uuid::new_v4().to_string(), &cycle_id, finding)
                        .await?;
                    self.publish_event(RunEvent::FindingRaised {
                        cycle_number,
                        source: finding.source.as_str().to_string(),
                        severity: finding.severity.as_str().to_string(),
                        file: finding.file.clone(),
                        description: finding.description.clone(),
                        action: finding.action.clone(),
                    })
                    .await?;
                }
                findings.extend(step_findings);

                let this_step_is_blocking = findings.iter().any(|finding| {
                    finding.severity == warden_core::Severity::Blocking
                        && (finding.source == warden_core::FindingSource::role(step.role.as_str())
                            || finding.source == warden_core::FindingSource::Warden)
                });

                let (current_cycle, max_cycles) = match step.budget {
                    Some(warden_core::StepBudget::Review) => {
                        if this_step_is_blocking {
                            review_cycle_number += 1;
                        }
                        db::set_run_current_review_cycle(&self.pool, &run_id, review_cycle_number)
                            .await?;
                        (review_cycle_number, config.max_review_cycles)
                    }
                    Some(warden_core::StepBudget::Test) => {
                        test_cycle_number += 1;
                        db::set_run_current_test_cycle(&self.pool, &run_id, test_cycle_number)
                            .await?;
                        (test_cycle_number, config.max_test_cycles)
                    }
                    Some(warden_core::StepBudget::Extra) => {
                        (extra_step_cycle_number, config.max_extra_step_cycles)
                    }
                    Some(warden_core::StepBudget::Own(max_cycles)) => {
                        own_step_cycle_numbers[step_index as usize] += 1;
                        (own_step_cycle_numbers[step_index as usize], max_cycles)
                    }
                    None => unreachable!(
                        "workflow steps at index >= 1 always carry Some(budget) -- \
                         Workflow::parse_yaml's own invariant, only steps[0] (never reached by \
                         this loop) has none"
                    ),
                };

                next_state = decide_next_state_for_step(
                    &findings,
                    &config.workflow,
                    step_index,
                    current_cycle,
                    max_cycles,
                );
            }

            db::close_cycle(&self.pool, &cycle_id).await?;
            previous_cycle_id = Some(cycle_id.clone());

            let mut converged_commit_for_tail: Option<String> = None;
            if next_state == RunState::Converged {
                // / fold any evidence captured across this run's cycles into the converged commit
                // before recording it -- `store_in_repo`'s "committed...
                let converged_commit = if config.evidence_store_in_repo {
                    let evidence = db::list_evidence_for_run(&self.pool, &run_id).await?;
                    self.commit_evidence_for_convergence(
                        &worktree_manager,
                        &config,
                        &run_id,
                        &base_commit,
                        &evidence,
                    )
                    .await
                } else {
                    base_commit.clone()
                };
                // Persist commit first: readers must never observe `Converged` without its SHA.
                db::set_run_converged_commit(&self.pool, &run_id, &converged_commit).await?;
                converged_commit_for_tail = Some(converged_commit);
            }
            self.transition(&run_id, next_state).await?;

            match next_state {
                RunState::CoderRunning => {
                    cycle_number += 1;
                    continue;
                }
                RunState::Converged => {
                    let converged_commit = converged_commit_for_tail
                            .unwrap_or_else(|| unreachable!("converged_commit_for_tail is always Some when next_state == RunState::Converged"));
                    match &config.gate {
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
                                    &converged_commit,
                                    &trigger,
                                )
                                .await?
                            {
                                PostConvergenceOutcome::Terminal(state) => break state,
                                PostConvergenceOutcome::Reboucle { findings } => {
                                    cycle_number += 1;
                                    review_cycle_number = db::get_run(&self.pool, &run_id)
                                        .await?
                                        .ok_or_else(|| WardenError::RunNotFound {
                                            run_id: run_id.clone(),
                                        })?
                                        .current_review_cycle;
                                    pending_ci_findings = findings;
                                    continue;
                                }
                            }
                        }
                    }
                }
                terminal => break terminal,
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
