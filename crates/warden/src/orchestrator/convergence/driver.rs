use super::super::diff::read_head_commit;
use super::super::tampering::{AgentDefinitionSnapshot, SNAPSHOT_WORKTREE_ROLE};
use super::super::*;
use super::select_prior_findings;

impl Orchestrator {
    /// Runs a full convergence loop for one intent: opens a run, then
    /// alternates coder / review+test cycles until convergence, the cycle
    /// budget is exhausted, or `cancel` fires. Returns the run id and its
    /// final [`RunState`].
    ///
    /// `runner` maps each role's markdown definition onto the command to
    /// spawn for it, and transforms a reviewer/tester's raw output into
    /// findings (issue #24). Injected as a generic parameter, the same
    /// compile-time seam [`crate::gate_trigger::GateTrigger`] uses, so tests
    /// can substitute a fake without spawning anything real.
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
        // Before the `runs` row exists: a definition this runner cannot
        // honour is a configuration error, and must not leave a half-started
        // run behind.
        let agents = ResolvedAgents::resolve(runner, &config)?;
        let worktree_manager =
            WorktreeManager::new(&config.repo_path, config.warden_home.join("worktrees"))?;

        // Phase 8: the Event Bus must be live before anything worth
        // publishing happens, so a `warden-tui` that connects right after
        // `RunStarted` never sees a socket that doesn't exist yet.
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

            // Issue #26: one `UntrustedAgentDefinitionUsed` per repo-sourced
            // reviewer/tester definition (`--trust-repo-agents`), right after
            // `RunStarted` -- see `RunConfig::untrusted_repo_agent_definitions`'s
            // own docs for why this is an event (persisted, replayable by a
            // later `warden-tui attach`) rather than only the `tracing::warn!`
            // `agent_def::resolve_agent_definition` already logged at resolution
            // time, before this run (or its Event Bus) even existed.
            for untrusted in &config.untrusted_repo_agent_definitions {
                self.publish_event(RunEvent::UntrustedAgentDefinitionUsed {
                    role: untrusted.role.as_str().to_string(),
                    path: untrusted.path.display().to_string(),
                    canonical_path: untrusted.canonical_path.display().to_string(),
                })
                .await?;
            }
            // Issue #31: the `runs` row and the Event Bus socket both exist by
            // now, so `warden-tui attach --run-id <run_id>` is already a valid
            // command -- this is the earliest point at which printing it is
            // meaningful.
            if let Some(callback) = &self.on_run_started {
                callback(&run_id);
            }

            // Run-level setup hooks: fire once, before the coder, while the run is
            // still `Pending`. This is where deterministic environment prep runs
            // (`docker compose up -d`, `git fetch`/pull, dependency install)
            // instead of being spent as agent tokens. A `Block` means the
            // environment could not be established -- there is nothing to code
            // against -- so the run fails here, before any agent spawns. Teardown
            // still runs (finally semantics): whatever a partial setup left behind
            // gets a chance to be cleaned up.
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

            // Write-ahead: the run is about to launch the coder, so record the
            // intent to do so before actually spawning anything (ADR-0004).
            self.transition(&run_id, RunState::CoderRunning).await?;
        }

        // Issue #30: the run's true original starting commit -- the fixed
        // point every cycle's agent-definition-tampering check (`run_coder`
        // -> `agent_definition_tampering_finding`) compares against.
        // Resolved once here, before cycle 1's coder ever runs. Deliberately
        // *not* recomputed per cycle: a coder that introduces a
        // `.warden/agents/` change in cycle 1 and then leaves it untouched
        // in cycle 2 must still be caught in cycle 2, since the poisoned
        // bytes are still sitting there relative to this same fixed origin
        // -- only actually reverting them (re-resolving back to what this
        // commit holds) stops the finding from firing.
        let run_base_commit_sha = match &restored {
            Some((_, continuation)) => continuation.run_base_commit_sha.clone(),
            None => read_head_commit(&config.repo_path).await?,
        };

        // Issue #30: the raw, unparsed run-start snapshot
        // `agent_definition_tampering_finding` compares every cycle's
        // re-resolution against -- see `AgentDefinitionSnapshot::capture`'s
        // own docs for why this reads through a throwaway `git worktree`
        // checkout of `run_base_commit_sha`, exactly like every later
        // re-resolution does, rather than `config.repo_path`'s own
        // (possibly dirty) working directory.
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
            // Issue #85: only inspect a CLI quota report at this boundary,
            // before a new workflow step/cycle starts. A tool that exposes no
            // report leaves the database value absent and this is a no-op.
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

            // Issue #81 review, HIGH: captured before the producer call
            // below reassigns `base_commit` to this cycle's new commit --
            // from cycle 2 on, this is the commit this cycle's producer diff
            // was computed *against* (see `agents.rs::run_producer`'s own
            // `base_commit_sha`), and therefore the value a step's own
            // `step_last_reviewed_commit` must match for a `Correctif` scope
            // to be sound for it this cycle (see that vec's own docs above).
            // On cycle 1 it is still the literal ref `"HEAD"` (this loop's
            // initial `base_commit`) rather than a resolved SHA, which is
            // harmless: every `step_last_reviewed_commit` entry is `None`
            // then, so nothing can match it and every step gets `Full`.
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

            // The producer has completed; do not start the first gated step
            // when its report says the configured quota threshold is reached.
            let mut findings = resumed_gated_phase
                .as_ref()
                .map(|(_, findings, _, _)| findings.clone())
                .unwrap_or_default();
            // Issue #24 review, M4: folded in alongside the first gated
            // step's own findings below -- an unresolved definition-
            // tampering finding gates it exactly like that step's own
            // finding would; no later step ever runs over a producer
            // commit that still carries one either. Persisted here,
            // immediately, since it never appears in any step's own
            // `step_findings` batch persisted further down.
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
            // Issue #73 (trio-unification follow-up): one uniform loop over
            // every gated step (`workflow.steps[1..]`) -- the built-in
            // reviewer/tester and any custom step (e.g. `techlead`) are
            // driven through the exact same `run_gated_step` call, never
            // branched on role name. `step_index == 1` (the first gated
            // step) always gets `ReviewScope::Correctif` after its own first
            // pass, whatever its own declared `gate` (retro-compat: this is
            // the pre-#81 positional mechanic, decision #37 Q2, unchanged);
            // issue #81 additionally offers the same scoping to any step
            // whose own declared `gate` is `warden_core::Gate::ScopedReReview`,
            // at any position -- see `step_is_scoped_re_reviewable` below.
            //
            // Which cycle-budget flag charges a step's own counter is *not*
            // positional (issue #73 review, finding F3: reordering the
            // built-in pair used to invert the budget rule) -- it follows
            // each step's own declared `WorkflowStep::budget` instead:
            // [`warden_core::StepBudget::Review`] charges `max_review_cycles`
            // (only when this step's own evaluation is blocking, decision
            // #37 Q1); [`warden_core::StepBudget::Test`] charges
            // `max_test_cycles` (unconditionally, once per invocation);
            // [`warden_core::StepBudget::Extra`] shares `max_extra_step_cycles`
            // (charged once per cycle for the whole remaining chain, the
            // first time any such step is entered -- tracked below by
            // `entered_extra_budget_this_cycle`); [`warden_core::StepBudget::Own`]
            // (issue #81) charges this step's own `max_cycles`, unconditionally,
            // tracked by `own_step_cycle_numbers` rather than a run-level
            // column. `Workflow::builtin_default` declares its reviewer/
            // tester steps' budgets explicitly (`Review`/`Test`), so this is
            // byte-for-byte the same rule the pre-review code applied by
            // position.
            let mut next_state = if let Some((_, _, next_step_index, _)) =
                resumed_gated_phase.as_ref()
            {
                RunState::RunningStep(*next_step_index)
            } else if total_steps <= 1 {
                // Issue #73 review, finding F4: a degenerate one-step
                // workflow (producer only, no gates at all) has no later
                // gated step to catch a blocking finding the producer itself
                // already raised this cycle (a definition-tampering finding,
                // `FindingSource::Warden`, folded into `findings` above) --
                // unconditionally converging here would silently let it
                // through. Reused via `decide_next_state_for_step` at
                // `step_index == 0` (structurally `workflow.is_last_step(0)`
                // whenever `total_steps == 1`) rather than a bespoke check,
                // so a single-step workflow follows the exact same
                // "blocking Warden/role-sourced finding" rule every other
                // step already does. The producer has no budget of its own
                // (`WorkflowStep::budget` is `None` for `steps[0]`), so a
                // reboucle here shares the same `max_extra_step_cycles`
                // bucket any other budget-less step would.
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
            // Reset once per cycle -- `Some(StepBudget::Extra)` is charged
            // (the counter advanced) at most once per cycle, the first time
            // any extra-budgeted step is entered, never once per individual
            // extra step (see the loop's own docs above).
            let mut entered_extra_budget_this_cycle = resumed_gated_phase
                .as_ref()
                .is_some_and(|(_, _, _, entered)| *entered);
            let mut first_resumed_gated_step = resumed_gated_phase.is_some();
            while let RunState::RunningStep(step_index) = next_state {
                let step = &config.workflow.steps[step_index as usize];
                let step_agent = agents.steps[step_index as usize].as_ref();

                // Issue #81: scoped re-review applies to `step_index == 1`
                // unconditionally (retro-compat -- the pre-#81 positional
                // mechanic, independent of that step's own declared `gate`),
                // and to any step whose own declared `gate` is
                // `ScopedReReview` (issue #81's generalization, usable at any
                // position).
                let step_is_scoped_re_reviewable =
                    step_index == 1 || step.gate == warden_core::Gate::ScopedReReview;
                // Issue #81 review, HIGH: `Correctif` is legal only when this
                // step's last recorded commit -- the target commit it was
                // actually invoked against, last time it ran -- equals this
                // cycle's producer base commit. A step skipped in one or more
                // intervening cycles (an earlier gated step blocked before
                // reaching it) fails this check and gets `Full` instead,
                // since `Correctif` would otherwise tell it to ignore
                // producer commits from cycles it never saw at all (see
                // `step_last_reviewed_commit`'s own docs above). This
                // strictly subsumes the old "has run at least once" `bool`
                // check: a step that has never run has no recorded commit
                // (`None`) and so never matches, exactly like before.
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
                    // First entry into the shared extra-step budget this
                    // cycle -- charged once for the whole remaining chain,
                    // never once per extra step (see the loop's own docs
                    // above).
                    entered_extra_budget_this_cycle = true;
                    extra_step_cycle_number += 1;
                    db::set_run_current_extra_step_cycle(
                        &self.pool,
                        &run_id,
                        extra_step_cycle_number,
                    )
                    .await?;
                }

                // This is the last boundary before this particular step can
                // create its worktree or spawn its agent. A prior gated step
                // can have updated the run's last-known quota, so checking
                // only after the producer would let a later step start even
                // though the threshold was already crossed.
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
                    // This step's target commit *this* invocation was
                    // `base_commit` (unchanged since the producer ran, above)
                    // -- recorded so a future cycle's own base-commit check
                    // can tell whether this step actually saw everything up
                    // to that cycle's producer base, or missed one or more
                    // cycles in between (see `step_last_reviewed_commit`'s
                    // own docs above).
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

                // Issue #73 review, finding F3: which counter/rule applies
                // follows this step's own declared `budget`, not its
                // position -- see the loop's own docs above.
                let (current_cycle, max_cycles) = match step.budget {
                    Some(warden_core::StepBudget::Review) => {
                        // Issue #43 code review (MEDIUM): the review
                        // budget's own counter only advances when this
                        // cycle's reboucle is actually charged to the
                        // review phase -- decision #37 Q1's imputation
                        // rule. A cycle whose first gated step is clean --
                        // whether it's the run's very first pass, or a
                        // scoped re-review that clears a later step's own
                        // reboucle -- never advances it, which is what
                        // keeps the budgets genuinely independent.
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
                        // Issue #81: charged unconditionally, once per
                        // invocation, exactly like `Test` -- this counter is
                        // this step's own, with no sibling budget it needs
                        // to stay independent from (see `StepBudget::Own`'s
                        // own docs).
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
            // ADR-0012: this cycle is now the "previous cycle" the next
            // iteration's reviewer/tester (if there is one) reports on.
            previous_cycle_id = Some(cycle_id.clone());

            let mut converged_commit_for_tail: Option<String> = None;
            if next_state == RunState::Converged {
                // Issue #7 / ADR-0009: fold any evidence captured across
                // this run's cycles into the converged commit before
                // recording it -- `store_in_repo`'s "committed... never
                // pushed before Finalize" only holds if it rides along with
                // the very commit `converged_commit_sha` names.
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
                // M4: record the commit the run converged on before
                // persisting the state transition, so a reader that
                // observes `Converged` can never see a missing SHA.
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
                    // Documented strict invariant (code-standards.md): set
                    // unconditionally a few lines above, in the
                    // `if next_state == RunState::Converged` block --
                    // reachable here only because `next_state` is that
                    // exact same value.
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
                                    // Issue #43: `apply_ci_result_message`
                                    // charged this CI reboucle to the review
                                    // budget and persisted the advanced
                                    // counter. Re-sync the in-loop counter so
                                    // the next iteration's clean-review write
                                    // (which persists `review_cycle_number`)
                                    // can't clobber the CI charge and let a
                                    // persistently-red CI loop forever.
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

        // Run-level teardown: the `finally` counterpart of the `on_run_start`
        // setup above. Fires on every non-erroring exit of the loop, whatever
        // the final state (converged, pushed, budget-exhausted, failed), so a
        // `docker compose down` / scratch cleanup always gets a chance to run.
        self.run_teardown(&run_id, &config.repo_path, final_state)
            .await;

        self.publish_event(RunEvent::RunFinished {
            final_state: final_state.as_str().to_string(),
        })
        .await?;

        Ok((run_id, final_state))
    }
}
