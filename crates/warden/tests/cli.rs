//! End-to-end tests driving the actual `warden` binary as a user/CI caller
//! would (`warden run --repo ... --coder-cmd ... `), not the internal
//! `Orchestrator` API directly. These exercise the acceptance criteria from
//! issue #1 through the real entry point: CLI arg parsing (`main.rs`),
//! startup crash recovery, the convergence loop, and the SQLite state left
//! behind — the same path a human invoking `warden run` from a shell hits.

use std::path::{Path, PathBuf};
use std::process::Command as SyncCommand;

use assert_cmd::Command;
use predicates::str::contains;
use tempfile::TempDir;
use warden_core::{AgentRole, FindingSource, RunState};

/// Sets up a throwaway git repo with a single commit, suitable as `--repo`.
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

/// The CLI splits `--coder-cmd`/etc. on whitespace (see `main.rs`,
/// `parse_agent_command`), so a script with embedded logic must live in its
/// own file rather than being passed as an inline `sh -c "..."` string.
fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    path
}

fn always_converging_scripts(dir: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let coder = write_script(
        dir,
        "coder.sh",
        r#"#!/bin/sh
echo hello >> notes.txt
git add notes.txt
git -c user.email=test@warden.local -c user.name=warden-test commit -q -m "coder cycle"
"#,
    );
    // NDJSON wire format (code-standards.md "Agent Subprocess Protocol"):
    // one finding object per line, no wrapping object — "no findings" is
    // simply no stdout at all.
    let reviewer = write_script(dir, "reviewer.sh", "#!/bin/sh\ntrue\n");
    let tester = write_script(dir, "tester.sh", "#!/bin/sh\ntrue\n");
    (coder, reviewer, tester)
}

/// Extracts the run id `warden run`'s final stdout line
/// (`run <uuid> finished: <State>`) so the test can look the run up in
/// SQLite afterwards.
fn extract_run_id(stdout: &str) -> String {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("run "))
        .and_then(|rest| rest.split(' ').next())
        .unwrap_or_else(|| panic!("could not find run id in stdout: {stdout:?}"))
        .to_string()
}

/// Acceptance criterion 1 (issue #1): "Un cycle complet (coder -> review ->
/// test -> reboucle si besoin) est reproductible sur un repo de test" —
/// driven through the real `warden run` CLI command, exactly as a user would
/// invoke it, with a coder that only converges after one reboucle.
///
/// Acceptance criterion 3 is also verified here (isolation): the main repo's
/// git history/working tree must be untouched by the run.
#[tokio::test]
async fn e2e_full_convergence_cycle_reboucles_then_converges_via_cli() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let scripts_dir = TempDir::new().unwrap();

    let coder = write_script(
        scripts_dir.path(),
        "coder.sh",
        r#"#!/bin/sh
if [ -f status.txt ] && [ "$(cat status.txt)" = "broken" ]; then
    echo fixed > status.txt
else
    echo broken > status.txt
fi
git add status.txt
git -c user.email=test@warden.local -c user.name=warden-test commit -q -m "coder cycle"
"#,
    );
    // NDJSON wire format (code-standards.md "Agent Subprocess Protocol"):
    // one finding object per line, no wrapping object — "no findings" is
    // simply no stdout at all.
    let reviewer = write_script(
        scripts_dir.path(),
        "reviewer.sh",
        r#"#!/bin/sh
if [ -f status.txt ] && [ "$(cat status.txt)" = "broken" ]; then
    echo '{"source":"reviewer","severity":"blocking","description":"status is broken"}'
fi
"#,
    );
    let tester = write_script(scripts_dir.path(), "tester.sh", "#!/bin/sh\ntrue\n");

    let before_status = SyncCommand::new("git")
        .current_dir(repo.path())
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    assert!(before_status.stdout.is_empty(), "repo must start clean");

    let assert = Command::cargo_bin("warden")
        .unwrap()
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "flip status to fixed",
            "--branch",
            "main",
            "--max-cycles",
            "5",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--coder-cmd",
            &format!("sh {}", coder.display()),
            "--reviewer-cmd",
            &format!("sh {}", reviewer.display()),
            "--tester-cmd",
            &format!("sh {}", tester.display()),
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let run_id = extract_run_id(&stdout);

    // Criterion 3: the CLI must never write into the user's main dev
    // worktree — only Warden's own worktrees under --warden-home.
    let after_log = SyncCommand::new("git")
        .current_dir(repo.path())
        .args(["log", "--oneline"])
        .output()
        .unwrap();
    let commit_count = String::from_utf8_lossy(&after_log.stdout).lines().count();
    assert_eq!(
        commit_count, 1,
        "main repo must still only have its initial commit after `warden run`"
    );
    let after_status = SyncCommand::new("git")
        .current_dir(repo.path())
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    assert!(
        after_status.stdout.is_empty(),
        "main repo working tree must be clean after `warden run`: {:?}",
        String::from_utf8_lossy(&after_status.stdout)
    );

    // Cross-check against the SQLite state the CLI left behind: two cycles
    // (one reboucle), final state Converged.
    let db_path = warden_home.path().join("state.db");
    let pool = warden::db::connect(&db_path).await.unwrap();
    let run = warden::db::get_run(&pool, &run_id).await.unwrap().unwrap();
    assert_eq!(run.state, RunState::Converged);
    assert_eq!(run.current_cycle, 2);
}

/// Acceptance criterion 2 (issue #1): "Un crash simulé de l'orchestrateur en
/// plein cycle est détectable au redémarrage (run marqué `Failed` si aucun
/// process vivant associé)" — driven through the real CLI restart path:
/// `main.rs::run` unconditionally calls `recover_crashed_runs` before
/// starting a new run, on every invocation. This test pre-seeds the SQLite
/// db (the same one `--warden-home` points `warden run` at) with a run
/// stuck in `CoderRunning` whose recorded agent process is already dead —
/// simulating an orchestrator that crashed mid-cycle — then launches a
/// brand new, unrelated `warden run` against that same warden-home and
/// checks the *stale* run was flipped to `Failed` as a side effect of
/// startup, exactly as a real restart after a crash would behave.
#[tokio::test]
async fn e2e_crashed_run_is_marked_failed_on_the_next_cli_invocation() {
    let warden_home = TempDir::new().unwrap();
    let db_path = warden_home.path().join("state.db");

    // Seed: a run "crashed" mid-cycle, with a dead PID recorded for its
    // coder agent process (deterministic dead pid: spawn-then-wait, not a
    // guessed unused number).
    {
        let pool = warden::db::connect(&db_path).await.unwrap();
        warden::db::insert_run(&pool, "crashed-run", "/tmp/some-repo", "main", "intent", 3)
            .await
            .unwrap();
        warden::db::update_run_state(&pool, "crashed-run", RunState::CoderRunning)
            .await
            .unwrap();
        warden::db::insert_cycle(&pool, "crashed-cycle", "crashed-run", 1)
            .await
            .unwrap();

        let mut child = tokio::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let dead_pid = child.id().unwrap();
        child.wait().await.unwrap();

        warden::db::insert_agent_process(
            &pool,
            "crashed-process",
            "crashed-cycle",
            AgentRole::Coder,
            dead_pid,
            "/tmp/wt",
        )
        .await
        .unwrap();
        // Deliberately never mark_agent_process_ended: simulates the
        // orchestrator dying before it could record completion.
        pool.close().await;
    }

    // Restart: a completely unrelated, trivial run against the same
    // --warden-home. Startup crash recovery must run first regardless.
    let repo = init_test_repo();
    let scripts_dir = TempDir::new().unwrap();
    let (coder, reviewer, tester) = always_converging_scripts(scripts_dir.path());

    Command::cargo_bin("warden")
        .unwrap()
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "unrelated new run",
            "--branch",
            "main",
            "--max-cycles",
            "3",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--coder-cmd",
            &format!("sh {}", coder.display()),
            "--reviewer-cmd",
            &format!("sh {}", reviewer.display()),
            "--tester-cmd",
            &format!("sh {}", tester.display()),
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    let pool = warden::db::connect(&db_path).await.unwrap();
    let recovered = warden::db::get_run(&pool, "crashed-run")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        recovered.state,
        RunState::Failed,
        "a run left mid-cycle with no live process must be marked Failed on the next CLI startup"
    );
}

/// M2: a coder that exits non-zero must short-circuit the cycle — the run
/// is persisted as `Failed` (write-ahead, before the CLI process exits) and
/// review/test must never run at all, not just "run but be ignored". Proven
/// here by checking the reviewer's worktree was never created under
/// `--warden-home`, since `run_review_and_test` is only reached after a
/// successful coder run.
#[tokio::test]
async fn e2e_failing_coder_marks_run_failed_and_never_reaches_review() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let scripts_dir = TempDir::new().unwrap();

    let coder = write_script(
        scripts_dir.path(),
        "coder.sh",
        "#!/bin/sh\necho 'boom: build failed' >&2\nexit 1\n",
    );
    let reviewer = write_script(scripts_dir.path(), "reviewer.sh", "#!/bin/sh\ntrue\n");
    let tester = write_script(scripts_dir.path(), "tester.sh", "#!/bin/sh\ntrue\n");

    Command::cargo_bin("warden")
        .unwrap()
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "coder will fail",
            "--branch",
            "main",
            "--max-cycles",
            "3",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--coder-cmd",
            &format!("sh {}", coder.display()),
            "--reviewer-cmd",
            &format!("sh {}", reviewer.display()),
            "--tester-cmd",
            &format!("sh {}", tester.display()),
        ])
        .assert()
        .failure();

    // A fresh --warden-home was used solely for this run, so exactly one
    // row exists; no db.rs getter lists all runs, so a direct query is used
    // here rather than adding production API surface just for a test.
    let db_path = warden_home.path().join("state.db");
    let pool = warden::db::connect(&db_path).await.unwrap();
    let (run_id,): (String,) = sqlx::query_as("SELECT id FROM runs LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();

    let run = warden::db::get_run(&pool, &run_id).await.unwrap().unwrap();
    assert_eq!(
        run.state,
        RunState::Failed,
        "a non-zero coder exit must persist the run as Failed"
    );

    let reviewer_worktree = warden_home
        .path()
        .join("worktrees")
        .join(&run_id)
        .join("reviewer");
    assert!(
        !reviewer_worktree.exists(),
        "reviewer must never run once the coder has failed the cycle (no short-circuit)"
    );
}

/// M4: the commit a cycle's coder produces must (a) be persisted and
/// retrievable from SQLite (`cycles.coder_commit_sha`,
/// `runs.converged_commit_sha`), (b) be protected by a local ref in the
/// *main* repository so it survives worktree removal / `git gc`, and (c)
/// never mutate the main repo's current branch, HEAD, or working tree — the
/// ref write is metadata-only, the same category of change
/// `git worktree add/remove` already makes.
#[tokio::test]
async fn e2e_converged_commit_is_persisted_and_protected_without_touching_main_branch() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let scripts_dir = TempDir::new().unwrap();
    let (coder, reviewer, tester) = always_converging_scripts(scripts_dir.path());

    let original_head_ref = String::from_utf8_lossy(
        &SyncCommand::new("git")
            .current_dir(repo.path())
            .args(["symbolic-ref", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .trim()
    .to_string();
    let original_commit_sha = String::from_utf8_lossy(
        &SyncCommand::new("git")
            .current_dir(repo.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .trim()
    .to_string();

    let assert = Command::cargo_bin("warden")
        .unwrap()
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "single converging cycle",
            "--branch",
            "main",
            "--max-cycles",
            "3",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--coder-cmd",
            &format!("sh {}", coder.display()),
            "--reviewer-cmd",
            &format!("sh {}", reviewer.display()),
            "--tester-cmd",
            &format!("sh {}", tester.display()),
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let run_id = extract_run_id(&stdout);

    // The main repo's current branch/HEAD must be exactly what it was
    // before `warden run` — writing `refs/warden/...` must never touch
    // `refs/heads/...` or move HEAD.
    let after_head_ref = String::from_utf8_lossy(
        &SyncCommand::new("git")
            .current_dir(repo.path())
            .args(["symbolic-ref", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .trim()
    .to_string();
    assert_eq!(after_head_ref, original_head_ref);
    let after_commit_sha = String::from_utf8_lossy(
        &SyncCommand::new("git")
            .current_dir(repo.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .trim()
    .to_string();
    assert_eq!(
        after_commit_sha, original_commit_sha,
        "main repo's checked-out commit must be unchanged by `warden run`"
    );
    let status = SyncCommand::new("git")
        .current_dir(repo.path())
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    assert!(
        status.stdout.is_empty(),
        "main repo working tree must stay clean"
    );

    let db_path = warden_home.path().join("state.db");
    let pool = warden::db::connect(&db_path).await.unwrap();
    let run = warden::db::get_run(&pool, &run_id).await.unwrap().unwrap();
    let converged_sha = run
        .converged_commit_sha
        .expect("a Converged run must have a persisted converged_commit_sha");
    assert_eq!(
        converged_sha.len(),
        40,
        "expected a full SHA-1 hex commit id"
    );
    assert_ne!(
        converged_sha, original_commit_sha,
        "converged commit must be the coder's new commit, not the repo's original HEAD"
    );

    // No `db.rs` getter exposes a single cycle's row yet, so this reads the
    // column directly — a test-only convenience, not new production API.
    let (cycle_sha,): (Option<String>,) =
        sqlx::query_as("SELECT coder_commit_sha FROM cycles WHERE run_id = ?")
            .bind(&run_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        cycle_sha.as_deref(),
        Some(converged_sha.as_str()),
        "cycles.coder_commit_sha must match the run's converged_commit_sha for a single-cycle run"
    );

    // M4: the commit must be reachable via a local ref in the *main* repo
    // (never the now-removed coder worktree) so it survives `git gc`.
    let ref_name = format!("refs/warden/runs/{run_id}/cycle-1");
    let ref_lookup = SyncCommand::new("git")
        .current_dir(repo.path())
        .args(["rev-parse", &ref_name])
        .output()
        .unwrap();
    assert!(
        ref_lookup.status.success(),
        "expected protective ref {ref_name} to exist in the main repo"
    );
    assert_eq!(
        String::from_utf8_lossy(&ref_lookup.stdout).trim(),
        converged_sha,
        "the protective ref must point at the same commit persisted in SQLite"
    );
}

/// Acceptance criterion 1 (issue #2): "Aucune collision d'écriture constatée
/// sur un repo de test avec des findings croisés (reviewer et tester
/// modifiant des fichiers différents en simultané)" — driven through the
/// real `warden run` CLI entry point.
///
/// The reviewer writes `review_target.txt`, then (after a deliberate sleep
/// that overlaps with the tester's run) reads back `test_target.txt` from
/// its own worktree; the tester does the mirror image. If reviewer and
/// tester ever shared a worktree/directory (a write collision), the other
/// role's write — which completes well before the sleep ends — would already
/// be visible, instead of the untouched original content. This is what
/// distinguishes "isolated worktrees" from "shared worktree" deterministically,
/// without relying on interleaving order.
#[tokio::test]
async fn e2e_reviewer_and_tester_modify_different_files_concurrently_without_collision() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let scripts_dir = TempDir::new().unwrap();

    let coder = write_script(
        scripts_dir.path(),
        "coder.sh",
        r#"#!/bin/sh
echo original-review > review_target.txt
echo original-test > test_target.txt
git add review_target.txt test_target.txt
git -c user.email=test@warden.local -c user.name=warden-test commit -q -m "coder cycle"
"#,
    );
    let reviewer = write_script(
        scripts_dir.path(),
        "reviewer.sh",
        r#"#!/bin/sh
echo modified-by-reviewer > review_target.txt
sleep 0.3
seen=$(cat test_target.txt)
echo "{\"source\":\"reviewer\",\"severity\":\"info\",\"description\":\"review_target=modified-by-reviewer test_target_seen=$seen\"}"
"#,
    );
    let tester = write_script(
        scripts_dir.path(),
        "tester.sh",
        r#"#!/bin/sh
echo modified-by-tester > test_target.txt
sleep 0.3
seen=$(cat review_target.txt)
echo "{\"source\":\"tester\",\"severity\":\"info\",\"description\":\"test_target=modified-by-tester review_target_seen=$seen\"}"
"#,
    );

    let assert = Command::cargo_bin("warden")
        .unwrap()
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "crossed findings, no collision",
            "--branch",
            "main",
            "--max-cycles",
            "3",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--coder-cmd",
            &format!("sh {}", coder.display()),
            "--reviewer-cmd",
            &format!("sh {}", reviewer.display()),
            "--tester-cmd",
            &format!("sh {}", tester.display()),
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let run_id = extract_run_id(&stdout);

    let db_path = warden_home.path().join("state.db");
    let pool = warden::db::connect(&db_path).await.unwrap();

    // No `db.rs` getter maps a run to its cycles yet, so a direct query is
    // used here rather than adding production API surface just for a test
    // (same convention as the other tests in this file).
    let (cycle_id,): (String,) = sqlx::query_as("SELECT id FROM cycles WHERE run_id = ?")
        .bind(&run_id)
        .fetch_one(&pool)
        .await
        .unwrap();

    let findings = warden::db::list_findings_for_cycle(&pool, &cycle_id)
        .await
        .unwrap();
    assert_eq!(
        findings.len(),
        2,
        "expected exactly one finding from each of reviewer and tester"
    );

    let reviewer_finding = findings
        .iter()
        .find(|f| f.source == FindingSource::Reviewer)
        .expect("reviewer finding present");
    let tester_finding = findings
        .iter()
        .find(|f| f.source == FindingSource::Tester)
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

    // Cross-check at the worktree-path level too: reviewer and tester must
    // have been assigned distinct directories for this cycle.
    let (reviewer_wt, tester_wt): (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT reviewer_worktree_path, tester_worktree_path FROM cycles WHERE id = ?",
    )
    .bind(&cycle_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let reviewer_wt = reviewer_wt.expect("reviewer worktree path recorded");
    let tester_wt = tester_wt.expect("tester worktree path recorded");
    assert_ne!(
        reviewer_wt, tester_wt,
        "reviewer and tester must run in distinct worktree directories"
    );
}

/// Issue #6 acceptance criteria, driven end-to-end through the real CLI
/// restart path (`main.rs::run` unconditionally calls `recover_crashed_runs`
/// before starting any new run): "aucun worktree ni process orphelin ne
/// persiste après un cycle de crash + redémarrage". This seeds a single
/// crashed run that left BOTH kinds of orphaned resources behind — an
/// on-disk worktree whose owning guard was never dropped (a crash is a
/// `SIGKILL`, not a graceful `Drop`), and a genuinely still-running agent
/// process — then launches a brand-new, unrelated `warden run` against the
/// same `--warden-home` and checks recovery cleaned up both as a side effect
/// of that single startup, with no manual intervention.
#[tokio::test]
async fn e2e_crash_restart_leaves_no_orphan_worktree_or_process() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let db_path = warden_home.path().join("state.db");

    // Seed: a run "crashed" mid-cycle, with a real orphaned worktree on disk
    // and a real, still-running orphaned agent process.
    let (worktree_path, mut orphan_child) = {
        let pool = warden::db::connect(&db_path).await.unwrap();

        let worktree_manager = warden::worktree::WorktreeManager::new(
            repo.path(),
            warden_home.path().join("worktrees"),
        )
        .unwrap();
        // Simulates the crash itself: the `Worktree` guard is forgotten
        // instead of dropped or explicitly removed -- exactly what a
        // SIGKILL'd orchestrator would leave behind.
        let worktree = worktree_manager
            .create("orphan-e2e-run", "coder", "HEAD")
            .await
            .unwrap();
        let worktree_path = worktree.path().to_path_buf();
        std::mem::forget(worktree);
        assert!(worktree_path.exists(), "precondition: orphan worktree exists on disk");

        warden::db::insert_run(
            &pool,
            "orphan-e2e-run",
            &repo.path().display().to_string(),
            "main",
            "intent",
            3,
        )
        .await
        .unwrap();
        warden::db::update_run_state(&pool, "orphan-e2e-run", RunState::CoderRunning)
            .await
            .unwrap();
        warden::db::insert_cycle(&pool, "orphan-e2e-cycle", "orphan-e2e-run", 1)
            .await
            .unwrap();
        warden::db::set_cycle_worktree_path(
            &pool,
            "orphan-e2e-cycle",
            AgentRole::Coder,
            &worktree_path.display().to_string(),
        )
        .await
        .unwrap();

        // Two concurrent agent processes recorded for the same cycle, the
        // way reviewer and tester run in parallel (ADR-0003): an earlier one
        // that is genuinely still alive (a real orphan agent process the
        // crashed orchestrator never reaped or killed), and a later, dead
        // one -- recovery decides whether the *run* crashed based on the
        // latest recorded process (`latest_open_agent_process_for_run`), so
        // the dead one must sort after the live one for this run to be
        // recovered as Failed at all, exactly like
        // `recovery_terminates_an_orphaned_live_agent_process` in
        // orchestrator.rs.
        let orphan_child = tokio::process::Command::new("sh")
            .args(["-c", "sleep 30"])
            .spawn()
            .unwrap();
        let orphan_pid = orphan_child.id().unwrap();
        warden::db::insert_agent_process(
            &pool,
            "orphan-e2e-live-process",
            "orphan-e2e-cycle",
            AgentRole::Reviewer,
            orphan_pid,
            &worktree_path.display().to_string(),
        )
        .await
        .unwrap();

        // Guarantees the dead process's `started_at` sorts strictly after
        // the live one's, so which row is "latest" is deterministic.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;

        let mut dead_child = tokio::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let dead_pid = dead_child.id().unwrap();
        dead_child.wait().await.unwrap();
        warden::db::insert_agent_process(
            &pool,
            "orphan-e2e-dead-process",
            "orphan-e2e-cycle",
            AgentRole::Coder,
            dead_pid,
            &worktree_path.display().to_string(),
        )
        .await
        .unwrap();

        pool.close().await;
        (worktree_path, orphan_child)
    };

    // Restart: a completely unrelated, trivial run against the same
    // --warden-home. Startup crash recovery must run first regardless.
    let scripts_dir = TempDir::new().unwrap();
    let (coder, reviewer, tester) = always_converging_scripts(scripts_dir.path());

    Command::cargo_bin("warden")
        .unwrap()
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "unrelated new run",
            "--branch",
            "main",
            "--max-cycles",
            "3",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--coder-cmd",
            &format!("sh {}", coder.display()),
            "--reviewer-cmd",
            &format!("sh {}", reviewer.display()),
            "--tester-cmd",
            &format!("sh {}", tester.display()),
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    // Behavior 1: the run is recovered as Failed and its orphan worktree no
    // longer exists on disk.
    let pool = warden::db::connect(&db_path).await.unwrap();
    let recovered = warden::db::get_run(&pool, "orphan-e2e-run")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(recovered.state, RunState::Failed);
    assert!(
        !worktree_path.exists(),
        "no orphan worktree may persist after a crash+restart cycle"
    );

    // Behavior 2: the orphan agent process was actually terminated, not
    // merely forgotten about.
    let exit_status = orphan_child.wait().await.unwrap();
    assert!(
        !exit_status.success(),
        "no orphan agent process may persist after a crash+restart cycle"
    );
    let open_processes =
        warden::db::list_open_agent_processes_for_run(&pool, "orphan-e2e-run")
            .await
            .unwrap();
    assert!(
        open_processes.is_empty(),
        "recovery must mark the orphaned agent_processes row ended"
    );
}

/// Issue #6 acceptance criterion: an automatic backup of the SQLite database
/// is taken before a pending schema migration is applied, driven through the
/// real CLI entry point rather than calling `db::connect` directly -- every
/// `warden run` invocation opens the db exactly this way on startup
/// (`main.rs::run`). Simulates restarting a pre-existing Warden installation
/// whose schema predates the latest migrations.
#[tokio::test]
async fn e2e_restart_backs_up_db_before_applying_pending_migrations_via_cli() {
    let warden_home = TempDir::new().unwrap();
    std::fs::create_dir_all(warden_home.path()).unwrap();
    let db_path = warden_home.path().join("state.db");

    // Simulate an older installation: only the first migration has ever
    // been applied, so the rest are pending on the next `warden run`.
    {
        use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
        let options = SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);
        let pool = SqlitePoolOptions::new()
            .connect_with(options)
            .await
            .unwrap();
        static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");
        let first_migration_version = MIGRATOR.iter().next().unwrap().version;
        MIGRATOR
            .run_to(first_migration_version, &pool)
            .await
            .unwrap();
        pool.close().await;
    }

    let repo = init_test_repo();
    let scripts_dir = TempDir::new().unwrap();
    let (coder, reviewer, tester) = always_converging_scripts(scripts_dir.path());

    Command::cargo_bin("warden")
        .unwrap()
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "trigger startup migration",
            "--branch",
            "main",
            "--max-cycles",
            "3",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--coder-cmd",
            &format!("sh {}", coder.display()),
            "--reviewer-cmd",
            &format!("sh {}", reviewer.display()),
            "--tester-cmd",
            &format!("sh {}", tester.display()),
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    let backups: Vec<_> = std::fs::read_dir(warden_home.path())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().contains(".bak-"))
        .collect();
    assert_eq!(
        backups.len(),
        1,
        "restarting against a pre-existing db with pending migrations must produce exactly one backup file: {backups:?}"
    );

    // The schema must actually have been migrated to current, not just
    // backed up and left stale.
    let pool = warden::db::connect(&db_path).await.unwrap();
    let (run_id,): (String,) = sqlx::query_as("SELECT id FROM runs LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    let run = warden::db::get_run(&pool, &run_id).await.unwrap().unwrap();
    assert_eq!(run.state, RunState::Converged);
}

/// A malformed `--coder-cmd` (empty string) must be a clean CLI error, not a
/// panic — realistic misuse from a human typo or a bad config file.
#[test]
fn e2e_empty_agent_command_is_a_clean_cli_error_not_a_panic() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();

    Command::cargo_bin("warden")
        .unwrap()
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "irrelevant",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--coder-cmd",
            "",
            "--reviewer-cmd",
            "sh -c true",
            "--tester-cmd",
            "sh -c true",
        ])
        .assert()
        .failure()
        .stderr(contains("agent command must not be empty"));
}

/// A `--repo` that isn't a git repository must fail cleanly with an
/// actionable error rather than a panic or a silently-created worktree.
#[test]
fn e2e_non_git_repo_path_is_a_clean_cli_error() {
    let not_a_repo = TempDir::new().unwrap();
    let warden_home = TempDir::new().unwrap();

    Command::cargo_bin("warden")
        .unwrap()
        .args([
            "run",
            "--repo",
            not_a_repo.path().to_str().unwrap(),
            "--intent",
            "irrelevant",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--coder-cmd",
            "sh -c true",
            "--reviewer-cmd",
            "sh -c true",
            "--tester-cmd",
            "sh -c true",
        ])
        .assert()
        .failure();
}
