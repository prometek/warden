//! Issue #15/ADR-0011's post-`Converged` tail: push into the local bare
//! gate repo, trigger `warden-gated`'s `run-tail`, and await its terminal
//! CI result.

use super::*;

/// Issue #15 review, M-new-1: once the triggered `warden-gated` subprocess is
/// observed to have exited, how long `warden` still waits for its terminal CI
/// message to arrive over the reverse socket before concluding the child died
/// without delivering. `warden-gated` writes the message and *then* exits, so
/// on a local Unix socket the bytes are already buffered by the time the exit
/// is observed -- this grace only covers the tiny window between the two, and
/// is never the primary bound (a *live* child is waited on with no wall-clock
/// cap at all, since `watch_pr`'s runtime is legitimately uncapped).
const GATE_CHILD_GRACE_PERIOD: Duration = Duration::from_secs(2);

/// The ref prefix `warden` stages a converged run's commit under in the
/// local bare gate repo (issue #15 review, H2).
///
/// **Deliberately outside `refs/heads/`, and specifically NOT
/// `refs/heads/warden-run/`** (`warden_gated::notification::GATE_REF_PREFIX`,
/// the ref the installed `post-receive` hook / `serve` daemon watch for a
/// push-notification): this push exists *only* to transfer git objects into
/// the bare repo's object store so `warden-gated run-tail`/`resume-watch`
/// can find `commit_sha` by SHA (ADR-0002). It must never be treated as a
/// push-notification -- staging under `GATE_REF_PREFIX` would make a
/// *deployed* gate independently re-verify and force-push this content
/// straight to `origin/<target_branch>`, bypassing PR review entirely
/// (exactly what ADR-0002/issue #5 forbid). Only the PR-based path
/// (`run_tail`/`Finalize`) ever pushes this content on to real `origin`.
const GATE_STAGING_REF_PREFIX: &str = "refs/warden-staging/";

/// Issue #15 review, M2: reads back every evidence row captured across
/// `run_id`'s cycles and converts it into the shared wire shape
/// `gate_trigger::RunTailTrigger::evidence` carries -- previously never
/// read here at all, so the finalized PR body's Evidence section (ADR-0009)
/// was always empty regardless of what the run actually captured.
async fn evidence_rows_for_run(
    pool: &SqlitePool,
    run_id: &str,
) -> Result<Vec<warden_core::EvidenceRow>> {
    let rows = db::list_evidence_for_run(pool, run_id).await?;
    Ok(rows
        .into_iter()
        .map(|row| warden_core::EvidenceRow {
            cycle_number: row.cycle_number,
            evidence_type: row.evidence.evidence_type,
            repo_relative_path: row.evidence.file_path,
            description: row.evidence.description,
        })
        .collect())
}

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
    // `NO_HOST_HOOKS` (issue #49 review, HIGH): `git push` runs `pre-push`
    // in the *pushing* repo (`repo_path`, `-C` above) -- under `--isolation
    // docker`, `repo_path`'s `.git` is bind-mounted read-write into the
    // container (`warden_sandbox::docker`'s own docs), so an agent could
    // plant a `pre-push` hook there and have it run here, on the host, with
    // full host credentials/network access. See `crate::git_util`'s own
    // docs.
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(NO_HOST_HOOKS)
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

/// Waits for `warden-gated`'s single terminal CI message on `listener`,
/// bounded by `gate_child`'s liveness (issue #15 review, M-new-1) rather than
/// any wall-clock timeout. A live child means the watch is still legitimately
/// in progress, so `receive_no_timeout` is awaited with no cap of `warden`'s
/// own; only if the child exits *without* delivering -- and a short grace for
/// an in-flight message elapses -- is this a
/// [`WardenError::GateChildDiedWithoutResult`].
async fn await_ci_result(
    run_id: &str,
    listener: &CiResultListener,
    gate_child: GateChild,
) -> Result<CiResultMessage> {
    tokio::select! {
        biased;
        // Polled first: a delivered (or malformed) message always wins over
        // the child-exit branch, including a message that lands during the
        // grace period below.
        result = listener.receive_no_timeout() => result,
        () = wait_child_then_grace(gate_child) => {
            Err(WardenError::GateChildDiedWithoutResult {
                run_id: run_id.to_string(),
            })
        }
    }
}

/// Resolves once the triggered child has exited *and* [`GATE_CHILD_GRACE_PERIOD`]
/// has since elapsed -- the grace covers the tiny window between
/// `warden-gated` writing its final message and its process being observed to
/// exit. The concurrently-awaited `receive_no_timeout` keeps running
/// throughout, so a message that lands during the grace still wins.
async fn wait_child_then_grace(gate_child: GateChild) {
    gate_child.wait_exit().await;
    tokio::time::sleep(GATE_CHILD_GRACE_PERIOD).await;
}

/// Best-effort removal of a run's staging ref from the bare gate repo once the
/// run is terminal (issue #15 review, L-new-1) -- the ref is force-pushed on
/// every pass and would otherwise accumulate (pinning objects) unbounded.
/// Never propagates: failing to reclaim it must not fail an otherwise-finished
/// run, and a lingering ref is harmless until a later GC.
pub(super) async fn delete_gate_staging_ref(bare_repo_path: &Path, run_id: &str) {
    // No `NO_HOST_HOOKS` needed here (issue #49 review, HIGH): unlike
    // `repo_path` or a role's own worktree, `bare_repo_path` (the local gate
    // repo) is never bind-mounted into any sandbox and no agent process ever
    // writes to it -- there is no hook an agent could have planted in its
    // `.git` in the first place. See `crate::git_util`'s own docs.
    let ref_name = format!("{GATE_STAGING_REF_PREFIX}{run_id}");
    let result = tokio::process::Command::new("git")
        .arg("-C")
        .arg(bare_repo_path)
        .args(["update-ref", "-d", &ref_name])
        .output()
        .await;
    match result {
        Ok(output) if output.status.success() => {}
        Ok(output) => tracing::debug!(
            run_id,
            stderr = %String::from_utf8_lossy(&output.stderr),
            "failed to delete the gate staging ref on a terminal outcome (best-effort)"
        ),
        Err(error) => tracing::debug!(
            run_id,
            %error,
            "failed to run git to delete the gate staging ref (best-effort)"
        ),
    }
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
pub(super) async fn protect_cycle_commit(
    main_repo_path: &Path,
    run_id: &str,
    cycle_number: u32,
    commit_sha: &str,
) -> Result<()> {
    let ref_name = format!("refs/warden/runs/{run_id}/cycle-{cycle_number}");
    // `NO_HOST_HOOKS` (issue #49 review, HIGH): `update-ref` runs the
    // `reference-transaction` hook -- see `crate::git_util`'s own docs.
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(main_repo_path)
        .args(NO_HOST_HOOKS)
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

impl Orchestrator {
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
    pub(super) async fn drive_post_convergence_tail<G: GateTrigger>(
        &self,
        run_id: &str,
        config: &RunConfig,
        converged_commit: &str,
        trigger: &G,
    ) -> Result<PostConvergenceOutcome> {
        let Some(gate_config) = &config.gate else {
            unreachable!("drive_post_convergence_tail is only called when config.gate is Some");
        };

        // Issue #15 review, H3: a reboucle re-enters this method for a run
        // that already has a PR (a prior pass through this same method
        // already set it) -- `Some` here skips `OpenDraft` on the
        // `warden-gated` side entirely.
        let existing_pr_number = db::get_run(&self.pool, run_id)
            .await?
            .and_then(|run| run.pr_number);
        // Issue #15 review, M2: folded into the finalized PR body's
        // Evidence section (ADR-0009) -- previously hardcoded to empty on
        // the `warden-gated` side of this boundary.
        let evidence = evidence_rows_for_run(&self.pool, run_id).await?;

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
        let gate_child = trigger
            .trigger_run_tail(&RunTailTrigger {
                run_id,
                branch: &branch,
                base_branch: &config.branch,
                intent: &config.intent,
                pushed_commit_sha: converged_commit,
                summary_body: &summary_body,
                ci_result_socket: listener.socket_path(),
                evidence: &evidence,
                existing_pr_number,
            })
            .await?;

        self.transition(run_id, RunState::AwaitingCi).await?;

        let outcome = self
            .await_and_apply_ci_result(run_id, &listener, gate_child)
            .await?;

        // Issue #15 review, L-new-1: the per-run staging ref
        // (`push_converged_commit_to_bare_repo`) is force-pushed every pass
        // and would otherwise accumulate unbounded in the bare gate repo. A
        // reboucle (`Reboucle`) re-pushes the same ref next pass, so only
        // reclaim it once the run has actually reached a terminal state.
        if let PostConvergenceOutcome::Terminal(_) = &outcome {
            delete_gate_staging_ref(&gate_config.bare_repo_path, run_id).await;
        }
        Ok(outcome)
    }

    /// Waits for `warden-gated`'s one terminal CI message and applies it,
    /// bounding the wait by the triggered child's *liveness* rather than a
    /// wall-clock timeout (issue #15 review, M-new-1). While the child is
    /// alive the watch is legitimately still in progress (bounded on the
    /// gated side by `watch_pr`'s own inactivity timeout), so `warden` keeps
    /// waiting with no cap of its own -- a wall-clock bound derived from that
    /// inactivity timeout would spuriously fail a long-but-active CI, since
    /// `watch_pr` has no absolute cap. Only if the child exits *without* ever
    /// delivering (a hard crash before it could send even a `GateFailed`) is
    /// the run failed outright, after a short grace for an in-flight message.
    pub(super) async fn await_and_apply_ci_result(
        &self,
        run_id: &str,
        listener: &CiResultListener,
        gate_child: GateChild,
    ) -> Result<PostConvergenceOutcome> {
        match await_ci_result(run_id, listener, gate_child).await {
            Ok(message) => self.apply_ci_result_message(run_id, &message).await,
            Err(WardenError::GateChildDiedWithoutResult { .. }) => {
                tracing::error!(
                    run_id,
                    "warden-gated exited without delivering a terminal CI result; failing the run"
                );
                self.fail_awaiting_ci_run(run_id).await
            }
            Err(error) => Err(error),
        }
    }

    /// Fails a run still sitting in `AwaitingCi` (issue #15 review, H1(b)) --
    /// a safe no-op (returns the run's actual current state) if it has
    /// already left that state by the time this runs, mirroring
    /// `apply_ci_result_message`'s own idempotency guard.
    async fn fail_awaiting_ci_run(&self, run_id: &str) -> Result<PostConvergenceOutcome> {
        let run =
            db::get_run(&self.pool, run_id)
                .await?
                .ok_or_else(|| WardenError::RunNotFound {
                    run_id: run_id.to_string(),
                })?;
        if run.state != RunState::AwaitingCi {
            return Ok(PostConvergenceOutcome::Terminal(run.state));
        }
        self.transition(run_id, RunState::Failed).await?;
        Ok(PostConvergenceOutcome::Terminal(RunState::Failed))
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
        // Issue #15 review, M5: `run_id` is the identity of the socket this
        // message was received on (bound per-run); `message.run_id` is
        // whatever the sender's own payload claims. Never apply a message
        // to a run other than the one its own transport already identifies
        // -- untrusted input at the process boundary, cross-checked rather
        // than silently taken on faith.
        if message.run_id != run_id {
            return Err(WardenError::CiResultRunIdMismatch {
                expected: run_id.to_string(),
                actual: message.run_id.clone(),
            });
        }

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
            // Issue #43: a CI `ChecksFailed` reboucle re-enters the loop at
            // `CoderRunning` -> `Reviewing` exactly like any review-charged
            // reboucle, so charge it to the review budget. The in-loop review
            // counter never moves on a CI reboucle (the code passed review
            // locally), so advance the persisted `current_review_cycle`
            // *before* gating -- otherwise a persistently-red CI would gate on
            // a counter that never grows and loop unboundedly instead of
            // terminating at the budget. The main loop re-reads this value
            // after the reboucle so its own clean-review write can't clobber
            // it.
            Some(CiOutcome::ChecksFailed) => {
                let charged = run.current_review_cycle + 1;
                db::set_run_current_review_cycle(&self.pool, run_id, charged).await?;
                decide_next_state_after_ci(CiOutcome::ChecksFailed, charged, run.max_review_cycles)
            }
            // Merged/ChecksPassed/Closed/TimedOut: terminal outcomes with no
            // reboucle and no budget to charge.
            Some(ci_outcome) => decide_next_state_after_ci(
                ci_outcome,
                run.current_review_cycle,
                run.max_review_cycles,
            ),
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::test_support::*;
    use std::process::Command as SyncCommand;
    use tempfile::TempDir;

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
        async fn trigger_run_tail(&self, request: &RunTailTrigger<'_>) -> Result<GateChild> {
            self.deliver(request.run_id, request.ci_result_socket)
                .await?;
            // The message is already buffered on the socket, so the caller's
            // `receive` wins immediately; modeling the child as still-alive
            // keeps the grace path out of these success-case tests entirely.
            Ok(GateChild::never_exiting())
        }

        async fn trigger_resume_watch(
            &self,
            run_id: &str,
            _pr_number: u64,
            ci_result_socket: &Path,
        ) -> Result<GateChild> {
            self.deliver(run_id, ci_result_socket).await?;
            Ok(GateChild::never_exiting())
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
        db::insert_run(pool, &run_id, "/tmp/repo", "main", "intent", 5, 5)
            .await
            .unwrap();
        db::update_run_state(pool, &run_id, RunState::CoderRunning)
            .await
            .unwrap();
        db::update_run_state(pool, &run_id, RunState::Reviewing)
            .await
            .unwrap();
        db::update_run_state(pool, &run_id, RunState::Testing)
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
            max_review_cycles: 5,
            max_test_cycles: 5,
            coder_agent: definition(AgentCommand::new("sh", ["-c", "true"])),
            reviewer_agent: definition(AgentCommand::new("sh", ["-c", "true"])),
            tester_agent: definition(AgentCommand::new("sh", ["-c", "true"])),
            evidence_tool: None,
            evidence_store_in_repo: false,
            gate: Some(GateConfig {
                bare_repo_path: bare_repo.path().to_path_buf(),
                gated_bin: PathBuf::from("/unused/in/this/test"),
                repo_slug: None,
                poll_interval_secs: 1,
                inactivity_timeout_secs: 3600,
            }),
            untrusted_repo_agent_definitions: Vec::new(),
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
    ///
    /// Uses a `ChecksFailed` (reboucle, non-terminal) outcome deliberately:
    /// the staging ref is reclaimed only once a run reaches a *terminal*
    /// state (issue #15 review, L-new-1), so a reboucle leaves it in place
    /// for this assertion to observe.
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
        let ci_finding = Finding {
            source: warden_core::FindingSource::Ci,
            severity: warden_core::Severity::Blocking,
            file: None,
            description: "build failed".to_string(),
            action: None,
        };
        let trigger = FakeGateTrigger {
            outcome: warden_core::CiWatchOutcome::checks_failed(&[ci_finding]),
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

    /// Issue #15 review, L-new-1: once a run reaches a terminal outcome, its
    /// per-run staging ref must be reclaimed from the bare gate repo (it is
    /// force-pushed every pass and would otherwise pin objects unbounded).
    #[tokio::test]
    async fn drive_post_convergence_tail_reclaims_the_staging_ref_on_a_terminal_outcome() {
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
            !staging_ref_check.status.success(),
            "the staging ref must be reclaimed once the run reaches a terminal state"
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

    /// Issue #15 review, M-new-1: the fresh-tail counterpart to
    /// `resume_awaiting_ci_runs_fails_the_run_when_the_ci_result_never_arrives`.
    /// If the triggered `warden-gated` subprocess exits without ever
    /// delivering a terminal message, `drive_post_convergence_tail` must fail
    /// the run once the grace period elapses -- bounded by the child's
    /// liveness, not a wall-clock timeout derived from `watch_pr`'s
    /// (uncapped) inactivity budget. Runs in real time in a couple of seconds
    /// because the already-exited child, not a timer, is what ends the wait.
    #[tokio::test]
    async fn drive_post_convergence_tail_fails_the_run_when_warden_gated_dies_without_delivering() {
        let repo = init_test_repo();
        let bare_repo = init_bare_repo_fixture();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let (run_id, config, converged_commit) =
            converged_run_fixture(&pool, &repo, &bare_repo).await;

        let orchestrator = Orchestrator::new(pool.clone());
        let trigger = NeverDeliversGateTrigger;

        let outcome = orchestrator
            .drive_post_convergence_tail(&run_id, &config, &converged_commit, &trigger)
            .await
            .unwrap();

        assert!(
            matches!(outcome, PostConvergenceOutcome::Terminal(RunState::Failed)),
            "a gated child that exits without delivering must fail the run, not hang it"
        );
        let run = db::get_run(&pool, &run_id).await.unwrap().unwrap();
        assert_eq!(run.state, RunState::Failed);
    }

    /// A [`GateTrigger`] whose subprocess is reported as having *exited*
    /// without ever connecting to `ci_result_socket` (an
    /// already-exited [`GateChild`]) -- standing in for `warden-gated`
    /// crashing/being killed *after* being triggered but *before* it could
    /// deliver even a `GateFailed`. The liveness-bounded wait must fail the
    /// run once its grace period elapses, never hang on it (issue #15 review,
    /// M-new-1).
    struct NeverDeliversGateTrigger;

    impl GateTrigger for NeverDeliversGateTrigger {
        async fn trigger_run_tail(&self, _request: &RunTailTrigger<'_>) -> Result<GateChild> {
            // Models a `warden-gated` that exited without ever delivering:
            // the wait must fail the run once the grace period elapses.
            Ok(GateChild::already_exited())
        }

        async fn trigger_resume_watch(
            &self,
            _run_id: &str,
            _pr_number: u64,
            _ci_result_socket: &Path,
        ) -> Result<GateChild> {
            Ok(GateChild::already_exited())
        }
    }

    /// Issue #15 review, M-new-1: if the triggered `warden-gated` subprocess
    /// exits without ever delivering a terminal `CiResultMessage` (a hard
    /// crash/kill before it could send even a `GateFailed`), the
    /// liveness-bounded wait must fail the run outright once its grace period
    /// elapses, never leave it hanging in `AwaitingCi` forever. The sibling
    /// `drive_post_convergence_tail_fails_the_run_when_warden_gated_dies_without_delivering`
    /// covers the identical branch on the fresh-tail path; both now run in
    /// real time (a short grace, no wall-clock timeout and so no paused-clock
    /// vs `SqlitePool` hazard) because the child-liveness signal, not a timer,
    /// is what ends the wait.
    #[tokio::test]
    async fn resume_awaiting_ci_runs_fails_the_run_when_the_ci_result_never_arrives() {
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let warden_home = TempDir::new().unwrap();

        db::insert_run(&pool, "run-silent", "/tmp/repo", "main", "intent", 3, 3)
            .await
            .unwrap();
        for state in [
            RunState::CoderRunning,
            RunState::Reviewing,
            RunState::Testing,
            RunState::Converged,
            RunState::Pushed,
            RunState::AwaitingCi,
        ] {
            db::update_run_state(&pool, "run-silent", state)
                .await
                .unwrap();
        }
        db::set_run_pr_number(&pool, "run-silent", 42)
            .await
            .unwrap();

        let trigger = NeverDeliversGateTrigger;

        let resumed = resume_awaiting_ci_runs(
            pool.clone(),
            warden_home.path().to_path_buf(),
            trigger,
            warden_home.path().to_path_buf(),
        )
        .await
        .unwrap();

        assert_eq!(resumed, vec!["run-silent".to_string()]);
        let run = db::get_run(&pool, "run-silent").await.unwrap().unwrap();
        assert_eq!(
            run.state,
            RunState::Failed,
            "a run stuck in AwaitingCi with no terminal message ever delivered must be failed \
                 outright once the bounded wait expires, not left hanging"
        );
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
        db::insert_run(&pool, &run_id, "/tmp/repo", "main", "intent", 5, 5)
            .await
            .unwrap();
        db::update_run_state(&pool, &run_id, RunState::CoderRunning)
            .await
            .unwrap();
        db::update_run_state(&pool, &run_id, RunState::Reviewing)
            .await
            .unwrap();
        db::update_run_state(&pool, &run_id, RunState::Testing)
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

    /// Issue #15 review, M5: a message delivered on `run-a`'s own reverse
    /// socket but whose payload claims a different `run_id` must never be
    /// applied to `run-a` -- rejected as a typed error, and `run-a`'s state
    /// must be left completely untouched.
    #[tokio::test]
    async fn apply_ci_result_message_rejects_a_run_id_mismatch() {
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        db::insert_run(&pool, "run-a", "/tmp/repo", "main", "intent", 5, 5)
            .await
            .unwrap();
        for state in [
            RunState::CoderRunning,
            RunState::Reviewing,
            RunState::Testing,
            RunState::Converged,
            RunState::Pushed,
            RunState::AwaitingCi,
        ] {
            db::update_run_state(&pool, "run-a", state).await.unwrap();
        }

        let orchestrator = Orchestrator::new(pool.clone());
        let message = CiResultMessage {
            run_id: "run-b".to_string(),
            pr_number: Some(99),
            outcome: warden_core::CiWatchOutcome::checks_passed(),
        };

        let result = orchestrator
            .apply_ci_result_message("run-a", &message)
            .await;

        assert!(matches!(
            result,
            Err(WardenError::CiResultRunIdMismatch { .. })
        ));
        let run = db::get_run(&pool, "run-a").await.unwrap().unwrap();
        assert_eq!(
            run.state,
            RunState::AwaitingCi,
            "a run_id mismatch must leave the run's state completely untouched"
        );
        assert_eq!(run.pr_number, None);
    }

    /// The bug `recover_crashed_runs` alone would have: `AwaitingCi` has no
    /// live *agent* process to find (it's waiting on `warden-gated`, not an
    /// `agent_processes` row), so the blanket "no live process -> Failed"
    /// rule would incorrectly fail it. Confirms it's left untouched instead.
    #[tokio::test]
    async fn recover_crashed_runs_leaves_awaiting_ci_runs_untouched() {
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();

        db::insert_run(&pool, "run-ci", "/tmp/repo", "main", "intent", 3, 3)
            .await
            .unwrap();
        db::update_run_state(&pool, "run-ci", RunState::CoderRunning)
            .await
            .unwrap();
        db::update_run_state(&pool, "run-ci", RunState::Reviewing)
            .await
            .unwrap();
        db::update_run_state(&pool, "run-ci", RunState::Testing)
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

        db::insert_run(&pool, "run-ci", "/tmp/repo", "main", "intent", 3, 3)
            .await
            .unwrap();
        for state in [
            RunState::CoderRunning,
            RunState::Reviewing,
            RunState::Testing,
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

        let resumed = resume_awaiting_ci_runs(
            pool.clone(),
            warden_home.path().to_path_buf(),
            trigger,
            warden_home.path().to_path_buf(),
        )
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

        db::insert_run(&pool, "run-no-pr", "/tmp/repo", "main", "intent", 3, 3)
            .await
            .unwrap();
        for state in [
            RunState::CoderRunning,
            RunState::Reviewing,
            RunState::Testing,
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

        let resumed = resume_awaiting_ci_runs(
            pool.clone(),
            warden_home.path().to_path_buf(),
            trigger,
            warden_home.path().to_path_buf(),
        )
        .await
        .unwrap();

        assert_eq!(resumed, vec!["run-no-pr".to_string()]);
        let run = db::get_run(&pool, "run-no-pr").await.unwrap().unwrap();
        assert_eq!(run.state, RunState::Failed);
    }

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
        async fn trigger_run_tail(&self, request: &RunTailTrigger<'_>) -> Result<GateChild> {
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
            // Delivery happens later, from the spawned task above -- the
            // child is still "alive" until then.
            Ok(GateChild::never_exiting())
        }

        async fn trigger_resume_watch(
            &self,
            _run_id: &str,
            _pr_number: u64,
            _ci_result_socket: &Path,
        ) -> Result<GateChild> {
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
    /// `MaxReviewCyclesExceeded` -- that transition is illegal from
    /// `AwaitingCi` ([`RunState::validate_transition`]); if
    /// `decide_next_state_after_ci` or its caller ever regressed to
    /// returning it here, `self.transition` would reject it and this test
    /// would fail loudly rather than silently accept a corrupted state.
    #[tokio::test]
    async fn drive_post_convergence_tail_maps_checks_failed_at_cycle_budget_to_failed_not_max_cycles_exceeded(
    ) {
        let repo = init_test_repo();
        let bare_repo = init_bare_repo_fixture();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let (run_id, config, converged_commit) =
            converged_run_fixture(&pool, &repo, &bare_repo).await;
        // `converged_run_fixture` inserts with max_review_cycles = 5 and
        // leaves current_review_cycle at its 0 default. Seed it to 4: this
        // `ChecksFailed` charges the review budget by one (issue #43),
        // advancing the counter to exactly 5 -- the budget -- so the gate
        // lands precisely at the limit.
        db::set_run_current_review_cycle(&pool, &run_id, 4)
            .await
            .unwrap();

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

    /// Issue #43 (review HIGH): a persistently-red CI must terminate at the
    /// review budget rather than loop unboundedly. Each `ChecksFailed`
    /// reboucle is charged to the review budget (it re-enters at
    /// `CoderRunning` -> `Reviewing` like any review-charged reboucle), and
    /// the counter must advance on the CI path *itself* -- driven here through
    /// the real `drive_post_convergence_tail`/`apply_ci_result_message` flow
    /// with no manual seeding. Between passes the run is returned to
    /// `Converged` exactly as the main loop does after a reboucle, so this
    /// exercises the genuine counter progression, not the pure mapping
    /// function. It never touches the test budget.
    #[tokio::test]
    async fn repeated_checks_failed_charges_the_review_budget_until_it_terminates_at_failed() {
        let repo = init_test_repo();
        let bare_repo = init_bare_repo_fixture();
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();
        let (run_id, config, converged_commit) =
            converged_run_fixture(&pool, &repo, &bare_repo).await;
        // Fixture inserts max_review_cycles = 5, current_review_cycle = 0.
        let max = config.max_review_cycles;

        let orchestrator = Orchestrator::new(pool.clone());
        let ci_finding = Finding {
            source: warden_core::FindingSource::Ci,
            severity: warden_core::Severity::Blocking,
            file: None,
            description: "flaky CI".to_string(),
            action: None,
        };
        let trigger = FakeGateTrigger {
            outcome: warden_core::CiWatchOutcome::checks_failed(&[ci_finding]),
            pr_number: Some(99),
        };

        // The first `max - 1` CI failures each reboucle and charge one review
        // cycle; the test budget's counter must never move.
        for expected_cycle in 1..max {
            let outcome = orchestrator
                .drive_post_convergence_tail(&run_id, &config, &converged_commit, &trigger)
                .await
                .unwrap();
            assert!(
                matches!(outcome, PostConvergenceOutcome::Reboucle { .. }),
                "CI failure {expected_cycle} below budget must reboucle, got {outcome:?}"
            );
            let run = db::get_run(&pool, &run_id).await.unwrap().unwrap();
            assert_eq!(
                run.current_review_cycle, expected_cycle,
                "each CI reboucle must advance the review budget counter by exactly one"
            );
            assert_eq!(
                run.current_test_cycle, 0,
                "a CI reboucle is charged to the review budget, never the test budget"
            );
            // The main loop returns the run to `Converged` before re-driving
            // the tail (CoderRunning -> Reviewing -> Testing -> Converged).
            db::update_run_state(&pool, &run_id, RunState::Reviewing)
                .await
                .unwrap();
            db::update_run_state(&pool, &run_id, RunState::Testing)
                .await
                .unwrap();
            db::update_run_state(&pool, &run_id, RunState::Converged)
                .await
                .unwrap();
        }

        // The `max`-th CI failure lands exactly at the budget: it must
        // terminate at `Failed`, never reboucle again.
        let outcome = orchestrator
            .drive_post_convergence_tail(&run_id, &config, &converged_commit, &trigger)
            .await
            .unwrap();
        assert!(
            matches!(outcome, PostConvergenceOutcome::Terminal(RunState::Failed)),
            "CI failure at the review budget must terminate at Failed, got {outcome:?}"
        );
        let run = db::get_run(&pool, &run_id).await.unwrap().unwrap();
        assert_eq!(run.state, RunState::Failed);
        assert_eq!(
            run.current_review_cycle, max,
            "the review budget is what ran out"
        );
        assert_eq!(
            run.current_test_cycle, 0,
            "CI reboucles never charge the test budget"
        );
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

    /// Plants an executable hook at `<repo_path>/.git/hooks/<name>` that
    /// touches `marker` and exits successfully -- if `NO_HOST_HOOKS` were
    /// ever missing from the invocation under test, `marker` would exist
    /// afterwards.
    #[cfg(unix)]
    fn plant_marker_hook(repo_path: &Path, hook_name: &str, marker: &Path) {
        use std::os::unix::fs::PermissionsExt;

        let hooks_dir = repo_path.join(".git").join("hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join(hook_name);
        std::fs::write(
            &hook_path,
            format!("#!/bin/sh\ntouch {}\nexit 0\n", marker.display()),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&hook_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&hook_path, perms).unwrap();
    }

    /// The exact vector the review flagged: `push_converged_commit_to_bare_repo`
    /// runs `git push` against `repo_path` (`-C` above) once a run
    /// converges -- `pre-push` runs in the *pushing* repo, so a hook planted
    /// there (as an agent could, under `--isolation docker`'s rw `.git`
    /// mount) must never fire when `warden` itself pushes.
    #[cfg(unix)]
    #[tokio::test]
    async fn push_to_bare_repo_disables_a_planted_pre_push_hook() {
        let repo = init_test_repo();
        let bare_repo = TempDir::new().unwrap();
        let status = SyncCommand::new("git")
            .current_dir(bare_repo.path())
            .args(["init", "--bare", "--quiet"])
            .status()
            .unwrap();
        assert!(status.success());

        let marker = repo.path().join("pre-push-ran");
        plant_marker_hook(repo.path(), "pre-push", &marker);

        let head = SyncCommand::new("git")
            .current_dir(repo.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let commit_sha = String::from_utf8_lossy(&head.stdout).trim().to_string();

        push_converged_commit_to_bare_repo(
            repo.path(),
            bare_repo.path(),
            &commit_sha,
            "run-hook-1",
        )
        .await
        .unwrap();

        assert!(
            !marker.exists(),
            "a pre-push hook planted in repo_path must never run when warden itself pushes \
                 to the bare gate repo"
        );
    }

    /// `protect_cycle_commit` runs `update-ref` against `repo_path`, which
    /// runs the `reference-transaction` hook -- a hook planted there must
    /// never fire either.
    #[cfg(unix)]
    #[tokio::test]
    async fn protect_cycle_commit_disables_a_planted_reference_transaction_hook() {
        let repo = init_test_repo();
        let marker = repo.path().join("reference-transaction-ran");
        plant_marker_hook(repo.path(), "reference-transaction", &marker);

        let head = SyncCommand::new("git")
            .current_dir(repo.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let commit_sha = String::from_utf8_lossy(&head.stdout).trim().to_string();

        protect_cycle_commit(repo.path(), "run-hook-2", 1, &commit_sha)
            .await
            .unwrap();

        assert!(
            !marker.exists(),
            "a reference-transaction hook planted in repo_path must never run when warden \
                 itself protects a cycle commit via update-ref"
        );
    }

    /// `WorktreeManager::create` runs `git worktree add`, a checkout --
    /// `post-checkout` must never fire, since the hook lives in the main
    /// repo's common `.git`, shared by every worktree an agent's own process
    /// could otherwise have written to.
    #[cfg(unix)]
    #[tokio::test]
    async fn worktree_create_disables_a_planted_post_checkout_hook() {
        let repo = init_test_repo();
        let worktrees_root = TempDir::new().unwrap();
        let marker = repo.path().join("post-checkout-ran");
        plant_marker_hook(repo.path(), "post-checkout", &marker);

        let manager = WorktreeManager::new(repo.path(), worktrees_root.path()).unwrap();
        let _worktree = manager.create("run-hook-3", "coder", "HEAD").await.unwrap();

        assert!(
            !marker.exists(),
            "a post-checkout hook planted in repo_path must never run when warden itself \
                 creates a role's worktree"
        );
    }
}
