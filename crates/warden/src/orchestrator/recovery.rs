//! Crash recovery (Architecture.md §6/§9): [`recover_crashed_runs`] fails
//! any run left in an intermediate state with no live process, and
//! reclaims worktrees/processes it left orphaned; [`resume_awaiting_ci_runs`]
//! is the dedicated counterpart for runs stuck in `AwaitingCi` (ADR-0011).

use super::gate_tail::delete_gate_staging_ref;
use super::*;

/// Crash recovery (Architecture.md §6 "Règle de récupération" / §9 Disaster
/// Recovery): any run left in an intermediate state
/// ([`RunState::is_intermediate`]) with no live process associated is marked
/// `Failed`. A run whose latest agent process is still alive -- same PID
/// *and* same recorded start time, see `process::is_process_alive` -- is
/// left untouched; this does not attempt to re-attach to it.
///
/// A run recovered as `Failed` may have left two kinds of resources
/// orphaned by the crash (a crash is a `SIGKILL`, so no `Drop`/`kill_on_drop`
/// ever ran): worktrees, and agent child processes. Reclaiming both is
/// [`reclaim_orphan_resources`] -- best-effort, a cleanup failure for one run
/// is logged and does not stop recovery from proceeding to the next one.
///
/// The state write and that reclaim are two separate steps, so a *second*
/// crash in between must not orphan a resource permanently: a run already
/// `Failed` is no longer [`RunState::is_intermediate`], so
/// [`db::list_failed_runs_with_pending_cleanup`] is a second pass that
/// re-finds it by what's still recorded (an open `agent_processes` row, or
/// an uncleared worktree path) rather than by run state, retrying only for
/// as long as it actually still has something to reclaim.
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

        run.state
            .validate_transition(RunState::Failed, run.total_steps)?;
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

/// Crash-recovery counterpart of issue #15/ADR-0011: resumes every run
/// found stuck in `AwaitingCi` on startup, re-requesting the watch from
/// `warden-gated` rather than treating it like a crashed agent process (see
/// [`recover_crashed_runs`] for why that check does not apply here).
/// Idempotent by construction: `watch_pr` re-polls GitHub, so re-requesting
/// a terminal PR just returns that same terminal outcome again (ADR-0011).
///
/// A run with no persisted `pr_number` (crashed before `OpenDraft` ever
/// returned one) has nothing to resume watching and is marked `Failed`
/// instead.
///
/// Returns the ids of runs this call reached a terminal state for (`Done`
/// or `Failed`) or reboucled to `CoderRunning`. Takes
/// `pool`/`warden_home`/`trigger`/`bare_repo_path` by value so callers can
/// move this whole call into a `tokio::spawn`ed task -- it must never gate
/// `warden`'s own startup, since a stuck run's watch can legitimately take a
/// long, uncapped time to resolve.
pub async fn resume_awaiting_ci_runs<G: GateTrigger>(
    pool: SqlitePool,
    warden_home: PathBuf,
    trigger: G,
    bare_repo_path: PathBuf,
) -> Result<Vec<String>> {
    let intermediate_runs = db::list_intermediate_runs(&pool).await?;
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
            run.state
                .validate_transition(RunState::Failed, run.total_steps)?;
            db::update_run_state(&pool, &run.id, RunState::Failed).await?;
            // Issue #15 review, L3: mirrors recover_crashed_runs's own
            // orphan reclaim for every other path that fails a run outright
            // -- defensive (a run that legitimately reached AwaitingCi
            // should already have no open agent_processes/worktree rows
            // left), but confirms no leak rather than assuming it.
            reclaim_orphan_resources(&pool, &run).await;
            // Best-effort staging-ref reclaim (issue #15 review, L-new-1):
            // the run reached `AwaitingCi`, so a staging ref was pushed for
            // it even though no PR number was ever recorded.
            delete_gate_staging_ref(&bare_repo_path, &run.id).await;
            resumed_run_ids.push(run.id);
            continue;
        };

        tracing::info!(run_id = %run.id, pr_number, "resuming CI watch for a run stuck in AwaitingCi");
        let runs_dir = warden_home.join("runs");
        let listener = CiResultListener::bind(&run.id, &runs_dir).await?;
        let gate_child = trigger
            .trigger_resume_watch(&run.id, pr_number, listener.socket_path())
            .await?;

        // Issue #15 review, M-new-1: bounded by the resumed child's liveness,
        // mirroring `drive_post_convergence_tail`'s identical handling.
        let outcome = orchestrator
            .await_and_apply_ci_result(&run.id, &listener, gate_child)
            .await?;
        if let PostConvergenceOutcome::Terminal(_) = &outcome {
            delete_gate_staging_ref(&bare_repo_path, &run.id).await;
        }
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
                    db::clear_cycle_worktree_path(pool, &entry.cycle_id, &entry.role).await
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
    use crate::orchestrator::test_support::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn recovery_marks_intermediate_run_failed_when_its_process_is_dead() {
        let db_dir = TempDir::new().unwrap();
        let pool = db::connect(&db_dir.path().join("state.db")).await.unwrap();

        db::insert_run(
            &pool,
            "crashed-run",
            "/tmp/repo",
            "main",
            "intent",
            3,
            3,
            3,
            5,
        )
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
            "coder",
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

        db::insert_run(&pool, "live-run", "/tmp/repo", "main", "intent", 3, 3, 3, 5)
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
            "coder",
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
            3,
            3,
            5,
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
            "coder",
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
            "coder",
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
            3,
            3,
            5,
        )
        .await
        .unwrap();
        db::update_run_state(&pool, "orphan-process-run", RunState::RunningStep(2))
            .await
            .unwrap();
        db::insert_cycle(&pool, "orphan-process-cycle", "orphan-process-run", 1)
            .await
            .unwrap();

        // An *earlier* process (the reviewer, which already ran and passed
        // this cycle -- Phase A's gate, issue #41) is still alive, but the
        // run's *latest* recorded process (inserted after it, so it sorts
        // last by `started_at` and is what drives the Failed decision, see
        // `latest_open_agent_process_for_run`) is dead -- exactly the shape
        // a crash mid-`Testing` leaves behind.
        let mut live_child = tokio::process::Command::new("sh")
            .args(["-c", "sleep 30"])
            .spawn()
            .unwrap();
        let live_pid = live_child.id().unwrap();
        db::insert_agent_process(
            &pool,
            "orphan-process-live",
            "orphan-process-cycle",
            "tester",
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
            "reviewer",
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

        db::insert_run(
            &pool,
            "pid-reuse-run",
            "/tmp/repo",
            "main",
            "intent",
            3,
            3,
            3,
            5,
        )
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
    /// `coder_running`/`reviewing`/`testing`/`awaiting_ci`, so a `Failed`
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
            3,
            3,
            5,
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
            "coder",
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
            "coder",
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
            3,
            3,
            5,
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
            "coder",
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
            "coder",
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
}
