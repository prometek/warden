//! The convergence loop: coder -> review/test -> reboucle if findings
//! (Architecture.md §5.1). Reviewer and tester run **in parallel**
//! (`tokio::join!`, ADR-0003), each synced onto the coder's commit in its
//! own worktree (see [`WorktreeManager::create`], keyed by role, so the two
//! never share a directory). Every [`RunState`] transition is written to
//! SQLite *before* the action it authorizes, per ADR-0004.
//!
//! Phase 8 (ADR-0008, issue #8): every significant transition is also
//! published as a [`RunEvent`] -- persisted to `events` and broadcast live on
//! the run's [`EventBus`] -- so a `warden-tui` can observe the run without
//! polling SQLite itself. See [`Orchestrator::publish_event`].

use std::path::{Path, PathBuf};

use chrono::Utc;
use sqlx::SqlitePool;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use warden_core::{
    decide_next_state, decide_next_state_after_ci, parse_findings, AgentRole, CiResultMessage,
    EvidenceTool, Finding, RunEvent, RunState,
};

use crate::ci_channel::CiResultListener;
use crate::db;
use crate::error::{Result, WardenError, WorktreeError};
use crate::event_bus::EventBus;
use crate::evidence::{self, EvidenceCaptureContext};
use crate::gate_trigger::{GateTrigger, RunTailTrigger};
use crate::process::{self, AgentCommand, AgentOutcome};
use crate::worktree::{self, WorktreeManager};

/// Static configuration for a single run of the convergence loop.
///
/// `coder_command`/`reviewer_command`/`tester_command` are the CLI agents to
/// invoke for each role (ADR-0005: Warden spawns whatever CLI the caller
/// configures; it never calls an LLM API directly, and Phase 1 does not
/// hardcode which agent binary is used).
pub struct RunConfig {
    /// The user's pre-existing repository. Never written to directly — only
    /// read to resolve the starting commit and to run `git worktree`.
    pub repo_path: PathBuf,
    /// Root directory for Warden's own state (`<warden_home>/worktrees/...`).
    pub warden_home: PathBuf,
    pub branch: String,
    pub intent: String,
    pub max_cycles: u32,
    pub coder_command: AgentCommand,
    pub reviewer_command: AgentCommand,
    pub tester_command: AgentCommand,
    /// Overrides automatic project-type detection for the Evidence Capture
    /// Adapter (`evidence.tool`, ADR-0009). `None` means "detect from the
    /// repo" (`warden_core::detect_project_type`).
    pub evidence_tool: Option<EvidenceTool>,
    /// Whether captured evidence gets committed into `.warden/evidence/` and
    /// pushed with the converged commit (`evidence.store_in_repo`,
    /// ADR-0009). Defaults to `true` at the CLI layer -- kept required here
    /// rather than defaulted in this struct so every caller states its
    /// choice explicitly.
    pub evidence_store_in_repo: bool,
    /// Issue #15/ADR-0011's post-`Converged` tail (push into the local bare
    /// gate repo + PR open/finalize + CI watch). `None` preserves this
    /// crate's original behaviour exactly: a converged run stops at
    /// `Converged` and never reaches `Pushed`/`AwaitingCi`/`Done`.
    pub gate: Option<GateConfig>,
}

/// Everything [`Orchestrator::drive_post_convergence_tail`] needs to trigger
/// `warden-gated`'s side of issue #15/ADR-0011's tail. Deliberately plain
/// data (no trigger trait object): the concrete
/// [`crate::gate_trigger::SubprocessGateTrigger`] used in production is
/// built from these fields at the one call site that needs it, and
/// `drive_post_convergence_tail` itself stays generic over
/// [`crate::gate_trigger::GateTrigger`] so tests can substitute a fake.
#[derive(Debug, Clone)]
pub struct GateConfig {
    /// The local bare gate repo `warden` pushes the converged commit into
    /// (ADR-0002) -- the same repo `warden-gated run-tail`/`resume-watch`
    /// push the PR's content from.
    pub bare_repo_path: PathBuf,
    /// Absolute path to the installed `warden-gated` binary.
    pub gated_bin: PathBuf,
    /// Explicit `owner/repo` override; `None` lets `warden-gated` resolve it
    /// from the bare repo's `origin` remote (`GhProvider::new`).
    pub repo_slug: Option<String>,
    pub poll_interval_secs: u64,
    pub inactivity_timeout_secs: u64,
}

/// Parameters for a single coder invocation. Grouped into a struct (rather
/// than passed positionally) purely to keep `run_coder`'s signature
/// readable — it has no behaviour of its own.
struct CoderInvocation<'a> {
    run_id: &'a str,
    cycle_id: &'a str,
    cycle_number: u32,
    config: &'a RunConfig,
    worktree_manager: &'a WorktreeManager,
    base_commit: &'a str,
    cancel: CancellationToken,
}

/// Parameters for a single reviewer/tester invocation. Grouped into a
/// struct (rather than passed positionally) purely to keep
/// `run_finding_agent`'s signature readable — it has no behaviour of its
/// own.
struct FindingAgentInvocation<'a> {
    run_id: &'a str,
    cycle_id: &'a str,
    cycle_number: u32,
    role: AgentRole,
    command: &'a AgentCommand,
    worktree_manager: &'a WorktreeManager,
    commit: &'a str,
    /// Only consulted for `AgentRole::Tester` (evidence capture,
    /// `evidence_tool`/`evidence_store_in_repo`/`warden_home`) -- carried
    /// through here rather than threading four separate fields.
    config: &'a RunConfig,
    cancel: CancellationToken,
}

/// The run this [`Orchestrator`] instance is currently driving, and the
/// [`EventBus`] its events are published on. Set exactly once, at the top of
/// [`Orchestrator::run_convergence_loop`] -- an orchestrator is one-run-
/// per-instance in this codebase (a fresh one is constructed per CLI
/// invocation, see `main.rs`), so this never needs to change after that.
struct RunContext {
    run_id: String,
    event_bus: EventBus,
}

/// One [`Orchestrator::drive_post_convergence_tail`] call's verdict: either
/// the run has reached a terminal [`RunState`] (`Done`/`Failed` -- see
/// [`Orchestrator::apply_ci_result_message`]), or `ChecksFailed` reboucles to
/// the coder within budget, carrying the CI findings to seed into the next
/// cycle.
#[derive(Debug)]
enum PostConvergenceOutcome {
    Terminal(RunState),
    Reboucle { findings: Vec<Finding> },
}

/// Drives the convergence loop against a persisted [`SqlitePool`].
pub struct Orchestrator {
    pool: SqlitePool,
    /// `None` until [`Orchestrator::run_convergence_loop`] starts a run.
    /// Read by [`Orchestrator::publish_event`], called from deep inside the
    /// agent-invocation call chain (`run_agent`) without needing to thread
    /// an `&EventBus`/`run_id` pair through every intermediate signature --
    /// several of those (`run_review_and_test`, `run_finding_agent`) are
    /// also exercised directly by unit tests below with a fixed argument
    /// list, so adding parameters there would be a breaking, test-rippling
    /// change for a purely additive observability feature.
    run_context: tokio::sync::OnceCell<RunContext>,
}

impl Orchestrator {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            run_context: tokio::sync::OnceCell::new(),
        }
    }

    /// Persists `event` to `events` and broadcasts it on the active run's
    /// [`EventBus`], using the exact same freshly generated id/timestamp for
    /// both (see `db::insert_event`'s docs on why that matters for
    /// `warden-tui`'s replay/live dedup). A no-op if no run is currently in
    /// progress on this instance -- only reachable from a test that calls a
    /// private agent-invocation method directly without going through
    /// [`Orchestrator::run_convergence_loop`] first (see the `run_context`
    /// field docs); the real CLI path always has a context set before any
    /// agent runs.
    async fn publish_event(&self, event: RunEvent) -> Result<()> {
        let Some(context) = self.run_context.get() else {
            return Ok(());
        };

        let id = Uuid::new_v4().to_string();
        let created_at = Utc::now().to_rfc3339();
        db::insert_event(&self.pool, &id, &context.run_id, &event, &created_at).await?;
        context.event_bus.publish(&warden_core::RunEventRecord {
            id,
            run_id: context.run_id.clone(),
            event,
            created_at,
        });
        Ok(())
    }

    /// Validates and persists a state transition by re-reading the run's
    /// *currently persisted* state first, rather than trusting an
    /// in-memory value the caller already believes is correct (L5: a
    /// transition validated against a hardcoded `from` constant can never
    /// fail, even if the database has drifted from what the loop assumes).
    /// Write-ahead of intention (ADR-0004): the new state is durable before
    /// this returns, and before the caller acts on it.
    async fn transition(&self, run_id: &str, to: RunState) -> Result<()> {
        let run =
            db::get_run(&self.pool, run_id)
                .await?
                .ok_or_else(|| WardenError::RunNotFound {
                    run_id: run_id.to_string(),
                })?;
        run.state.validate_transition(to)?;
        db::update_run_state(&self.pool, run_id, to).await?;
        Ok(())
    }

    /// Runs a full convergence loop for one intent: opens a run, then
    /// alternates coder / review+test cycles until convergence, the cycle
    /// budget is exhausted, or `cancel` fires. Returns the run id and its
    /// final [`RunState`].
    pub async fn run_convergence_loop(
        &self,
        config: RunConfig,
        cancel: CancellationToken,
    ) -> Result<(String, RunState)> {
        let run_id = Uuid::new_v4().to_string();
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

        db::insert_run(
            &self.pool,
            &run_id,
            &config.repo_path.display().to_string(),
            &config.branch,
            &config.intent,
            config.max_cycles,
        )
        .await?;
        self.publish_event(RunEvent::RunStarted {
            intent: config.intent.clone(),
            branch: config.branch.clone(),
            max_cycles: config.max_cycles,
        })
        .await?;

        // Write-ahead: the run is about to launch the coder, so record the
        // intent to do so before actually spawning anything (ADR-0004).
        self.transition(&run_id, RunState::CoderRunning).await?;

        let mut base_commit = "HEAD".to_string();
        let mut cycle_number: u32 = 1;
        // Issue #15/ADR-0011: a `ChecksFailed` CI outcome reboucles to the
        // coder exactly like a reviewer/tester blocking finding does, just
        // one step later in the pipeline -- these are seeded into the next
        // cycle's `findings` rows right below, the one time this is
        // non-empty (see the `PostConvergenceOutcome::Reboucle` arm further
        // down).
        let mut pending_ci_findings: Vec<Finding> = Vec::new();

        let final_state = loop {
            let cycle_id = Uuid::new_v4().to_string();
            db::insert_cycle(&self.pool, &cycle_id, &run_id, cycle_number).await?;
            db::set_run_current_cycle(&self.pool, &run_id, cycle_number).await?;
            self.publish_event(RunEvent::CycleStarted { cycle_number })
                .await?;

            for finding in pending_ci_findings.drain(..) {
                db::insert_finding(&self.pool, &Uuid::new_v4().to_string(), &cycle_id, &finding)
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

            base_commit = self
                .run_coder(CoderInvocation {
                    run_id: &run_id,
                    cycle_id: &cycle_id,
                    cycle_number,
                    config: &config,
                    worktree_manager: &worktree_manager,
                    base_commit: &base_commit,
                    cancel: cancel.clone(),
                })
                .await?;

            // Write-ahead: about to launch reviewer + tester.
            self.transition(&run_id, RunState::AwaitingReviewTest)
                .await?;

            let findings = self
                .run_review_and_test(
                    &run_id,
                    &cycle_id,
                    cycle_number,
                    &config,
                    &worktree_manager,
                    &base_commit,
                    cancel.clone(),
                )
                .await?;

            for finding in &findings {
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
            db::close_cycle(&self.pool, &cycle_id).await?;

            let next_state = decide_next_state(&findings, cycle_number, config.max_cycles);
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

        self.publish_event(RunEvent::RunFinished {
            final_state: final_state.as_str().to_string(),
        })
        .await?;

        Ok((run_id, final_state))
    }

    /// Drives issue #15/ADR-0011's post-`Converged` tail: pushes the
    /// converged commit into the local bare gate repo (the ADR-0002 forward
    /// channel), triggers `warden-gated`'s fresh `run-tail` (skeleton commit
    /// plus `OpenDraft`, `Finalize`, and `watch_pr`), and awaits the one
    /// terminal `CiResultMessage` it delivers back over a freshly bound
    /// reverse socket. Generic over [`GateTrigger`] so tests can inject a
    /// fake that delivers a scripted message without spawning a real
    /// `warden-gated` subprocess (code-standards.md: no network/subprocess
    /// in tests).
    ///
    /// Every state transition here is write-ahead of the action it
    /// authorizes (ADR-0004): `Pushed` is persisted before the push,
    /// `AwaitingCi` before the (possibly long) wait for a result.
    async fn drive_post_convergence_tail<G: GateTrigger>(
        &self,
        run_id: &str,
        config: &RunConfig,
        converged_commit: &str,
        trigger: &G,
    ) -> Result<PostConvergenceOutcome> {
        let Some(gate_config) = &config.gate else {
            unreachable!("drive_post_convergence_tail is only called when config.gate is Some");
        };

        self.transition(run_id, RunState::Pushed).await?;
        push_converged_commit_to_bare_repo(
            &config.repo_path,
            &gate_config.bare_repo_path,
            converged_commit,
            run_id,
        )
        .await?;

        let runs_dir = config.warden_home.join("runs");
        let listener = CiResultListener::bind(run_id, &runs_dir).await?;

        let branch = format!("warden/{run_id}");
        let summary_body = format!(
            "Run {run_id} converged.\n\nIntent:\n{}\n",
            config.intent.trim()
        );
        trigger
            .trigger_run_tail(&RunTailTrigger {
                run_id,
                branch: &branch,
                base_branch: &config.branch,
                intent: &config.intent,
                pushed_commit_sha: converged_commit,
                summary_body: &summary_body,
                ci_result_socket: listener.socket_path(),
            })
            .await?;

        self.transition(run_id, RunState::AwaitingCi).await?;

        let message = listener.receive().await?;
        self.apply_ci_result_message(run_id, &message).await
    }

    /// Applies one received [`CiResultMessage`] to `run_id`'s persisted
    /// state (issue #15/ADR-0011): maps `CiWatchOutcome` -> `CiOutcome`
    /// (`GateFailed` maps to an unconditional `Failed`, having no CI signal
    /// of its own to interpret), calls `decide_next_state_after_ci`, and
    /// writes the transition.
    ///
    /// **Idempotency guard**: applies the outcome only if the run is still
    /// `AwaitingCi`. A duplicate/stale delivery for a run that already left
    /// that state (e.g. a crash-recovery resume racing an earlier delivery)
    /// is a safe no-op, never an error (ADR-0011).
    async fn apply_ci_result_message(
        &self,
        run_id: &str,
        message: &CiResultMessage,
    ) -> Result<PostConvergenceOutcome> {
        let run =
            db::get_run(&self.pool, run_id)
                .await?
                .ok_or_else(|| WardenError::RunNotFound {
                    run_id: run_id.to_string(),
                })?;

        if run.state != RunState::AwaitingCi {
            tracing::info!(
                run_id,
                ?run.state,
                "ignoring CI result: run already left AwaitingCi (stale/duplicate delivery)"
            );
            return Ok(PostConvergenceOutcome::Terminal(run.state));
        }

        if let Some(pr_number) = message.pr_number {
            db::set_run_pr_number(&self.pool, run_id, pr_number).await?;
        }

        let next_state = match message.outcome.as_ci_outcome() {
            Some(ci_outcome) => {
                decide_next_state_after_ci(ci_outcome, run.current_cycle, run.max_cycles)
            }
            // GateFailed: no CI signal to interpret, and no cycle-budget
            // reboucle either -- an infrastructure failure (push/PR/finalize)
            // is not something re-running the coder can fix.
            None => RunState::Failed,
        };
        self.transition(run_id, next_state).await?;

        match next_state {
            RunState::CoderRunning => Ok(PostConvergenceOutcome::Reboucle {
                findings: message.outcome.findings()?,
            }),
            terminal => Ok(PostConvergenceOutcome::Terminal(terminal)),
        }
    }

    async fn run_coder(&self, invocation: CoderInvocation<'_>) -> Result<String> {
        let CoderInvocation {
            run_id,
            cycle_id,
            cycle_number,
            config,
            worktree_manager,
            base_commit,
            cancel,
        } = invocation;

        let worktree = worktree_manager
            .create(run_id, AgentRole::Coder.as_str(), base_commit)
            .await?;
        db::set_cycle_worktree_path(
            &self.pool,
            cycle_id,
            AgentRole::Coder,
            &worktree.path().display().to_string(),
        )
        .await?;

        let outcome = self
            .run_agent(
                cycle_id,
                AgentRole::Coder,
                &config.coder_command,
                worktree.path(),
                cancel,
            )
            .await?;

        // M2: a coder that exits non-zero has not reliably produced a
        // commit worth reviewing — `read_head_commit` below would just
        // return the unchanged base commit, silently making the loop look
        // like a no-op success. Fail the run explicitly instead.
        if outcome.exit_code != 0 {
            tracing::warn!(
                run_id,
                cycle_id,
                exit_code = outcome.exit_code,
                stderr = %outcome.stderr,
                "coder exited with a non-zero status; failing the run"
            );
            // Write-ahead (ADR-0004): persist Failed before returning the
            // error to the caller.
            self.transition(run_id, RunState::Failed).await?;
            // A TUI observer must see a terminal event rather than the
            // stream simply going silent -- this is the one place the run
            // ends without ever reaching `run_convergence_loop`'s own
            // `RunFinished` publish at the bottom of its loop.
            self.publish_event(RunEvent::RunFinished {
                final_state: RunState::Failed.as_str().to_string(),
            })
            .await?;
            if let Err(error) = worktree.remove().await {
                tracing::warn!(%error, "failed to clean up coder worktree after a failed coder run");
            }
            return Err(WardenError::CoderFailed {
                run_id: run_id.to_string(),
                cycle_id: cycle_id.to_string(),
                exit_code: outcome.exit_code,
                stderr: truncate_for_error(&outcome.stderr),
            });
        }

        let new_commit = read_head_commit(worktree.path()).await?;

        // M4: protect the commit from `git gc` (worktrees share the main
        // repo's object store, so this commit becomes unreachable garbage
        // the moment its worktree is removed) and persist its SHA so it
        // stays discoverable — both purely local git/DB operations, no
        // push, no remote (that's Phase 3's git gate).
        protect_cycle_commit(&config.repo_path, run_id, cycle_number, &new_commit).await?;
        db::set_cycle_commit_sha(&self.pool, cycle_id, &new_commit).await?;

        if let Err(error) = worktree.remove().await {
            tracing::warn!(%error, "failed to clean up coder worktree after cycle");
        }

        Ok(new_commit)
    }

    /// Runs reviewer and tester **concurrently** (ADR-0003) via
    /// `tokio::join!`, each against its own worktree synced onto `commit`
    /// (`WorktreeManager::create` paths worktrees by `<run_id>/<role>`, so
    /// the two agents never touch the same directory — no shared mutable
    /// state between the two concurrent branches, per code-standards.md
    /// "Async & concurrence"). On an `Err` from either branch, `tokio::join!`
    /// still awaits the other branch to completion before this returns, so a
    /// failing reviewer never leaves the tester's worktree/process
    /// bookkeeping half-done (the exception is a panic, which unwinds and
    /// drops the sibling future without awaiting it — mitigated by
    /// `kill_on_drop` on the spawned child process and `Worktree`'s `Drop`
    /// impl, both of which clean up best-effort even on an ungraceful exit).
    #[allow(clippy::too_many_arguments)]
    async fn run_review_and_test(
        &self,
        run_id: &str,
        cycle_id: &str,
        cycle_number: u32,
        config: &RunConfig,
        worktree_manager: &WorktreeManager,
        commit: &str,
        cancel: CancellationToken,
    ) -> Result<Vec<Finding>> {
        // ADR-0003 / issue #2 explicitly permit "tokio::join! ou
        // équivalent"; code-standards.md's "sa propre task" phrasing is
        // satisfied loosely here rather than via `tokio::spawn`. `join!`
        // polls both futures concurrently on the current task, which is
        // enough: the actual agent work happens in a child process started
        // by `process::spawn` (with `kill_on_drop`), so a dedicated tokio
        // task around `run_finding_agent` would add no real isolation --
        // the child process is already the isolation boundary, and its
        // worktree already gives it a private working directory.
        let (reviewer_result, tester_result) = tokio::join!(
            self.run_finding_agent(FindingAgentInvocation {
                run_id,
                cycle_id,
                cycle_number,
                role: AgentRole::Reviewer,
                command: &config.reviewer_command,
                worktree_manager,
                commit,
                config,
                cancel: cancel.clone(),
            }),
            self.run_finding_agent(FindingAgentInvocation {
                run_id,
                cycle_id,
                cycle_number,
                role: AgentRole::Tester,
                command: &config.tester_command,
                worktree_manager,
                commit,
                config,
                cancel,
            })
        );

        let mut findings = reviewer_result?;
        findings.extend(tester_result?);
        Ok(findings)
    }

    async fn run_finding_agent(
        &self,
        invocation: FindingAgentInvocation<'_>,
    ) -> Result<Vec<Finding>> {
        let FindingAgentInvocation {
            run_id,
            cycle_id,
            cycle_number,
            role,
            command,
            worktree_manager,
            commit,
            config,
            cancel,
        } = invocation;

        let worktree = worktree_manager
            .create(run_id, role.as_str(), commit)
            .await?;
        db::set_cycle_worktree_path(
            &self.pool,
            cycle_id,
            role,
            &worktree.path().display().to_string(),
        )
        .await?;

        let outcome = self
            .run_agent(cycle_id, role, command, worktree.path(), cancel.clone())
            .await?;

        // Agent stdout is untrusted input: a parse failure becomes a
        // blocking finding describing the problem, never a run-ending
        // panic (code-standards.md: "Ne jamais faire confiance à la sortie
        // d'un agent CLI").
        let findings = match parse_findings(&outcome.stdout) {
            Ok(findings) => findings,
            Err(parse_error) => {
                tracing::warn!(%parse_error, ?role, stdout = %outcome.stdout, "agent produced unparsable output");
                vec![Finding {
                    source: role_to_finding_source(role),
                    severity: warden_core::Severity::Blocking,
                    file: None,
                    description: format!("{role:?} produced unparsable output: {parse_error}"),
                    action: Some("fix the agent's output format".to_string()),
                }]
            }
        };

        // ADR-0009 (issue #7): capture evidence right after a *successful*
        // tester run, still inside its worktree -- which is about to be
        // removed below, so this must happen before that, not after.
        if role == AgentRole::Tester && tester_succeeded(&findings) {
            self.capture_evidence_for_cycle(
                run_id,
                cycle_id,
                cycle_number,
                config,
                worktree.path(),
                cancel,
            )
            .await;
        }

        if let Err(error) = worktree.remove().await {
            tracing::warn!(%error, ?role, "failed to clean up worktree after cycle");
        }

        Ok(findings)
    }

    /// Best-effort evidence commit at convergence (ADR-0009 / code-review
    /// MEDIUM finding #1, issue #7): mirrors `capture_evidence_for_cycle`'s
    /// philosophy -- a git failure while folding captured evidence into the
    /// repo (disk full, permissions, an evidence worktree collision, ...)
    /// must not abort an otherwise-converged run. Falls back to
    /// `base_commit` (i.e. "converge without evidence attached") and logs
    /// loudly rather than swallowing the error silently (code-standards.md:
    /// "catch-and-ignore ... qui jette l'erreur sans la logger").
    async fn commit_evidence_for_convergence(
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
    async fn capture_evidence_for_cycle(
        &self,
        run_id: &str,
        cycle_id: &str,
        cycle_number: u32,
        config: &RunConfig,
        tester_worktree_path: &Path,
        cancel: CancellationToken,
    ) {
        if let Err(error) = self
            .try_capture_evidence_for_cycle(
                run_id,
                cycle_id,
                cycle_number,
                config,
                tester_worktree_path,
                cancel,
            )
            .await
        {
            tracing::warn!(
                %error,
                run_id,
                cycle_id,
                "evidence capture failed; continuing without evidence for this cycle"
            );
        }
    }

    async fn try_capture_evidence_for_cycle(
        &self,
        run_id: &str,
        cycle_id: &str,
        cycle_number: u32,
        config: &RunConfig,
        tester_worktree_path: &Path,
        cancel: CancellationToken,
    ) -> Result<()> {
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
            record_command: &config.tester_command,
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

    /// Spawns `command`, persisting its PID to `agent_processes` before
    /// awaiting completion so a crash of the orchestrator itself (not the
    /// agent) is still detectable on restart via [`recover_crashed_runs`].
    async fn run_agent(
        &self,
        cycle_id: &str,
        role: AgentRole,
        command: &AgentCommand,
        cwd: &Path,
        cancel: CancellationToken,
    ) -> Result<AgentOutcome> {
        let child = process::spawn(command, cwd)?;
        // H1: never persist pid 0. A missing `Child::id()` right after
        // spawn is a typed error, not a silent fallback — a persisted pid
        // 0 would make `is_process_alive` misreport this run as having a
        // live process forever (POSIX `kill(0, ...)` semantics), defeating
        // crash recovery.
        let pid = child
            .id()
            .ok_or_else(|| crate::error::ProcessError::MissingPid {
                command: command.program.clone(),
            })?;
        let process_id = Uuid::new_v4().to_string();

        db::insert_agent_process(
            &self.pool,
            &process_id,
            cycle_id,
            role,
            pid,
            &cwd.display().to_string(),
        )
        .await?;
        self.publish_event(RunEvent::AgentStarted {
            role: role.as_str().to_string(),
        })
        .await?;

        let outcome_result = process::wait(child, &command.program, cancel).await;
        let exit_code_for_db = match &outcome_result {
            Ok(outcome) => outcome.exit_code,
            Err(_) => -1,
        };
        db::mark_agent_process_ended(&self.pool, &process_id, exit_code_for_db).await?;

        // L1: log stderr on the success path too — previously only ever
        // surfaced when findings-parsing failed, so a noisy-but-successful
        // agent (warnings, debug chatter) left no trace anywhere.
        if let Ok(outcome) = &outcome_result {
            if !outcome.stderr.trim().is_empty() {
                tracing::debug!(cycle_id, ?role, stderr = %outcome.stderr, "agent stderr output");
            }
            self.publish_event(RunEvent::AgentFinished {
                role: role.as_str().to_string(),
                exit_code: outcome.exit_code,
            })
            .await?;
        }

        Ok(outcome_result?)
    }
}

/// Bounds how much of an agent's stderr is embedded in an error message —
/// full output is already logged via `tracing` before this is constructed;
/// this is just what surfaces in `Display`/CLI output.
const MAX_ERROR_STDERR_LEN: usize = 2000;

fn truncate_for_error(stderr: &str) -> String {
    if stderr.len() <= MAX_ERROR_STDERR_LEN {
        return stderr.to_string();
    }
    // Truncate on a char boundary — stderr is arbitrary agent output and
    // may contain multi-byte UTF-8, so a byte-offset slice could panic.
    let boundary = stderr
        .char_indices()
        .map(|(i, _)| i)
        .take_while(|&i| i <= MAX_ERROR_STDERR_LEN)
        .last()
        .unwrap_or(0);
    format!("{}… (truncated)", &stderr[..boundary])
}

/// "The cycle's e2e test succeeded" (ADR-0009: evidence is captured "après
/// le succès du test e2e"), inferred as "the tester itself raised no
/// blocking finding" -- there's no separate pass/fail signal in the
/// findings protocol, so absence of a blocking `Tester`-sourced finding is
/// the only available proxy.
fn tester_succeeded(findings: &[Finding]) -> bool {
    !findings.iter().any(|finding| {
        finding.source == warden_core::FindingSource::Tester
            && finding.severity == warden_core::Severity::Blocking
    })
}

fn role_to_finding_source(role: AgentRole) -> warden_core::FindingSource {
    match role {
        AgentRole::Reviewer => warden_core::FindingSource::Reviewer,
        AgentRole::Tester => warden_core::FindingSource::Tester,
        // Coder never produces findings; only used defensively.
        AgentRole::Coder => warden_core::FindingSource::Reviewer,
    }
}

async fn read_head_commit(worktree_path: &Path) -> Result<String> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["rev-parse", "HEAD"])
        .output()
        .await?;

    if !output.status.success() {
        return Err(WardenError::Worktree(WorktreeError::GitCommandFailed {
            command: format!("git -C {} rev-parse HEAD", worktree_path.display()),
            exit_code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The ref prefix `warden` stages a converged run's commit under in the
/// local bare gate repo (issue #15 review, H2).
///
/// **Deliberately outside `refs/heads/`, and specifically NOT
/// `refs/heads/warden-run/`** (`warden_gated::notification::GATE_REF_PREFIX`,
/// the ref the installed `post-receive` hook / `serve` daemon watch for a
/// push-notification -- see `notification::parse_post_receive_line` /
/// `serve::handle_push_notification_line`): this push exists *only* to
/// transfer git objects into the bare repo's object store so
/// `warden-gated run-tail`/`resume-watch` can find `commit_sha` by SHA
/// (ADR-0002: the bare repo is a separate git repository from the user's
/// own, so the commit isn't otherwise reachable there at all). It is not a
/// push-notification and must never be treated as one -- staging under
/// `GATE_REF_PREFIX` would make a *deployed* gate (hook + `serve` daemon
/// both installed) independently re-verify and then force-push this
/// business content straight to `origin/<target_branch>`, bypassing the PR
/// review flow entirely and effectively auto-merging without a human
/// (exactly what ADR-0002/issue #5 forbid). The PR-based path
/// (`run_tail`/`Finalize`) is the only thing that ever pushes this content
/// on to real `origin`, onto the run's own PR branch, never `main` directly.
const GATE_STAGING_REF_PREFIX: &str = "refs/warden-staging/";

/// Pushes `commit_sha` (already reachable in `repo_path`'s object store --
/// `protect_cycle_commit` keeps it so) into the local bare gate repo under
/// this run's staging ref ([`GATE_STAGING_REF_PREFIX`]), transferring the
/// objects `warden-gated`'s `run-tail`/`resume-watch` need before they can
/// push anything onward to real `origin`. `--force`: a reboucled run pushes
/// a new converged commit onto the same per-run ref repeatedly, and this
/// ref is exclusively `warden`-managed, so a rejected non-fast-forward push
/// here would only ever be a false alarm, never a real conflict with
/// anything else touching it.
async fn push_converged_commit_to_bare_repo(
    repo_path: &Path,
    bare_repo_path: &Path,
    commit_sha: &str,
    run_id: &str,
) -> Result<()> {
    let refspec = format!("{commit_sha}:{GATE_STAGING_REF_PREFIX}{run_id}");
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["push", "--force"])
        .arg(bare_repo_path)
        .arg(&refspec)
        .output()
        .await?;

    if !output.status.success() {
        return Err(WardenError::Worktree(WorktreeError::GitCommandFailed {
            command: format!(
                "git -C {} push --force {} {refspec}",
                repo_path.display(),
                bare_repo_path.display()
            ),
            exit_code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }));
    }
    Ok(())
}

/// Creates a local ref pointing at `commit_sha` in the main repository
/// (M4), so the commit produced by a cycle's coder stays reachable — and
/// therefore safe from `git gc` — after its worktree is removed. Worktrees
/// share the main repo's object store, so a commit with nothing pointing
/// at it becomes ordinary unreachable garbage the moment its worktree is
/// gone.
///
/// This only ever writes to `.git/refs/...` (repository metadata), never to
/// the main repo's checked-out working tree files, index, or current
/// branch — the same category of write `git worktree add/remove` already
/// makes to `.git/worktrees/...`. It is a purely local git operation: no
/// push, no remote, no interaction with `origin` — that boundary belongs to
/// Phase 3's git gate.
async fn protect_cycle_commit(
    main_repo_path: &Path,
    run_id: &str,
    cycle_number: u32,
    commit_sha: &str,
) -> Result<()> {
    let ref_name = format!("refs/warden/runs/{run_id}/cycle-{cycle_number}");
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(main_repo_path)
        .args(["update-ref", &ref_name, commit_sha])
        .output()
        .await?;

    if !output.status.success() {
        return Err(WardenError::Worktree(WorktreeError::GitCommandFailed {
            command: format!(
                "git -C {} update-ref {ref_name} {commit_sha}",
                main_repo_path.display()
            ),
            exit_code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }));
    }

    Ok(())
}

/// Crash recovery (Architecture.md §6, "Règle de récupération" / §9
/// Disaster Recovery): any run left in an intermediate state
/// ([`RunState::is_intermediate`]) with no live process associated is
/// marked `Failed`. A run whose latest agent process is still alive — same
/// PID *and* same recorded start time, see `process::is_process_alive` and
/// H1 — is left untouched; Phase 1 does not attempt to re-attach to it.
///
/// Beyond the state transition itself, a run recovered as `Failed` (issue
/// #6) may have left two kinds of resources orphaned by the crash: worktrees
/// whose owning `Worktree` guard never ran `Drop` (a crash is a `SIGKILL`,
/// not a graceful drop), and agent child processes that outlive the
/// orchestrator that spawned them (`kill_on_drop` also never fires without a
/// `Drop`). Reclaiming both is [`reclaim_orphan_resources`] — best-effort, a
/// cleanup failure for one run is logged and does not stop recovery from
/// proceeding to the next one.
///
/// The state write and that reclaim are two separate steps, so a *second*
/// crash — the orchestrator dying again in the window between persisting
/// `Failed` and cleanup finishing — must not be able to orphan a resource
/// permanently: a run already `Failed` is no longer
/// [`RunState::is_intermediate`], so it would never be revisited by the pass
/// below on its own. [`db::list_failed_runs_with_pending_cleanup`] is the
/// second pass that catches exactly that case, driven off what's still
/// recorded (an open `agent_processes` row, or a worktree path not yet
/// cleared) rather than off the run's state — so it keeps retrying a
/// specific run only for as long as it actually still has something to
/// reclaim.
///
/// Returns the ids of runs newly transitioned to `Failed` by this call (not
/// runs merely revisited for cleanup because an earlier, interrupted pass
/// already made that transition).
pub async fn recover_crashed_runs(pool: &SqlitePool) -> Result<Vec<String>> {
    let intermediate_runs = db::list_intermediate_runs(pool).await?;
    let mut failed_run_ids = Vec::new();

    for run in intermediate_runs {
        // Issue #15/ADR-0011: `AwaitingCi` is intermediate, but has no
        // *agent* process to check liveness of at all -- the run is waiting
        // on `warden-gated`'s CI watch, tracked nowhere in
        // `agent_processes`. Treating it the same as a crashed coder/
        // reviewer/tester (no live process found => `Failed`) would
        // incorrectly fail every run still legitimately waiting on CI.
        // Left untouched here; [`resume_awaiting_ci_runs`] is the dedicated
        // recovery path for this state, called separately once a
        // [`crate::gate_trigger::GateTrigger`] is available to re-request
        // the watch with (this free function has no trigger of its own).
        if run.state == RunState::AwaitingCi {
            continue;
        }

        let open_process = db::latest_open_agent_process_for_run(pool, &run.id).await?;
        let has_live_process = open_process
            .map(|p| process::is_process_alive(p.pid, p.pid_started_at_unix))
            .unwrap_or(false);

        if has_live_process {
            tracing::info!(run_id = %run.id, "intermediate run has a live process; leaving state untouched");
            continue;
        }

        run.state.validate_transition(RunState::Failed)?;
        db::update_run_state(pool, &run.id, RunState::Failed).await?;
        tracing::warn!(run_id = %run.id, previous_state = run.state.as_str(), "run recovered as Failed: no live process found");

        reclaim_orphan_resources(pool, &run).await;
        failed_run_ids.push(run.id);
    }

    // Second pass (issue #6, HIGH): resumes cleanup for runs that were
    // already `Failed` before this call — either by an earlier, interrupted
    // recovery pass (the case this exists for), or by the normal
    // `CoderFailed` path in `run_coder`, which already removes its own
    // worktree, so those runs simply won't match here at all. Idempotent by
    // construction: a run stops being returned the moment nothing is left
    // recorded for it to reclaim.
    let failed_runs_needing_cleanup = db::list_failed_runs_with_pending_cleanup(pool).await?;
    for run in failed_runs_needing_cleanup {
        tracing::warn!(run_id = %run.id, "resuming orphan cleanup for a run already marked Failed by an earlier, interrupted recovery pass");
        reclaim_orphan_resources(pool, &run).await;
    }

    Ok(failed_run_ids)
}

/// Phase 6 crash-recovery counterpart of issue #15/ADR-0011: resumes every
/// run found stuck in `AwaitingCi` on startup, re-requesting the watch from
/// `warden-gated` rather than treating it like a crashed agent process (see
/// [`recover_crashed_runs`]'s own doc comment on why that check does not
/// apply here). Idempotent by construction: `watch_pr` re-polls GitHub, so
/// re-requesting a terminal PR just returns that same terminal outcome
/// again (ADR-0011) -- `warden-gated` keeps no watch state of its own for
/// this to lose.
///
/// A run with no persisted `pr_number` (crashed before `OpenDraft` ever
/// returned one) has nothing to resume watching and is marked `Failed`
/// instead -- there is no PR to re-derive.
///
/// Returns the ids of runs this call reached a terminal state for (`Done`
/// or `Failed`) or reboucled to `CoderRunning`, purely for the caller's own
/// startup logging (mirrors [`recover_crashed_runs`]'s return contract).
pub async fn resume_awaiting_ci_runs<G: GateTrigger>(
    pool: &SqlitePool,
    warden_home: &Path,
    trigger: &G,
) -> Result<Vec<String>> {
    let intermediate_runs = db::list_intermediate_runs(pool).await?;
    let orchestrator = Orchestrator::new(pool.clone());
    let mut resumed_run_ids = Vec::new();

    for run in intermediate_runs {
        if run.state != RunState::AwaitingCi {
            continue;
        }

        let Some(pr_number) = run.pr_number else {
            tracing::warn!(
                run_id = %run.id,
                "run stuck in AwaitingCi with no pr_number recorded; nothing to resume watching, marking Failed"
            );
            run.state.validate_transition(RunState::Failed)?;
            db::update_run_state(pool, &run.id, RunState::Failed).await?;
            resumed_run_ids.push(run.id);
            continue;
        };

        tracing::info!(run_id = %run.id, pr_number, "resuming CI watch for a run stuck in AwaitingCi");
        let runs_dir = warden_home.join("runs");
        let listener = CiResultListener::bind(&run.id, &runs_dir).await?;
        trigger
            .trigger_resume_watch(&run.id, pr_number, listener.socket_path())
            .await?;
        let message = listener.receive().await?;
        orchestrator
            .apply_ci_result_message(&run.id, &message)
            .await?;
        resumed_run_ids.push(run.id);
    }

    Ok(resumed_run_ids)
}

/// Reclaims both kinds of resources a crashed run may have left orphaned
/// (issue #6). Processes are terminated *before* worktrees are removed: a
/// still-live orphan agent's `cwd` is inside the worktree directory
/// `cleanup_orphan_worktrees` is about to `git worktree remove --force`, and
/// could keep writing to (or recreating files in) it while removal is in
/// progress otherwise. Both steps are best-effort — failures are logged,
/// never propagated, so one bad run's cleanup never stops the rest of
/// recovery; each step is independently safe to retry on a later pass (see
/// [`terminate_orphan_processes`] and [`cleanup_orphan_worktrees`]).
async fn reclaim_orphan_resources(pool: &SqlitePool, run: &db::Run) {
    if let Err(error) = terminate_orphan_processes(pool, &run.id).await {
        tracing::error!(run_id = %run.id, %error, "failed to terminate orphan agent processes during crash recovery");
    }
    if let Err(error) = cleanup_orphan_worktrees(pool, run).await {
        tracing::error!(run_id = %run.id, %error, "failed to clean up orphaned worktrees during crash recovery");
    }
}

/// Removes every worktree recorded across `run`'s cycles that still exists
/// on disk, then runs `git worktree prune` once to clear any leftover
/// `.git/worktrees/...` administrative entries (issue #6). `run.repo_path`
/// (persisted at `insert_run` time) is the main repository to run these git
/// commands against — the same one `WorktreeManager` would have used had the
/// orchestrator not crashed.
///
/// A path is only cleared from `cycles` (via
/// `db::clear_cycle_worktree_path`) once it is actually confirmed removed —
/// that's what lets [`db::list_failed_runs_with_pending_cleanup`] stop
/// re-processing a run once its cleanup has genuinely succeeded, and keeps
/// retrying it otherwise.
///
/// Per-path removal failures (`remove_orphan_worktree` — already-gone paths
/// are not an error, see its docs) and the final prune are both logged, not
/// propagated: one bad worktree must not stop the rest of crash recovery.
async fn cleanup_orphan_worktrees(pool: &SqlitePool, run: &db::Run) -> Result<()> {
    let entries = db::list_cycle_worktree_entries_for_run(pool, &run.id).await?;
    if entries.is_empty() {
        return Ok(());
    }

    let main_repo_path = Path::new(&run.repo_path);
    for entry in &entries {
        match worktree::remove_orphan_worktree(main_repo_path, Path::new(&entry.path)).await {
            Ok(()) => {
                if let Err(error) =
                    db::clear_cycle_worktree_path(pool, &entry.cycle_id, entry.role).await
                {
                    tracing::error!(run_id = %run.id, cycle_id = %entry.cycle_id, %error, "failed to clear recorded worktree path after removing it");
                }
            }
            Err(error) => {
                tracing::error!(run_id = %run.id, worktree_path = %entry.path, %error, "failed to remove orphaned worktree");
            }
        }
    }

    if let Err(error) = worktree::prune_worktrees(main_repo_path).await {
        tracing::error!(run_id = %run.id, %error, "git worktree prune failed during crash recovery");
    }

    Ok(())
}

/// Exit code recorded for an `agent_processes` row that crash recovery
/// closed out itself, rather than one the process reported on a normal
/// exit — mirrors the `-1` `run_agent` already records for a cancelled or
/// errored outcome (never a valid real exit code), named here for clarity
/// at this second call site.
const RECOVERY_TERMINATED_EXIT_CODE: i32 = -1;

/// Terminates every agent process still recorded as open
/// (`agent_processes.ended_at IS NULL`) for a run crash recovery is
/// reclaiming, then marks each successfully-handled row ended — closing out
/// bookkeeping the crashed orchestrator never got to write itself. Safety
/// against PID reuse is delegated entirely to `process::kill_pid`, which
/// checks the recorded start time against the *exact same* process handle it
/// signals, in one refresh (H1): a process whose fingerprint no longer
/// matches is left untouched, never killed.
///
/// One process's failure — a DB error, or `kill_pid` itself failing — is
/// logged and does not stop the rest from being processed. A process
/// `kill_pid` could not terminate is deliberately left `ended_at IS NULL`:
/// it is still alive, so the row must stay visible to a later recovery pass
/// (via `db::list_failed_runs_with_pending_cleanup`) rather than being
/// forgotten about while the process keeps running.
async fn terminate_orphan_processes(pool: &SqlitePool, run_id: &str) -> Result<()> {
    let open_processes = db::list_open_agent_processes_for_run(pool, run_id).await?;

    for open_process in open_processes {
        if let Err(error) = process::kill_pid(open_process.pid, open_process.pid_started_at_unix) {
            tracing::error!(
                run_id,
                pid = open_process.pid,
                %error,
                "failed to terminate a live orphan agent process; leaving its row open for a later recovery pass"
            );
            continue;
        }

        if let Err(error) =
            db::mark_agent_process_ended(pool, &open_process.id, RECOVERY_TERMINATED_EXIT_CODE)
                .await
        {
            tracing::error!(
                run_id,
                pid = open_process.pid,
                %error,
                "failed to mark a terminated orphan agent process ended"
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as SyncCommand;
    use tempfile::TempDir;

    fn init_test_repo() -> TempDir {
        let dir = TempDir::new().expect("tempdir");
        let run = |args: &[&str]| {
            let status = SyncCommand::new("git")
                .current_dir(dir.path())
                .args(args)
                .status()
                .expect("spawn git");
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["init", "--quiet"]);
        run(&["config", "user.email", "test@warden.local"]);
        run(&["config", "user.name", "warden-test"]);
        std::fs::write(dir.path().join("README.md"), "warden test repo\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "--quiet", "-m", "initial commit"]);
        dir
    }

    /// A coder that flips `status.txt` between "broken" and "fixed" each
    /// time it runs, and a reviewer that raises a blocking finding only
    /// while it reads "broken" — this deterministically exercises exactly
    /// one reboucle before converging, without depending on a real AI
    /// agent (out of scope for Phase 1; see ADR-0005 for the general
    /// subprocess contract this stands in for).
    fn flip_status_coder() -> AgentCommand {
        AgentCommand::new(
            "sh",
            [
                "-c",
                r#"
                if [ -f status.txt ] && [ "$(cat status.txt)" = "broken" ]; then
                    echo fixed > status.txt
                else
                    echo broken > status.txt
                fi
                git add status.txt
                git -c user.email=test@warden.local -c user.name=warden-test commit -q -m "coder cycle"
                "#,
            ],
        )
    }

    /// NDJSON wire format (code-standards.md "Agent Subprocess Protocol",
    /// M3): one finding object per line, no wrapping `{"findings": [...]}`.
    /// "No findings" is simply no stdout at all.
    fn status_gated_reviewer() -> AgentCommand {
        AgentCommand::new(
            "sh",
            [
                "-c",
                r#"
                if [ -f status.txt ] && [ "$(cat status.txt)" = "broken" ]; then
                    echo '{"source":"reviewer","severity":"blocking","description":"status is broken"}'
                fi
                "#,
            ],
        )
    }

    fn always_passing_tester() -> AgentCommand {
        AgentCommand::new("sh", ["-c", "true"])
    }

    #[tokio::test]
    async fn full_cycle_reboucles_once_then_converges() {
        let repo = init_test_repo();
        let warden_home = TempDir::new().unwrap();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();

        let orchestrator = Orchestrator::new(pool.clone());
        let config = RunConfig {
            repo_path: repo.path().to_path_buf(),
            warden_home: warden_home.path().to_path_buf(),
            branch: "main".to_string(),
            intent: "flip status to fixed".to_string(),
            max_cycles: 5,
            coder_command: flip_status_coder(),
            reviewer_command: status_gated_reviewer(),
            tester_command: always_passing_tester(),
            evidence_tool: None,
            evidence_store_in_repo: true,
            gate: None,
        };

        let (run_id, final_state) = orchestrator
            .run_convergence_loop(config, CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(final_state, RunState::Converged);

        let run = db::get_run(&pool, &run_id).await.unwrap().unwrap();
        assert_eq!(run.state, RunState::Converged);
        // Cycle 1: coder writes "broken", reviewer blocks -> reboucle.
        // Cycle 2: coder writes "fixed", reviewer passes -> converged.
        assert_eq!(run.current_cycle, 2);

        // Never written into the user's main repo working tree: only
        // Warden's own worktrees under warden_home should contain the
        // coder's commits; the main repo stays on its original commit.
        let main_repo_log = SyncCommand::new("git")
            .current_dir(repo.path())
            .args(["log", "--oneline"])
            .output()
            .unwrap();
        let commit_count = String::from_utf8_lossy(&main_repo_log.stdout)
            .lines()
            .count();
        assert_eq!(
            commit_count, 1,
            "main repo must still only have its initial commit"
        );
    }

    #[tokio::test]
    async fn max_cycles_exceeded_when_findings_never_clear() {
        let repo = init_test_repo();
        let warden_home = TempDir::new().unwrap();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();

        let orchestrator = Orchestrator::new(pool.clone());
        let always_blocking_reviewer = AgentCommand::new(
            "sh",
            [
                "-c",
                r#"echo '{"source":"reviewer","severity":"blocking","description":"never happy"}'"#,
            ],
        );
        let noop_coder = AgentCommand::new(
            "sh",
            [
                "-c",
                r#"echo change >> notes.txt && git add notes.txt && git -c user.email=t@w.local -c user.name=w commit -q -m cycle"#,
            ],
        );

        let config = RunConfig {
            repo_path: repo.path().to_path_buf(),
            warden_home: warden_home.path().to_path_buf(),
            branch: "main".to_string(),
            intent: "never converges".to_string(),
            max_cycles: 2,
            coder_command: noop_coder,
            reviewer_command: always_blocking_reviewer,
            tester_command: always_passing_tester(),
            evidence_tool: None,
            evidence_store_in_repo: true,
            gate: None,
        };

        let (_run_id, final_state) = orchestrator
            .run_convergence_loop(config, CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(final_state, RunState::MaxCyclesExceeded);
    }

    #[tokio::test]
    async fn recovery_marks_intermediate_run_failed_when_its_process_is_dead() {
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();

        db::insert_run(&pool, "crashed-run", "/tmp/repo", "main", "intent", 3)
            .await
            .unwrap();
        db::update_run_state(&pool, "crashed-run", RunState::CoderRunning)
            .await
            .unwrap();
        db::insert_cycle(&pool, "crashed-cycle", "crashed-run", 1)
            .await
            .unwrap();

        // A process that has already exited by the time we check it —
        // deterministic "dead pid" without guessing at unused pid numbers.
        let mut child = tokio::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let dead_pid = child.id().unwrap();
        child.wait().await.unwrap();

        db::insert_agent_process(
            &pool,
            "crashed-process",
            "crashed-cycle",
            AgentRole::Coder,
            dead_pid,
            "/tmp/wt",
        )
        .await
        .unwrap();
        // Deliberately never call mark_agent_process_ended: this simulates
        // the orchestrator crashing mid-`run_agent`, before it could record
        // completion.

        let failed = recover_crashed_runs(&pool).await.unwrap();
        assert_eq!(failed, vec!["crashed-run".to_string()]);

        let run = db::get_run(&pool, "crashed-run").await.unwrap().unwrap();
        assert_eq!(run.state, RunState::Failed);
    }

    #[tokio::test]
    async fn recovery_leaves_intermediate_run_alone_when_its_process_is_alive() {
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();

        db::insert_run(&pool, "live-run", "/tmp/repo", "main", "intent", 3)
            .await
            .unwrap();
        db::update_run_state(&pool, "live-run", RunState::CoderRunning)
            .await
            .unwrap();
        db::insert_cycle(&pool, "live-cycle", "live-run", 1)
            .await
            .unwrap();

        let mut child = tokio::process::Command::new("sh")
            .args(["-c", "sleep 5"])
            .spawn()
            .unwrap();
        let live_pid = child.id().unwrap();

        db::insert_agent_process(
            &pool,
            "live-process",
            "live-cycle",
            AgentRole::Coder,
            live_pid,
            "/tmp/wt",
        )
        .await
        .unwrap();

        let failed = recover_crashed_runs(&pool).await.unwrap();
        assert!(failed.is_empty());

        let run = db::get_run(&pool, "live-run").await.unwrap().unwrap();
        assert_eq!(run.state, RunState::CoderRunning);

        child.kill().await.unwrap();
    }

    /// Issue #6, acceptance criterion "aucun worktree ... ne persiste après
    /// un cycle de crash + redémarrage": a worktree left behind by a crashed
    /// run (its `Worktree` guard never ran `Drop` — a crash is `SIGKILL`,
    /// not a graceful drop) must be removed as a side effect of the same
    /// recovery pass that marks the run `Failed`.
    #[tokio::test]
    async fn recovery_removes_an_orphaned_worktree_left_behind_by_a_crashed_run() {
        let repo = init_test_repo();
        let warden_home = TempDir::new().unwrap();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();

        let worktree_manager =
            WorktreeManager::new(repo.path(), warden_home.path().join("worktrees")).unwrap();
        // Simulates the crash itself: the `Worktree` guard is forgotten
        // rather than dropped or explicitly removed, exactly what a
        // SIGKILL'd orchestrator would leave behind.
        let worktree = worktree_manager
            .create("orphan-recovery-run", "coder", "HEAD")
            .await
            .unwrap();
        let worktree_path = worktree.path().to_path_buf();
        std::mem::forget(worktree);
        assert!(worktree_path.exists(), "precondition: worktree exists");

        db::insert_run(
            &pool,
            "orphan-recovery-run",
            &repo.path().display().to_string(),
            "main",
            "intent",
            3,
        )
        .await
        .unwrap();
        db::update_run_state(&pool, "orphan-recovery-run", RunState::CoderRunning)
            .await
            .unwrap();
        db::insert_cycle(&pool, "orphan-recovery-cycle", "orphan-recovery-run", 1)
            .await
            .unwrap();
        db::set_cycle_worktree_path(
            &pool,
            "orphan-recovery-cycle",
            AgentRole::Coder,
            &worktree_path.display().to_string(),
        )
        .await
        .unwrap();

        // Dead pid recorded for the coder, same as the other crash-recovery
        // tests: no live process, so this run must recover as `Failed`.
        let mut child = tokio::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let dead_pid = child.id().unwrap();
        child.wait().await.unwrap();
        db::insert_agent_process(
            &pool,
            "orphan-recovery-process",
            "orphan-recovery-cycle",
            AgentRole::Coder,
            dead_pid,
            &worktree_path.display().to_string(),
        )
        .await
        .unwrap();

        let failed = recover_crashed_runs(&pool).await.unwrap();
        assert_eq!(failed, vec!["orphan-recovery-run".to_string()]);

        assert!(
            !worktree_path.exists(),
            "orphaned worktree must be removed by crash recovery"
        );
    }

    /// Issue #6, acceptance criterion "aucun ... process orphelin ne
    /// persiste": an agent process still alive when its run is recovered as
    /// `Failed` must be terminated, and its `agent_processes` row marked
    /// ended, so it no longer looks like an in-flight process on the next
    /// recovery pass.
    #[tokio::test]
    async fn recovery_terminates_an_orphaned_live_agent_process() {
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();

        db::insert_run(
            &pool,
            "orphan-process-run",
            "/tmp/repo",
            "main",
            "intent",
            3,
        )
        .await
        .unwrap();
        db::update_run_state(&pool, "orphan-process-run", RunState::AwaitingReviewTest)
            .await
            .unwrap();
        db::insert_cycle(&pool, "orphan-process-cycle", "orphan-process-run", 1)
            .await
            .unwrap();

        // An *earlier* concurrent process (reviewer/tester run via
        // `tokio::join!`, ADR-0003) is still alive, but the run's *latest*
        // recorded process (inserted after it, so it sorts last by
        // `started_at` and is what drives the Failed decision, see
        // `latest_open_agent_process_for_run`) is dead -- exactly the shape
        // a crash mid-`AwaitingReviewTest` leaves behind.
        let mut live_child = tokio::process::Command::new("sh")
            .args(["-c", "sleep 30"])
            .spawn()
            .unwrap();
        let live_pid = live_child.id().unwrap();
        db::insert_agent_process(
            &pool,
            "orphan-process-live",
            "orphan-process-cycle",
            AgentRole::Tester,
            live_pid,
            "/tmp/wt/tester",
        )
        .await
        .unwrap();

        // Guarantees the dead process's `started_at` sorts strictly after
        // the live one's, so which row is "latest" is deterministic rather
        // than relying on two `now_rfc3339()` calls happening to differ.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;

        let mut dead_child = tokio::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let dead_pid = dead_child.id().unwrap();
        dead_child.wait().await.unwrap();
        db::insert_agent_process(
            &pool,
            "orphan-process-dead",
            "orphan-process-cycle",
            AgentRole::Reviewer,
            dead_pid,
            "/tmp/wt/reviewer",
        )
        .await
        .unwrap();

        let failed = recover_crashed_runs(&pool).await.unwrap();
        assert_eq!(failed, vec!["orphan-process-run".to_string()]);

        // The live process must actually be gone, not just marked ended in
        // the database.
        let exit_status = live_child.wait().await.unwrap();
        assert!(
            !exit_status.success(),
            "orphaned live process must have been killed by recovery"
        );

        let open_processes = db::list_open_agent_processes_for_run(&pool, "orphan-process-run")
            .await
            .unwrap();
        assert!(
            open_processes.is_empty(),
            "every agent_processes row for a Failed run must be marked ended by recovery"
        );
    }

    /// Acceptance criterion 1 (issue #2), exercised directly against
    /// `run_review_and_test` rather than through the full CLI: reviewer and
    /// tester each write to a DIFFERENT file in their own worktree, then
    /// (after a deliberate sleep long enough to overlap with the other
    /// role's run) read back the *other* role's target file from their own
    /// worktree. If the two roles ever shared a worktree/directory, the
    /// other role's write -- which completes well before the sleep ends --
    /// would already be visible here instead of the original, untouched
    /// content. This distinguishes "isolated worktrees" from "shared
    /// worktree" deterministically, regardless of exact interleaving.
    #[tokio::test]
    async fn run_review_and_test_isolates_concurrent_writes_to_different_worktree_files() {
        let repo = init_test_repo();
        std::fs::write(repo.path().join("review_target.txt"), "original-review\n").unwrap();
        std::fs::write(repo.path().join("test_target.txt"), "original-test\n").unwrap();
        let commit = |args: &[&str]| {
            let status = SyncCommand::new("git")
                .current_dir(repo.path())
                .args(args)
                .status()
                .expect("spawn git");
            assert!(status.success(), "git {args:?} failed");
        };
        commit(&["add", "."]);
        commit(&["commit", "--quiet", "-m", "add review/test targets"]);

        let warden_home = TempDir::new().unwrap();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let worktree_manager =
            WorktreeManager::new(repo.path(), warden_home.path().join("worktrees")).unwrap();

        db::insert_run(
            &pool,
            "collision-run",
            &repo.path().display().to_string(),
            "main",
            "crossed findings, no collision",
            3,
        )
        .await
        .unwrap();
        db::insert_cycle(&pool, "collision-cycle", "collision-run", 1)
            .await
            .unwrap();

        let reviewer_command = AgentCommand::new(
            "sh",
            [
                "-c",
                r#"
                echo modified-by-reviewer > review_target.txt
                sleep 0.3
                seen=$(cat test_target.txt)
                echo "{\"source\":\"reviewer\",\"severity\":\"info\",\"description\":\"review_target=modified-by-reviewer test_target_seen=$seen\"}"
                "#,
            ],
        );
        let tester_command = AgentCommand::new(
            "sh",
            [
                "-c",
                r#"
                echo modified-by-tester > test_target.txt
                sleep 0.3
                seen=$(cat review_target.txt)
                echo "{\"source\":\"tester\",\"severity\":\"info\",\"description\":\"test_target=modified-by-tester review_target_seen=$seen\"}"
                "#,
            ],
        );
        let config = RunConfig {
            repo_path: repo.path().to_path_buf(),
            warden_home: warden_home.path().to_path_buf(),
            branch: "main".to_string(),
            intent: "crossed findings, no collision".to_string(),
            max_cycles: 3,
            coder_command: AgentCommand::new("sh", ["-c", "true"]),
            reviewer_command,
            tester_command,
            evidence_tool: None,
            evidence_store_in_repo: true,
            gate: None,
        };

        let orchestrator = Orchestrator::new(pool.clone());
        let findings = orchestrator
            .run_review_and_test(
                "collision-run",
                "collision-cycle",
                1,
                &config,
                &worktree_manager,
                "HEAD",
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert_eq!(findings.len(), 2);
        let reviewer_finding = findings
            .iter()
            .find(|f| f.source == warden_core::FindingSource::Reviewer)
            .expect("reviewer finding present");
        let tester_finding = findings
            .iter()
            .find(|f| f.source == warden_core::FindingSource::Tester)
            .expect("tester finding present");

        assert!(
            reviewer_finding
                .description
                .contains("test_target_seen=original-test"),
            "reviewer's worktree must still see the untouched original \
             test_target.txt, not the tester's concurrent write -- got: {}",
            reviewer_finding.description
        );
        assert!(
            tester_finding
                .description
                .contains("review_target_seen=original-review"),
            "tester's worktree must still see the untouched original \
             review_target.txt, not the reviewer's concurrent write -- got: {}",
            tester_finding.description
        );
    }

    /// Acceptance criterion 2 (issue #2): "Temps de cycle mesurablement
    /// réduit par rapport à la Phase 1" -- reviewer and tester must run
    /// concurrently (`tokio::join!`), so total wall-clock time is dominated
    /// by the slower of the two, not their sum.
    ///
    /// Deliberately not a fixed wall-clock threshold (e.g. `elapsed <
    /// 1.5 * SLEEP`): under cargo's default parallel test harness, `git
    /// worktree add` contention and process-spawn overhead from other
    /// worktree-creating tests running at the same time can push a single
    /// absolute bound past its margin without anything actually being wrong
    /// -- non-deterministic per code-standards.md line 17. Instead, this
    /// measures a **sequential baseline through the exact same code path**
    /// (`run_finding_agent`, back-to-back: worktree create -> spawn -> wait
    /// -> worktree remove -> DB writes, for reviewer then tester) and
    /// compares it against the real `run_review_and_test` (`tokio::join!`)
    /// path measured immediately after, on the same machine/run. Both
    /// numbers absorb the same ambient overhead, so only their *ratio* is
    /// asserted on: sequential is ~2x SLEEP + overhead, parallel is ~1x
    /// SLEEP + overhead, so parallel must land under 75% of sequential
    /// regardless of how loaded the machine is.
    #[tokio::test]
    async fn run_review_and_test_runs_reviewer_and_tester_concurrently_not_sequentially() {
        let repo = init_test_repo();
        let warden_home = TempDir::new().unwrap();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let worktree_manager =
            WorktreeManager::new(repo.path(), warden_home.path().join("worktrees")).unwrap();

        let sleepy_agent = AgentCommand::new("sh", ["-c", "sleep 0.5"]);
        let config = RunConfig {
            repo_path: repo.path().to_path_buf(),
            warden_home: warden_home.path().to_path_buf(),
            branch: "main".to_string(),
            intent: "timing check".to_string(),
            max_cycles: 3,
            coder_command: AgentCommand::new("sh", ["-c", "true"]),
            reviewer_command: sleepy_agent.clone(),
            tester_command: sleepy_agent,
            evidence_tool: None,
            evidence_store_in_repo: true,
            gate: None,
        };

        let orchestrator = Orchestrator::new(pool.clone());

        // Sequential baseline: same real invocation (`run_finding_agent`)
        // used for reviewer then tester, awaited back-to-back rather than
        // concurrently. Uses its own run/cycle id so it doesn't share
        // worktree paths or DB rows with the parallel measurement below.
        db::insert_run(
            &pool,
            "sequential-run",
            &repo.path().display().to_string(),
            "main",
            "timing check",
            3,
        )
        .await
        .unwrap();
        db::insert_cycle(&pool, "sequential-cycle", "sequential-run", 1)
            .await
            .unwrap();

        let sequential_start = std::time::Instant::now();
        orchestrator
            .run_finding_agent(FindingAgentInvocation {
                run_id: "sequential-run",
                cycle_id: "sequential-cycle",
                cycle_number: 1,
                role: AgentRole::Reviewer,
                command: &config.reviewer_command,
                worktree_manager: &worktree_manager,
                commit: "HEAD",
                config: &config,
                cancel: CancellationToken::new(),
            })
            .await
            .unwrap();
        orchestrator
            .run_finding_agent(FindingAgentInvocation {
                run_id: "sequential-run",
                cycle_id: "sequential-cycle",
                cycle_number: 1,
                role: AgentRole::Tester,
                command: &config.tester_command,
                worktree_manager: &worktree_manager,
                commit: "HEAD",
                config: &config,
                cancel: CancellationToken::new(),
            })
            .await
            .unwrap();
        let sequential_elapsed = sequential_start.elapsed();

        // Parallel path: the real `run_review_and_test`, measured right
        // after, on the same machine/run as the baseline above.
        db::insert_run(
            &pool,
            "timing-run",
            &repo.path().display().to_string(),
            "main",
            "timing check",
            3,
        )
        .await
        .unwrap();
        db::insert_cycle(&pool, "timing-cycle", "timing-run", 1)
            .await
            .unwrap();

        let parallel_start = std::time::Instant::now();
        orchestrator
            .run_review_and_test(
                "timing-run",
                "timing-cycle",
                1,
                &config,
                &worktree_manager,
                "HEAD",
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let parallel_elapsed = parallel_start.elapsed();

        assert!(
            parallel_elapsed < sequential_elapsed.mul_f64(0.75),
            "expected the tokio::join! path ({parallel_elapsed:?}) to be \
             meaningfully faster than the sequential baseline \
             ({sequential_elapsed:?}) -- this looks like reviewer/tester ran \
             one after another instead of concurrently"
        );
    }

    /// Issue #6, H1 (PID-reuse hardening) exercised through the *full*
    /// recovery path, not just `process::kill_pid` in isolation: a run's
    /// recorded agent process has a `pid_started_at_unix` that no longer
    /// matches the process currently holding that PID (the OS reused it
    /// after the original process died) — recovery must still mark the run
    /// `Failed` and close out the stale `agent_processes` row, but must
    /// never signal the unrelated live process that now happens to hold
    /// that PID.
    #[tokio::test]
    async fn recovery_never_kills_a_live_process_whose_pid_fingerprint_no_longer_matches() {
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();

        db::insert_run(&pool, "pid-reuse-run", "/tmp/repo", "main", "intent", 3)
            .await
            .unwrap();
        db::update_run_state(&pool, "pid-reuse-run", RunState::CoderRunning)
            .await
            .unwrap();
        db::insert_cycle(&pool, "pid-reuse-cycle", "pid-reuse-run", 1)
            .await
            .unwrap();

        // A genuinely live process, standing in for "the OS handed this PID
        // to an unrelated process after the originally-recorded one died".
        let mut child = tokio::process::Command::new("sh")
            .args(["-c", "sleep 30"])
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        let real_start_time = process::process_start_time(pid).unwrap();
        let bogus_start_time = real_start_time + 1_000_000;

        // insert_agent_process derives the start time itself from the live
        // PID at insert time, so the row is written directly here instead,
        // with a fingerprint that deliberately does not match the process
        // currently alive at `pid`.
        sqlx::query!(
            "INSERT INTO agent_processes (id, cycle_id, role, pid, pid_started_at_unix, worktree_path, started_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
            "pid-reuse-process",
            "pid-reuse-cycle",
            "coder",
            pid,
            bogus_start_time,
            "/tmp/wt",
            "2020-01-01T00:00:00+00:00",
        )
        .execute(&pool)
        .await
        .unwrap();

        let failed = recover_crashed_runs(&pool).await.unwrap();
        assert_eq!(failed, vec!["pid-reuse-run".to_string()]);

        // The live process must be untouched: recovery believed the row was
        // "dead" (fingerprint mismatch), never a live process to leave
        // running, so it never called kill_pid on it at all.
        assert!(
            process::is_process_alive(pid, real_start_time),
            "a process whose PID was reused must never be killed by crash recovery"
        );

        // The stale row must still be closed out, so it doesn't keep
        // looking like an open process on the next recovery pass.
        let open_processes = db::list_open_agent_processes_for_run(&pool, "pid-reuse-run")
            .await
            .unwrap();
        assert!(
            open_processes.is_empty(),
            "the stale agent_processes row must be marked ended even though its process was never touched"
        );

        child.kill().await.unwrap();
    }

    /// Regression test for a real gap in `recover_crashed_runs`: the run's
    /// state is written as `Failed` to SQLite *before* its orphaned
    /// worktree/process cleanup runs (see the function body — `Failed` is
    /// persisted, then `cleanup_orphan_worktrees`/`terminate_orphan_processes`
    /// are attempted afterwards, best-effort). If the orchestrator process
    /// itself dies in that window -- a crash *during* recovery, e.g. the
    /// very next `SIGKILL` -- the run is already `Failed` by the time the
    /// process comes back up. `list_intermediate_runs` only looks at
    /// `coder_running`/`awaiting_review_test`/`awaiting_ci`, so a `Failed`
    /// run is never revisited by a later recovery pass, and its worktree
    /// and/or live process are never cleaned up again: a permanent leak,
    /// not merely a delayed cleanup.
    ///
    /// This test sets up exactly that already-crashed-mid-recovery state
    /// directly (run already `Failed`, orphan worktree still on disk, an
    /// agent process still recorded open and still alive) and asserts what
    /// issue #6 actually requires -- "aucun worktree ni process orphelin ne
    /// persiste après un cycle de crash + redémarrage" makes no exception
    /// for a run already marked `Failed`. As of this commit this FAILS
    /// against the current implementation, which silently skips `Failed`
    /// runs entirely: see the discrepancy reported alongside this test.
    #[tokio::test]
    async fn recovery_cleans_up_orphans_even_for_a_run_already_marked_failed_by_an_earlier_crashed_recovery_pass(
    ) {
        let repo = init_test_repo();
        let warden_home = TempDir::new().unwrap();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();

        let worktree_manager =
            WorktreeManager::new(repo.path(), warden_home.path().join("worktrees")).unwrap();
        let worktree = worktree_manager
            .create("crash-during-recovery-run", "coder", "HEAD")
            .await
            .unwrap();
        let worktree_path = worktree.path().to_path_buf();
        std::mem::forget(worktree);
        assert!(
            worktree_path.exists(),
            "precondition: orphan worktree exists"
        );

        db::insert_run(
            &pool,
            "crash-during-recovery-run",
            &repo.path().display().to_string(),
            "main",
            "intent",
            3,
        )
        .await
        .unwrap();
        db::insert_cycle(
            &pool,
            "crash-during-recovery-cycle",
            "crash-during-recovery-run",
            1,
        )
        .await
        .unwrap();
        db::set_cycle_worktree_path(
            &pool,
            "crash-during-recovery-cycle",
            AgentRole::Coder,
            &worktree_path.display().to_string(),
        )
        .await
        .unwrap();

        // A still-running agent process, recorded but never marked ended --
        // exactly the shape left behind if the *first* recovery pass died
        // right after writing `Failed` but before `terminate_orphan_processes`
        // ran.
        let mut child = tokio::process::Command::new("sh")
            .args(["-c", "sleep 30"])
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        db::insert_agent_process(
            &pool,
            "crash-during-recovery-process",
            "crash-during-recovery-cycle",
            AgentRole::Coder,
            pid,
            &worktree_path.display().to_string(),
        )
        .await
        .unwrap();

        // Simulates the first, interrupted recovery pass having already
        // committed the state transition (write-ahead of intention,
        // ADR-0004) before it crashed -- CoderRunning -> Failed is a valid
        // transition, so this mirrors exactly what
        // `recover_crashed_runs` itself would have written.
        db::update_run_state(&pool, "crash-during-recovery-run", RunState::Failed)
            .await
            .unwrap();

        // A second, successful recovery pass -- the restart after the crash
        // that hit mid-recovery.
        recover_crashed_runs(&pool).await.unwrap();

        // Kill the still-running child unconditionally, before asserting,
        // so this test never leaks a real background `sleep 30` process
        // regardless of whether the assertions below pass or (expectedly,
        // as of this commit) fail.
        let _ = child.kill().await;
        let _ = child.wait().await;

        assert!(
            !worktree_path.exists(),
            "BUG: a run already marked Failed by an interrupted recovery pass is never \
             revisited by list_intermediate_runs, so its orphan worktree is leaked forever, \
             not just cleaned up late"
        );

        let open_processes =
            db::list_open_agent_processes_for_run(&pool, "crash-during-recovery-run")
                .await
                .unwrap();
        assert!(
            open_processes.is_empty(),
            "BUG: a run already marked Failed by an interrupted recovery pass leaves its \
             agent_processes row open forever, and the process itself keeps running"
        );
    }

    /// Issue #6 (idempotency, MED): once a crashed run's cleanup has
    /// genuinely succeeded -- process terminated and its `agent_processes`
    /// row marked ended, worktree removed and its path cleared from
    /// `cycles` -- a *second* `recover_crashed_runs` call must find nothing
    /// left to reclaim for it: it must not be reported as newly failed
    /// again, and must not error or re-attempt work on resources that are
    /// already gone. Otherwise every restart would keep "reclaiming" the
    /// same, already-clean run forever.
    #[tokio::test]
    async fn second_recovery_pass_is_a_noop_once_a_failed_runs_cleanup_has_actually_succeeded() {
        let repo = init_test_repo();
        let warden_home = TempDir::new().unwrap();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();

        let worktree_manager =
            WorktreeManager::new(repo.path(), warden_home.path().join("worktrees")).unwrap();
        let worktree = worktree_manager
            .create("idempotent-recovery-run", "coder", "HEAD")
            .await
            .unwrap();
        let worktree_path = worktree.path().to_path_buf();
        std::mem::forget(worktree);

        db::insert_run(
            &pool,
            "idempotent-recovery-run",
            &repo.path().display().to_string(),
            "main",
            "intent",
            3,
        )
        .await
        .unwrap();
        db::update_run_state(&pool, "idempotent-recovery-run", RunState::CoderRunning)
            .await
            .unwrap();
        db::insert_cycle(
            &pool,
            "idempotent-recovery-cycle",
            "idempotent-recovery-run",
            1,
        )
        .await
        .unwrap();
        db::set_cycle_worktree_path(
            &pool,
            "idempotent-recovery-cycle",
            AgentRole::Coder,
            &worktree_path.display().to_string(),
        )
        .await
        .unwrap();

        let mut child = tokio::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let dead_pid = child.id().unwrap();
        child.wait().await.unwrap();
        db::insert_agent_process(
            &pool,
            "idempotent-recovery-process",
            "idempotent-recovery-cycle",
            AgentRole::Coder,
            dead_pid,
            &worktree_path.display().to_string(),
        )
        .await
        .unwrap();

        // First pass: transitions the run to Failed and fully reclaims both
        // the worktree and the process.
        let failed = recover_crashed_runs(&pool).await.unwrap();
        assert_eq!(failed, vec!["idempotent-recovery-run".to_string()]);
        assert!(
            !worktree_path.exists(),
            "precondition: first pass must actually remove the worktree"
        );

        // Precondition for the real assertion below: cleanup actually
        // finished (nothing left recorded), not just that the run was
        // marked Failed -- otherwise this test would not exercise
        // idempotency at all.
        let pending_after_first_pass = db::list_failed_runs_with_pending_cleanup(&pool)
            .await
            .unwrap();
        assert!(
            pending_after_first_pass.is_empty(),
            "precondition: first pass must leave nothing pending"
        );

        // Second pass -- simulates the next CLI invocation / restart
        // finding this already-Failed, already-clean run again. It must
        // not be reported as newly failed (that already happened on the
        // first pass), and must complete without error even though there
        // is nothing left to reclaim.
        let failed_again = recover_crashed_runs(&pool).await.unwrap();
        assert!(
            failed_again.is_empty(),
            "a run already Failed with nothing pending must not be reported as newly \
             failed again"
        );

        let run = db::get_run(&pool, "idempotent-recovery-run")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.state, RunState::Failed);
    }

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
            max_cycles: 3,
            coder_command: AgentCommand::new(
                "sh",
                [
                    "-c",
                    "echo hi >> notes.txt && git add notes.txt && git -c user.email=t@w.local -c user.name=w commit -q -m cycle",
                ],
            ),
            reviewer_command: AgentCommand::new("sh", ["-c", "true"]),
            tester_command: always_passing_tester(),
            evidence_tool: None,
            evidence_store_in_repo: true,
            gate: None,
        };

        let (run_id, final_state) = orchestrator
            .run_convergence_loop(config, CancellationToken::new())
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

    // ---- Issue #15/ADR-0011: post-Converged tail -------------------------

    /// A local bare repo standing in for `warden-gated`'s own bare gate repo
    /// (ADR-0002) -- real git, no network, so `push_converged_commit_to_bare_repo`
    /// exercises an actual `git push` (code-standards.md: "pas d'appel
    /// réseau externe").
    fn init_bare_repo_fixture() -> TempDir {
        let dir = TempDir::new().expect("tempdir");
        let status = SyncCommand::new("git")
            .current_dir(dir.path())
            .args(["init", "--bare", "--quiet"])
            .status()
            .expect("spawn git");
        assert!(status.success());
        dir
    }

    /// A [`GateTrigger`] fake that delivers a scripted [`CiResultMessage`]
    /// synchronously within `trigger_run_tail`/`trigger_resume_watch`,
    /// standing in for `warden-gated`'s real subprocess (which would need a
    /// live `gh`/GitHub PR, code-standards.md: "pas d'appel réseau
    /// externe"). Connecting before the caller's own `listener.receive()`
    /// is safe: a Unix listener's accept backlog holds the connection
    /// regardless of `accept()` timing.
    struct FakeGateTrigger {
        outcome: warden_core::CiWatchOutcome,
        pr_number: Option<u64>,
    }

    impl GateTrigger for FakeGateTrigger {
        async fn trigger_run_tail(&self, request: &RunTailTrigger<'_>) -> Result<()> {
            self.deliver(request.run_id, request.ci_result_socket).await
        }

        async fn trigger_resume_watch(
            &self,
            run_id: &str,
            _pr_number: u64,
            ci_result_socket: &Path,
        ) -> Result<()> {
            self.deliver(run_id, ci_result_socket).await
        }
    }

    impl FakeGateTrigger {
        async fn deliver(&self, run_id: &str, ci_result_socket: &Path) -> Result<()> {
            use tokio::io::AsyncWriteExt;

            let message = CiResultMessage {
                run_id: run_id.to_string(),
                pr_number: self.pr_number,
                outcome: self.outcome.clone(),
            };
            let json = message.to_json()?;
            let mut stream = tokio::net::UnixStream::connect(ci_result_socket).await?;
            stream.write_all(json.as_bytes()).await?;
            stream.shutdown().await?;
            Ok(())
        }
    }

    /// Builds a run already sitting in `Converged` (the state
    /// `drive_post_convergence_tail`'s first transition, `-> Pushed`,
    /// requires) with a real commit -- `init_test_repo`'s own initial
    /// commit -- reachable in `repo_path`'s object store, so
    /// `push_converged_commit_to_bare_repo` has something real to push.
    async fn converged_run_fixture(
        pool: &SqlitePool,
        repo: &TempDir,
        bare_repo: &TempDir,
    ) -> (String, RunConfig, String) {
        let run_id = Uuid::new_v4().to_string();
        db::insert_run(pool, &run_id, "/tmp/repo", "main", "intent", 5)
            .await
            .unwrap();
        db::update_run_state(pool, &run_id, RunState::CoderRunning)
            .await
            .unwrap();
        db::update_run_state(pool, &run_id, RunState::AwaitingReviewTest)
            .await
            .unwrap();
        db::update_run_state(pool, &run_id, RunState::Converged)
            .await
            .unwrap();

        let head_output = SyncCommand::new("git")
            .current_dir(repo.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let converged_commit = String::from_utf8_lossy(&head_output.stdout)
            .trim()
            .to_string();
        db::set_run_converged_commit(pool, &run_id, &converged_commit)
            .await
            .unwrap();

        let warden_home = TempDir::new().unwrap();
        let config = RunConfig {
            repo_path: repo.path().to_path_buf(),
            warden_home: warden_home.path().to_path_buf(),
            branch: "main".to_string(),
            intent: "intent".to_string(),
            max_cycles: 5,
            coder_command: AgentCommand::new("sh", ["-c", "true"]),
            reviewer_command: AgentCommand::new("sh", ["-c", "true"]),
            tester_command: AgentCommand::new("sh", ["-c", "true"]),
            evidence_tool: None,
            evidence_store_in_repo: false,
            gate: Some(GateConfig {
                bare_repo_path: bare_repo.path().to_path_buf(),
                gated_bin: PathBuf::from("/unused/in/this/test"),
                repo_slug: None,
                poll_interval_secs: 1,
                inactivity_timeout_secs: 3600,
            }),
        };
        // Leaked deliberately: `warden_home`'s TempDir must outlive the
        // `CiResultListener` bound inside it for the duration of this test,
        // and giving each test its own leaked TempDir is simpler than
        // threading an extra return value through every caller.
        std::mem::forget(warden_home);

        (run_id, config, converged_commit)
    }

    #[tokio::test]
    async fn drive_post_convergence_tail_reaches_done_on_checks_passed() {
        let repo = init_test_repo();
        let bare_repo = init_bare_repo_fixture();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let (run_id, config, converged_commit) =
            converged_run_fixture(&pool, &repo, &bare_repo).await;

        let orchestrator = Orchestrator::new(pool.clone());
        let trigger = FakeGateTrigger {
            outcome: warden_core::CiWatchOutcome::checks_passed(),
            pr_number: Some(42),
        };

        let outcome = orchestrator
            .drive_post_convergence_tail(&run_id, &config, &converged_commit, &trigger)
            .await
            .unwrap();

        assert!(matches!(
            outcome,
            PostConvergenceOutcome::Terminal(RunState::Done)
        ));
        let run = db::get_run(&pool, &run_id).await.unwrap().unwrap();
        assert_eq!(run.state, RunState::Done);
        assert_eq!(run.pr_number, Some(42));
    }

    /// Issue #15 review, H2: the staged commit must land under
    /// `refs/warden-staging/`, never under `refs/heads/warden-run/` (the ref
    /// `warden_gated::notification::parse_post_receive_line`/`serve.rs`
    /// watch for a push-notification) -- otherwise a deployed gate (hook +
    /// `serve` daemon) would independently re-verify and force-push this
    /// business content straight to `origin/<target_branch>`, bypassing the
    /// PR review flow entirely.
    #[tokio::test]
    async fn drive_post_convergence_tail_stages_the_commit_outside_the_notify_hooks_ref_namespace()
    {
        let repo = init_test_repo();
        let bare_repo = init_bare_repo_fixture();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let (run_id, config, converged_commit) =
            converged_run_fixture(&pool, &repo, &bare_repo).await;

        let orchestrator = Orchestrator::new(pool.clone());
        let trigger = FakeGateTrigger {
            outcome: warden_core::CiWatchOutcome::checks_passed(),
            pr_number: Some(42),
        };
        orchestrator
            .drive_post_convergence_tail(&run_id, &config, &converged_commit, &trigger)
            .await
            .unwrap();

        let staging_ref_check = SyncCommand::new("git")
            .current_dir(bare_repo.path())
            .args([
                "rev-parse",
                "--verify",
                &format!("refs/warden-staging/{run_id}"),
            ])
            .output()
            .unwrap();
        assert!(
            staging_ref_check.status.success(),
            "the converged commit must be staged under refs/warden-staging/<run_id>"
        );

        let notify_ref_check = SyncCommand::new("git")
            .current_dir(bare_repo.path())
            .args([
                "rev-parse",
                "--verify",
                &format!("refs/heads/warden-run/{run_id}"),
            ])
            .output()
            .unwrap();
        assert!(
            !notify_ref_check.status.success(),
            "the converged commit must NOT be staged under refs/heads/warden-run/<run_id> -- \
             that ref is what the notify hook/serve daemon watch for a push-notification, and \
             would auto-push this content straight to origin on a deployed gate"
        );
    }

    #[tokio::test]
    async fn drive_post_convergence_tail_reboucles_to_coder_running_with_ci_findings_on_checks_failed(
    ) {
        let repo = init_test_repo();
        let bare_repo = init_bare_repo_fixture();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let (run_id, config, converged_commit) =
            converged_run_fixture(&pool, &repo, &bare_repo).await;

        let orchestrator = Orchestrator::new(pool.clone());
        let ci_finding = Finding {
            source: warden_core::FindingSource::Ci,
            severity: warden_core::Severity::Blocking,
            file: None,
            description: "build failed".to_string(),
            action: None,
        };
        let trigger = FakeGateTrigger {
            outcome: warden_core::CiWatchOutcome::checks_failed(&[ci_finding]),
            pr_number: Some(7),
        };

        let outcome = orchestrator
            .drive_post_convergence_tail(&run_id, &config, &converged_commit, &trigger)
            .await
            .unwrap();

        match outcome {
            PostConvergenceOutcome::Reboucle { findings } => {
                assert_eq!(findings.len(), 1);
                assert_eq!(findings[0].source, warden_core::FindingSource::Ci);
                assert_eq!(findings[0].description, "build failed");
            }
            other => panic!("expected Reboucle, got {other:?}"),
        }
        let run = db::get_run(&pool, &run_id).await.unwrap().unwrap();
        assert_eq!(run.state, RunState::CoderRunning);
    }

    #[tokio::test]
    async fn drive_post_convergence_tail_maps_gate_failed_to_failed() {
        let repo = init_test_repo();
        let bare_repo = init_bare_repo_fixture();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let (run_id, config, converged_commit) =
            converged_run_fixture(&pool, &repo, &bare_repo).await;

        let orchestrator = Orchestrator::new(pool.clone());
        let trigger = FakeGateTrigger {
            outcome: warden_core::CiWatchOutcome::gate_failed("skeleton push failed"),
            pr_number: None,
        };

        let outcome = orchestrator
            .drive_post_convergence_tail(&run_id, &config, &converged_commit, &trigger)
            .await
            .unwrap();

        assert!(matches!(
            outcome,
            PostConvergenceOutcome::Terminal(RunState::Failed)
        ));
        let run = db::get_run(&pool, &run_id).await.unwrap().unwrap();
        assert_eq!(run.state, RunState::Failed);
    }

    /// ADR-0011's idempotency guard: a run that has already left
    /// `AwaitingCi` (e.g. a duplicate/stale delivery racing an earlier one)
    /// must not have its state clobbered by a second `CiResultMessage` --
    /// this is a safe no-op, never an error.
    #[tokio::test]
    async fn apply_ci_result_message_is_a_noop_once_the_run_already_left_awaiting_ci() {
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let run_id = Uuid::new_v4().to_string();
        db::insert_run(&pool, &run_id, "/tmp/repo", "main", "intent", 5)
            .await
            .unwrap();
        db::update_run_state(&pool, &run_id, RunState::CoderRunning)
            .await
            .unwrap();
        db::update_run_state(&pool, &run_id, RunState::AwaitingReviewTest)
            .await
            .unwrap();
        db::update_run_state(&pool, &run_id, RunState::Converged)
            .await
            .unwrap();
        db::update_run_state(&pool, &run_id, RunState::Pushed)
            .await
            .unwrap();
        db::update_run_state(&pool, &run_id, RunState::AwaitingCi)
            .await
            .unwrap();
        // Already left AwaitingCi by the time this (stale/duplicate)
        // message is applied.
        db::update_run_state(&pool, &run_id, RunState::Done)
            .await
            .unwrap();

        let orchestrator = Orchestrator::new(pool.clone());
        let message = CiResultMessage {
            run_id: run_id.clone(),
            pr_number: Some(99),
            outcome: warden_core::CiWatchOutcome::checks_passed(),
        };

        let outcome = orchestrator
            .apply_ci_result_message(&run_id, &message)
            .await
            .unwrap();

        assert!(matches!(
            outcome,
            PostConvergenceOutcome::Terminal(RunState::Done)
        ));
        let run = db::get_run(&pool, &run_id).await.unwrap().unwrap();
        assert_eq!(run.state, RunState::Done);
        assert_eq!(
            run.pr_number, None,
            "a stale delivery must not even record its pr_number once ignored"
        );
    }

    // ---- Issue #15/ADR-0011: crash-recovery resume of AwaitingCi ---------

    /// The bug `recover_crashed_runs` alone would have: `AwaitingCi` has no
    /// live *agent* process to find (it's waiting on `warden-gated`, not an
    /// `agent_processes` row), so the blanket "no live process -> Failed"
    /// rule would incorrectly fail it. Confirms it's left untouched instead.
    #[tokio::test]
    async fn recover_crashed_runs_leaves_awaiting_ci_runs_untouched() {
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();

        db::insert_run(&pool, "run-ci", "/tmp/repo", "main", "intent", 3)
            .await
            .unwrap();
        db::update_run_state(&pool, "run-ci", RunState::CoderRunning)
            .await
            .unwrap();
        db::update_run_state(&pool, "run-ci", RunState::AwaitingReviewTest)
            .await
            .unwrap();
        db::update_run_state(&pool, "run-ci", RunState::Converged)
            .await
            .unwrap();
        db::update_run_state(&pool, "run-ci", RunState::Pushed)
            .await
            .unwrap();
        db::update_run_state(&pool, "run-ci", RunState::AwaitingCi)
            .await
            .unwrap();

        let failed = recover_crashed_runs(&pool).await.unwrap();

        assert!(
            failed.is_empty(),
            "AwaitingCi must never be marked Failed by recover_crashed_runs"
        );
        let run = db::get_run(&pool, "run-ci").await.unwrap().unwrap();
        assert_eq!(run.state, RunState::AwaitingCi);
    }

    #[tokio::test]
    async fn resume_awaiting_ci_runs_resumes_the_watch_and_applies_its_outcome() {
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let warden_home = TempDir::new().unwrap();

        db::insert_run(&pool, "run-ci", "/tmp/repo", "main", "intent", 3)
            .await
            .unwrap();
        for state in [
            RunState::CoderRunning,
            RunState::AwaitingReviewTest,
            RunState::Converged,
            RunState::Pushed,
            RunState::AwaitingCi,
        ] {
            db::update_run_state(&pool, "run-ci", state).await.unwrap();
        }
        db::set_run_pr_number(&pool, "run-ci", 42).await.unwrap();

        let trigger = FakeGateTrigger {
            outcome: warden_core::CiWatchOutcome::merged(),
            pr_number: Some(42),
        };

        let resumed = resume_awaiting_ci_runs(&pool, warden_home.path(), &trigger)
            .await
            .unwrap();

        assert_eq!(resumed, vec!["run-ci".to_string()]);
        let run = db::get_run(&pool, "run-ci").await.unwrap().unwrap();
        assert_eq!(run.state, RunState::Done);
    }

    /// A run crashed before `OpenDraft` ever returned a PR number: there is
    /// nothing to resume watching, so this must fail the run rather than
    /// hang forever waiting for a watch that was never started.
    #[tokio::test]
    async fn resume_awaiting_ci_runs_fails_a_run_with_no_recorded_pr_number() {
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let warden_home = TempDir::new().unwrap();

        db::insert_run(&pool, "run-no-pr", "/tmp/repo", "main", "intent", 3)
            .await
            .unwrap();
        for state in [
            RunState::CoderRunning,
            RunState::AwaitingReviewTest,
            RunState::Converged,
            RunState::Pushed,
            RunState::AwaitingCi,
        ] {
            db::update_run_state(&pool, "run-no-pr", state)
                .await
                .unwrap();
        }

        let trigger = FakeGateTrigger {
            outcome: warden_core::CiWatchOutcome::merged(),
            pr_number: None,
        };

        let resumed = resume_awaiting_ci_runs(&pool, warden_home.path(), &trigger)
            .await
            .unwrap();

        assert_eq!(resumed, vec!["run-no-pr".to_string()]);
        let run = db::get_run(&pool, "run-no-pr").await.unwrap().unwrap();
        assert_eq!(run.state, RunState::Failed);
    }

    // ---- Issue #15/ADR-0011: deeper coverage added by the tester agent ---

    /// A [`GateTrigger`] that independently re-reads `run_id`'s persisted
    /// state from its own pool handle -- the same posture `warden-gated`
    /// itself takes (ADR-0006: never trust the caller, re-verify from
    /// SQLite) -- rather than trusting that `drive_post_convergence_tail`
    /// merely *called* things in the right order. Proves the tail's state
    /// transitions are durably persisted in order, not just issued in order:
    /// `trigger_run_tail` snapshots whatever is persisted the instant it's
    /// invoked (must already be `Pushed`), and delivery of the terminal
    /// message is deliberately deferred until this trigger independently
    /// observes `AwaitingCi` persisted -- so a successful delivery is only
    /// possible if `AwaitingCi` was written to SQLite first.
    struct RecordingGateTrigger {
        pool: SqlitePool,
        run_id: String,
        outcome: warden_core::CiWatchOutcome,
        pr_number: Option<u64>,
        observed_state_at_trigger: std::sync::Mutex<Option<RunState>>,
    }

    impl GateTrigger for RecordingGateTrigger {
        async fn trigger_run_tail(&self, request: &RunTailTrigger<'_>) -> Result<()> {
            let run = db::get_run(&self.pool, &self.run_id)
                .await
                .unwrap()
                .unwrap();
            *self.observed_state_at_trigger.lock().unwrap() = Some(run.state);

            let pool = self.pool.clone();
            let run_id = self.run_id.clone();
            let socket_path = request.ci_result_socket.to_path_buf();
            let message = CiResultMessage {
                run_id: request.run_id.to_string(),
                pr_number: self.pr_number,
                outcome: self.outcome.clone(),
            };
            tokio::spawn(async move {
                // Bounded poll (real sleep used only as inter-task
                // synchronization, never as a correctness assertion) for
                // this trigger's own independent view of `run_id` to reach
                // `AwaitingCi` before delivering -- caps at ~1s so a genuine
                // regression (the transition never gets persisted) fails
                // the test instead of hanging it.
                for _ in 0..200 {
                    let run = db::get_run(&pool, &run_id).await.unwrap().unwrap();
                    if run.state == RunState::AwaitingCi {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                let run = db::get_run(&pool, &run_id).await.unwrap().unwrap();
                assert_eq!(
                    run.state,
                    RunState::AwaitingCi,
                    "gave up waiting for AwaitingCi to be persisted before delivering"
                );

                use tokio::io::AsyncWriteExt;
                let json = message.to_json().unwrap();
                let mut stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
                stream.write_all(json.as_bytes()).await.unwrap();
                stream.shutdown().await.unwrap();
            });
            Ok(())
        }

        async fn trigger_resume_watch(
            &self,
            _run_id: &str,
            _pr_number: u64,
            _ci_result_socket: &Path,
        ) -> Result<()> {
            unreachable!("resume-watch is not exercised by this test")
        }
    }

    #[tokio::test]
    async fn drive_post_convergence_tail_persists_pushed_then_awaiting_ci_before_the_terminal_message_is_ever_applied(
    ) {
        let repo = init_test_repo();
        let bare_repo = init_bare_repo_fixture();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let (run_id, config, converged_commit) =
            converged_run_fixture(&pool, &repo, &bare_repo).await;

        let orchestrator = Orchestrator::new(pool.clone());
        let trigger = RecordingGateTrigger {
            pool: pool.clone(),
            run_id: run_id.clone(),
            outcome: warden_core::CiWatchOutcome::checks_passed(),
            pr_number: Some(11),
            observed_state_at_trigger: std::sync::Mutex::new(None),
        };

        let outcome = orchestrator
            .drive_post_convergence_tail(&run_id, &config, &converged_commit, &trigger)
            .await
            .unwrap();

        assert_eq!(
            *trigger.observed_state_at_trigger.lock().unwrap(),
            Some(RunState::Pushed),
            "Pushed must be durably persisted before the watch is even triggered"
        );
        assert!(matches!(
            outcome,
            PostConvergenceOutcome::Terminal(RunState::Done)
        ));
        let run = db::get_run(&pool, &run_id).await.unwrap().unwrap();
        assert_eq!(run.state, RunState::Done);
    }

    /// `ChecksFailed` at the cycle budget must reach `Failed`, never
    /// `MaxCyclesExceeded` -- that transition is illegal from `AwaitingCi`
    /// ([`RunState::validate_transition`]); if `decide_next_state_after_ci`
    /// or its caller ever regressed to returning it here, `self.transition`
    /// would reject it and this test would fail loudly rather than silently
    /// accept a corrupted state.
    #[tokio::test]
    async fn drive_post_convergence_tail_maps_checks_failed_at_cycle_budget_to_failed_not_max_cycles_exceeded(
    ) {
        let repo = init_test_repo();
        let bare_repo = init_bare_repo_fixture();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let (run_id, config, converged_commit) =
            converged_run_fixture(&pool, &repo, &bare_repo).await;
        // `converged_run_fixture` inserts with max_cycles = 5 and leaves
        // current_cycle at its 0 default; set it to the budget so this
        // `ChecksFailed` lands exactly at the limit.
        db::set_run_current_cycle(&pool, &run_id, 5).await.unwrap();

        let orchestrator = Orchestrator::new(pool.clone());
        let ci_finding = Finding {
            source: warden_core::FindingSource::Ci,
            severity: warden_core::Severity::Blocking,
            file: None,
            description: "flaky test at budget".to_string(),
            action: None,
        };
        let trigger = FakeGateTrigger {
            outcome: warden_core::CiWatchOutcome::checks_failed(&[ci_finding]),
            pr_number: Some(13),
        };

        let outcome = orchestrator
            .drive_post_convergence_tail(&run_id, &config, &converged_commit, &trigger)
            .await
            .unwrap();

        assert!(
            matches!(outcome, PostConvergenceOutcome::Terminal(RunState::Failed)),
            "expected Terminal(Failed), got {outcome:?}"
        );
        let run = db::get_run(&pool, &run_id).await.unwrap().unwrap();
        assert_eq!(run.state, RunState::Failed);
    }

    /// `Closed` (PR closed without merging) reaches `Failed` -- verified at
    /// the orchestrator level (real DB, real push, real socket), not just
    /// the pure `decide_next_state_after_ci` unit test.
    #[tokio::test]
    async fn drive_post_convergence_tail_maps_closed_to_failed() {
        let repo = init_test_repo();
        let bare_repo = init_bare_repo_fixture();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let (run_id, config, converged_commit) =
            converged_run_fixture(&pool, &repo, &bare_repo).await;

        let orchestrator = Orchestrator::new(pool.clone());
        let trigger = FakeGateTrigger {
            outcome: warden_core::CiWatchOutcome::closed(),
            pr_number: Some(21),
        };

        let outcome = orchestrator
            .drive_post_convergence_tail(&run_id, &config, &converged_commit, &trigger)
            .await
            .unwrap();

        assert!(matches!(
            outcome,
            PostConvergenceOutcome::Terminal(RunState::Failed)
        ));
        let run = db::get_run(&pool, &run_id).await.unwrap().unwrap();
        assert_eq!(run.state, RunState::Failed);
    }

    /// `TimedOut` (inactivity timeout inside `watch_pr`) reaches `Failed` --
    /// verified at the orchestrator level, mirroring the `Closed` case above.
    #[tokio::test]
    async fn drive_post_convergence_tail_maps_timed_out_to_failed() {
        let repo = init_test_repo();
        let bare_repo = init_bare_repo_fixture();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let (run_id, config, converged_commit) =
            converged_run_fixture(&pool, &repo, &bare_repo).await;

        let orchestrator = Orchestrator::new(pool.clone());
        let trigger = FakeGateTrigger {
            outcome: warden_core::CiWatchOutcome::timed_out(),
            pr_number: Some(22),
        };

        let outcome = orchestrator
            .drive_post_convergence_tail(&run_id, &config, &converged_commit, &trigger)
            .await
            .unwrap();

        assert!(matches!(
            outcome,
            PostConvergenceOutcome::Terminal(RunState::Failed)
        ));
        let run = db::get_run(&pool, &run_id).await.unwrap().unwrap();
        assert_eq!(run.state, RunState::Failed);
    }
}
