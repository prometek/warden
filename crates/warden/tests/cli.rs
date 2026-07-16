//! End-to-end tests driving the actual `warden` binary as a user/CI caller
//! would (`warden run --repo ... --coder-agent ... `), not the internal
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

/// A script with embedded logic lives in its own file, and a markdown agent
/// definition (ADR-0013) points at it -- see [`script_agent_definition`].
fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    path
}

/// Writes the markdown agent definition the CLI now takes (ADR-0013, issue
/// #22): TOML frontmatter naming the runner and its program/args, then the
/// role's system prompt as the markdown body.
fn write_agent_definition(
    dir: &Path,
    name: &str,
    program: &str,
    args: &[&str],
    system_prompt: &str,
) -> PathBuf {
    let args_toml = args
        .iter()
        .map(|arg| format!("{arg:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let path = dir.join(name);
    std::fs::write(
        &path,
        format!(
            "+++\nrunner = \"command\"\nprogram = {program:?}\nargs = [{args_toml}]\n+++\n\n{system_prompt}\n"
        ),
    )
    .unwrap();
    path
}

/// Wraps a test script in a definition that runs it via `sh` -- the
/// `command` runner is the escape hatch (ADR-0013 / Q4) that keeps a plain
/// script a first-class agent target now that `--*-cmd` is gone. Named after
/// the script, so a test with several scripts gets one definition each.
fn script_agent_definition(script: &Path, role: &str) -> PathBuf {
    let name = format!("{}.agent.md", script.file_stem().unwrap().to_str().unwrap());
    write_agent_definition(
        script.parent().unwrap(),
        &name,
        "sh",
        &[script.to_str().unwrap()],
        &format!("You are Warden's {role}."),
    )
}

/// A definition whose agent must never actually run: for tests where the CLI
/// itself has to reject the invocation first.
fn noop_agent_definition(dir: &Path, role: &str) -> PathBuf {
    write_agent_definition(
        dir,
        &format!("{role}.agent.md"),
        "sh",
        &["-c", "true"],
        &format!("You are Warden's {role}."),
    )
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
            "--coder-agent",
            script_agent_definition(&coder, "coder").to_str().unwrap(),
            "--reviewer-agent",
            script_agent_definition(&reviewer, "reviewer")
                .to_str()
                .unwrap(),
            "--tester-agent",
            script_agent_definition(&tester, "tester").to_str().unwrap(),
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
            "--coder-agent",
            script_agent_definition(&coder, "coder").to_str().unwrap(),
            "--reviewer-agent",
            script_agent_definition(&reviewer, "reviewer")
                .to_str()
                .unwrap(),
            "--tester-agent",
            script_agent_definition(&tester, "tester").to_str().unwrap(),
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
            "--coder-agent",
            script_agent_definition(&coder, "coder").to_str().unwrap(),
            "--reviewer-agent",
            script_agent_definition(&reviewer, "reviewer")
                .to_str()
                .unwrap(),
            "--tester-agent",
            script_agent_definition(&tester, "tester").to_str().unwrap(),
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
            "--coder-agent",
            script_agent_definition(&coder, "coder").to_str().unwrap(),
            "--reviewer-agent",
            script_agent_definition(&reviewer, "reviewer")
                .to_str()
                .unwrap(),
            "--tester-agent",
            script_agent_definition(&tester, "tester").to_str().unwrap(),
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
            "--coder-agent",
            script_agent_definition(&coder, "coder").to_str().unwrap(),
            "--reviewer-agent",
            script_agent_definition(&reviewer, "reviewer")
                .to_str()
                .unwrap(),
            "--tester-agent",
            script_agent_definition(&tester, "tester").to_str().unwrap(),
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
        assert!(
            worktree_path.exists(),
            "precondition: orphan worktree exists on disk"
        );

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
            "--coder-agent",
            script_agent_definition(&coder, "coder").to_str().unwrap(),
            "--reviewer-agent",
            script_agent_definition(&reviewer, "reviewer")
                .to_str()
                .unwrap(),
            "--tester-agent",
            script_agent_definition(&tester, "tester").to_str().unwrap(),
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
    let open_processes = warden::db::list_open_agent_processes_for_run(&pool, "orphan-e2e-run")
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
            "--coder-agent",
            script_agent_definition(&coder, "coder").to_str().unwrap(),
            "--reviewer-agent",
            script_agent_definition(&reviewer, "reviewer")
                .to_str()
                .unwrap(),
            "--tester-agent",
            script_agent_definition(&tester, "tester").to_str().unwrap(),
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

/// A `--coder-agent` pointing at a file that isn't there must be a clean CLI
/// error naming the path, not a panic — the realistic misuse (a typo, a
/// moved file) now that a role is configured by file rather than by string.
#[test]
fn e2e_a_missing_agent_definition_is_a_clean_cli_error_not_a_panic() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let definitions = TempDir::new().unwrap();
    let missing = definitions.path().join("coder.agent.md");

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
            "--coder-agent",
            missing.to_str().unwrap(),
            "--reviewer-agent",
            noop_agent_definition(definitions.path(), "reviewer")
                .to_str()
                .unwrap(),
            "--tester-agent",
            noop_agent_definition(definitions.path(), "tester")
                .to_str()
                .unwrap(),
        ])
        .assert()
        .failure()
        .stderr(contains("failed to read agent definition"))
        .stderr(contains("coder.agent.md"));
}

/// ADR-0013 / Q3 through the real entry point: an unknown frontmatter key is
/// rejected outright (`deny_unknown_fields`), naming the offending key —
/// never silently ignored, which would run an agent configured differently
/// than its author asked for.
#[test]
fn e2e_an_agent_definition_with_an_unknown_key_is_a_clean_cli_error() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let definitions = TempDir::new().unwrap();
    let bad = definitions.path().join("coder.agent.md");
    std::fs::write(
        &bad,
        "+++\nrunner = \"command\"\nprogram = \"sh\"\nmodel = \"opus\"\n+++\n\nbe a coder\n",
    )
    .unwrap();

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
            "--coder-agent",
            bad.to_str().unwrap(),
            "--reviewer-agent",
            noop_agent_definition(definitions.path(), "reviewer")
                .to_str()
                .unwrap(),
            "--tester-agent",
            noop_agent_definition(definitions.path(), "tester")
                .to_str()
                .unwrap(),
        ])
        .assert()
        .failure()
        .stderr(contains("invalid agent definition"))
        .stderr(contains("model"));
}

/// ADR-0013 / Q3: a definition whose markdown body says nothing configures
/// nothing — a typed error at the boundary, not an agent silently invoked
/// with an empty system prompt.
#[test]
fn e2e_an_agent_definition_with_a_blank_system_prompt_is_a_clean_cli_error() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let definitions = TempDir::new().unwrap();
    let blank = definitions.path().join("coder.agent.md");
    std::fs::write(
        &blank,
        "+++\nrunner = \"command\"\nprogram = \"sh\"\n+++\n\n   \n",
    )
    .unwrap();

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
            "--coder-agent",
            blank.to_str().unwrap(),
            "--reviewer-agent",
            noop_agent_definition(definitions.path(), "reviewer")
                .to_str()
                .unwrap(),
            "--tester-agent",
            noop_agent_definition(definitions.path(), "tester")
                .to_str()
                .unwrap(),
        ])
        .assert()
        .failure()
        .stderr(contains("system prompt"));
}

/// Re-test cycle (issue #20 review fix, fdcaa4e), M2 intent: warden must
/// never emit a payload its own parser would reject, and that starts with
/// rejecting a blank `--intent` at the CLI boundary -- before any `runs`
/// row is written, not deep inside the first cycle when
/// `AgentInputMessage::for_coder` builds the coder's stdin payload. Driven
/// through the real CLI entry point (`Cli::parse()`'s `parse_intent` value
/// parser), not a direct call into `AgentInputMessage::for_coder`.
#[test]
fn e2e_blank_intent_is_a_clean_cli_error_and_creates_no_run_row() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let definitions = TempDir::new().unwrap();

    Command::cargo_bin("warden")
        .unwrap()
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--coder-agent",
            noop_agent_definition(definitions.path(), "coder")
                .to_str()
                .unwrap(),
            "--reviewer-agent",
            noop_agent_definition(definitions.path(), "reviewer")
                .to_str()
                .unwrap(),
            "--tester-agent",
            noop_agent_definition(definitions.path(), "tester")
                .to_str()
                .unwrap(),
        ])
        .assert()
        .failure()
        .stderr(contains("run intent must not be blank"));

    // No SQLite db should have been created at all: the CLI must reject the
    // blank intent during arg parsing, before `main.rs::run` ever opens
    // (and migrates) the state db.
    let db_path = warden_home.path().join("state.db");
    assert!(
        !db_path.exists(),
        "a blank --intent must be rejected before any state db (let alone a run row) is created"
    );
}

/// Whitespace-only intent is equally blank (`str::trim`), not merely a
/// literal empty string -- covers the exact validation rule
/// `AgentInputMessage::for_coder` mirrors on the construction side.
#[test]
fn e2e_whitespace_only_intent_is_a_clean_cli_error() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let definitions = TempDir::new().unwrap();

    Command::cargo_bin("warden")
        .unwrap()
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "   \t  ",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--coder-agent",
            noop_agent_definition(definitions.path(), "coder")
                .to_str()
                .unwrap(),
            "--reviewer-agent",
            noop_agent_definition(definitions.path(), "reviewer")
                .to_str()
                .unwrap(),
            "--tester-agent",
            noop_agent_definition(definitions.path(), "tester")
                .to_str()
                .unwrap(),
        ])
        .assert()
        .failure()
        .stderr(contains("run intent must not be blank"));
}

/// Re-test cycle (issue #20 review fix, fdcaa4e), H1 intent, driven through
/// the real CLI end-to-end: a coder that never reads stdin at all and exits
/// immediately must not fail the run, even when the stdin payload is large
/// enough (over a typical 64KiB OS pipe buffer) to guarantee the write is
/// still in flight when the agent exits and closes its read end (a genuine
/// broken pipe, not a small payload that just happens to fit unread in the
/// buffer). The existing e2e fixtures' reviewer/tester scripts also ignore
/// stdin, but their payloads are tiny (a short diff), so they never
/// actually exercise this path -- this test forces it with a >64KiB
/// `--intent`.
#[tokio::test]
async fn e2e_coder_ignoring_a_large_stdin_payload_and_exiting_immediately_still_converges() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let scripts_dir = TempDir::new().unwrap();

    // Never reads stdin at all; commits immediately and exits.
    let coder = write_script(
        scripts_dir.path(),
        "coder.sh",
        r#"#!/bin/sh
echo hello >> notes.txt
git add notes.txt
git -c user.email=test@warden.local -c user.name=warden-test commit -q -m "coder cycle"
exit 0
"#,
    );
    let reviewer = write_script(scripts_dir.path(), "reviewer.sh", "#!/bin/sh\ntrue\n");
    let tester = write_script(scripts_dir.path(), "tester.sh", "#!/bin/sh\ntrue\n");

    // Comfortably over a typical 64KiB pipe buffer.
    let large_intent = format!("large intent payload: {}", "x".repeat(200_000));

    Command::cargo_bin("warden")
        .unwrap()
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            &large_intent,
            "--branch",
            "main",
            "--max-cycles",
            "3",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--coder-agent",
            script_agent_definition(&coder, "coder").to_str().unwrap(),
            "--reviewer-agent",
            script_agent_definition(&reviewer, "reviewer")
                .to_str()
                .unwrap(),
            "--tester-agent",
            script_agent_definition(&tester, "tester").to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));
}

/// A `--repo` that isn't a git repository must fail cleanly with an
/// actionable error rather than a panic or a silently-created worktree.
#[test]
fn e2e_non_git_repo_path_is_a_clean_cli_error() {
    let not_a_repo = TempDir::new().unwrap();
    let warden_home = TempDir::new().unwrap();
    let definitions = TempDir::new().unwrap();

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
            "--coder-agent",
            noop_agent_definition(definitions.path(), "coder")
                .to_str()
                .unwrap(),
            "--reviewer-agent",
            noop_agent_definition(definitions.path(), "reviewer")
                .to_str()
                .unwrap(),
            "--tester-agent",
            noop_agent_definition(definitions.path(), "tester")
                .to_str()
                .unwrap(),
        ])
        .assert()
        .failure();
}

// ---------------------------------------------------------------------
// Issue #7 (ADR-0009): Evidence Capture Adapter.
//
// The real `playwright`/`asciinema` binaries are neither installed in
// this environment nor safe to invoke from a deterministic test (real
// Playwright execution needs `npx` to fetch a package over the network;
// see code-standards.md "Tests déterministes: ... pas d'appel réseau
// externe"). These tests instead put minimal fake `npx`/`asciinema`
// executables first on `PATH` for the `warden` process (and everything
// it spawns) -- the exact same `process::spawn` -> PATH lookup -> real
// subprocess code path production code takes, just with a stand-in tool
// binary. `git`/`sh` still resolve normally, since the fakes are
// prepended onto (not substituted for) the real `PATH`.
// ---------------------------------------------------------------------

/// Writes an executable script to `dir/<name>` (chmod +x, `#!/bin/sh`
/// shebang), suitable for direct PATH-lookup invocation (unlike
/// `write_script`, whose callers always invoke `sh <path>` explicitly).
#[cfg(unix)]
fn write_fake_tool(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = write_script(dir, name, body);
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

/// Stands in for the real `asciinema` binary: `AsciinemaAdapter` always
/// passes the destination `.cast` path as the last argument
/// (`asciinema rec --quiet --overwrite --command <cmd> <output>`), so this
/// just writes a minimal cast-shaped file there and exits 0.
#[cfg(unix)]
fn write_fake_asciinema(dir: &Path) -> PathBuf {
    write_fake_tool(
        dir,
        "asciinema",
        r#"#!/bin/sh
for arg in "$@"; do
    output="$arg"
done
echo '{"version": 2, "width": 80, "height": 24, "timestamp": 0}' > "$output"
exit 0
"#,
    )
}

/// Stands in for `npx --yes playwright test --reporter=list`:
/// `PlaywrightAdapter` only cares that the command exits 0 and that
/// `test-results/` contains files with a recognized image/video extension
/// afterwards, so this writes exactly that.
#[cfg(unix)]
fn write_fake_npx(dir: &Path) -> PathBuf {
    write_fake_tool(
        dir,
        "npx",
        r#"#!/bin/sh
mkdir -p test-results/example-spec
printf 'fake-png-bytes' > test-results/example-spec/screenshot.png
exit 0
"#,
    )
}

/// `fake_bin_dir` prepended onto the current process's real `PATH`, so a
/// fake tool placed there is found first while `git`/`sh`/coreutils still
/// resolve normally through the rest of the real `PATH`.
#[cfg(unix)]
fn path_with_fake_bin_first(fake_bin_dir: &Path) -> String {
    let real_path = std::env::var("PATH").unwrap_or_default();
    format!("{}:{real_path}", fake_bin_dir.display())
}

/// Acceptance criterion 1 (issue #7, CLI direction): a project with no web
/// markers is classified `Cli`, selecting asciinema. Also covers
/// criterion 3 (`evidence.store_in_repo` defaults to `true`, since
/// `--evidence-store-in-repo` is deliberately omitted here) and criterion 4
/// (the captured artifact is committed under `.warden/evidence/<cycle>/` in
/// a dedicated commit on top of the coder's own commit, only at
/// convergence) and criterion 5 (the `EVIDENCE` row round-trips through
/// SQLite) -- all driven through the real `warden run` CLI entry point.
#[cfg(unix)]
#[tokio::test]
async fn e2e_cli_project_selects_asciinema_and_evidence_is_stored_and_committed_by_default() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let scripts_dir = TempDir::new().unwrap();
    let fake_bin_dir = TempDir::new().unwrap();
    write_fake_asciinema(fake_bin_dir.path());
    let path = path_with_fake_bin_first(fake_bin_dir.path());

    // No package.json, no web marker files anywhere in the repo -> Cli.
    let coder = write_script(
        scripts_dir.path(),
        "coder.sh",
        "#!/bin/sh\necho hi >> notes.txt\ngit add notes.txt\ngit -c user.email=test@warden.local -c user.name=warden-test commit -q -m \"coder cycle\"\n",
    );
    let reviewer = write_script(scripts_dir.path(), "reviewer.sh", "#!/bin/sh\ntrue\n");
    let tester = write_script(scripts_dir.path(), "tester.sh", "#!/bin/sh\ntrue\n");

    let assert = Command::cargo_bin("warden")
        .unwrap()
        .env("PATH", &path)
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "cli project captures evidence via asciinema",
            "--branch",
            "main",
            "--max-cycles",
            "3",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--coder-agent",
            script_agent_definition(&coder, "coder").to_str().unwrap(),
            "--reviewer-agent",
            script_agent_definition(&reviewer, "reviewer")
                .to_str()
                .unwrap(),
            "--tester-agent",
            script_agent_definition(&tester, "tester").to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let run_id = extract_run_id(&stdout);

    let db_path = warden_home.path().join("state.db");
    let pool = warden::db::connect(&db_path).await.unwrap();

    let evidence = warden::db::list_evidence_for_run(&pool, &run_id)
        .await
        .unwrap();
    assert_eq!(
        evidence.len(),
        1,
        "expected one evidence row captured by the fake asciinema tool"
    );
    assert_eq!(
        evidence[0].evidence.evidence_type,
        warden_core::EvidenceType::Other
    );
    assert_eq!(
        evidence[0].evidence.file_path,
        ".warden/evidence/1/session.cast"
    );

    // store_in_repo defaults to true (--evidence-store-in-repo omitted):
    // the artifact must be committed under .warden/evidence/<cycle>/, in a
    // dedicated commit layered on top of the coder's own commit.
    let run = warden::db::get_run(&pool, &run_id).await.unwrap().unwrap();
    let converged_sha = run
        .converged_commit_sha
        .expect("a converged run has a persisted commit sha");

    let show = SyncCommand::new("git")
        .current_dir(repo.path())
        .args([
            "show",
            &format!("{converged_sha}:.warden/evidence/1/session.cast"),
        ])
        .output()
        .unwrap();
    assert!(
        show.status.success(),
        "expected .warden/evidence/1/session.cast inside the converged commit"
    );

    let (cycle_sha,): (Option<String>,) =
        sqlx::query_as("SELECT coder_commit_sha FROM cycles WHERE run_id = ?")
            .bind(&run_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_ne!(
        Some(converged_sha.as_str()),
        cycle_sha.as_deref(),
        "the converged commit must be a distinct evidence commit on top of the coder's own commit"
    );

    // Never merged/checked out into the user's own working tree.
    let status = SyncCommand::new("git")
        .current_dir(repo.path())
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    assert!(status.stdout.is_empty());
}

/// Acceptance criterion 1 (issue #7, web direction): a project with a known
/// web marker file (`index.html`, present in the tester's own worktree
/// since it's checked out at the coder's commit) is classified `Web`,
/// selecting Playwright -- the mirror image of the asciinema/Cli test
/// above.
#[cfg(unix)]
#[tokio::test]
async fn e2e_web_project_marker_selects_playwright_and_evidence_is_committed() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let scripts_dir = TempDir::new().unwrap();
    let fake_bin_dir = TempDir::new().unwrap();
    write_fake_npx(fake_bin_dir.path());
    let path = path_with_fake_bin_first(fake_bin_dir.path());

    let coder = write_script(
        scripts_dir.path(),
        "coder.sh",
        "#!/bin/sh\necho '<html></html>' > index.html\ngit add index.html\ngit -c user.email=test@warden.local -c user.name=warden-test commit -q -m \"coder cycle\"\n",
    );
    let reviewer = write_script(scripts_dir.path(), "reviewer.sh", "#!/bin/sh\ntrue\n");
    let tester = write_script(scripts_dir.path(), "tester.sh", "#!/bin/sh\ntrue\n");

    let assert = Command::cargo_bin("warden")
        .unwrap()
        .env("PATH", &path)
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "web project captures evidence via playwright",
            "--branch",
            "main",
            "--max-cycles",
            "3",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--coder-agent",
            script_agent_definition(&coder, "coder").to_str().unwrap(),
            "--reviewer-agent",
            script_agent_definition(&reviewer, "reviewer")
                .to_str()
                .unwrap(),
            "--tester-agent",
            script_agent_definition(&tester, "tester").to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let run_id = extract_run_id(&stdout);

    let db_path = warden_home.path().join("state.db");
    let pool = warden::db::connect(&db_path).await.unwrap();
    let evidence = warden::db::list_evidence_for_run(&pool, &run_id)
        .await
        .unwrap();
    assert_eq!(evidence.len(), 1);
    assert_eq!(
        evidence[0].evidence.evidence_type,
        warden_core::EvidenceType::Image
    );
    assert!(evidence[0]
        .evidence
        .file_path
        .starts_with(".warden/evidence/1/"));
    assert!(evidence[0].evidence.file_path.ends_with("screenshot.png"));

    let run = warden::db::get_run(&pool, &run_id).await.unwrap().unwrap();
    let converged_sha = run.converged_commit_sha.unwrap();
    let show = SyncCommand::new("git")
        .current_dir(repo.path())
        .args([
            "show",
            &format!("{converged_sha}:{}", evidence[0].evidence.file_path),
        ])
        .output()
        .unwrap();
    assert!(show.status.success());
    assert_eq!(show.stdout, b"fake-png-bytes".to_vec());
}

/// Acceptance criterion 2: `evidence.tool` config override always wins over
/// auto-detection. The repo carries an unambiguous web marker
/// (`index.html`) -- auto-detection alone would select Playwright -- but
/// `--evidence-tool asciinema` must still force the asciinema adapter,
/// observable by the artifact's file name (`session.cast`, which only the
/// asciinema adapter ever produces).
#[cfg(unix)]
#[tokio::test]
async fn e2e_evidence_tool_override_wins_over_web_auto_detection() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let scripts_dir = TempDir::new().unwrap();
    let fake_bin_dir = TempDir::new().unwrap();
    write_fake_asciinema(fake_bin_dir.path());
    let path = path_with_fake_bin_first(fake_bin_dir.path());

    let coder = write_script(
        scripts_dir.path(),
        "coder.sh",
        "#!/bin/sh\necho '<html></html>' > index.html\ngit add index.html\ngit -c user.email=test@warden.local -c user.name=warden-test commit -q -m \"coder cycle\"\n",
    );
    let reviewer = write_script(scripts_dir.path(), "reviewer.sh", "#!/bin/sh\ntrue\n");
    let tester = write_script(scripts_dir.path(), "tester.sh", "#!/bin/sh\ntrue\n");

    let assert = Command::cargo_bin("warden")
        .unwrap()
        .env("PATH", &path)
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "override forces asciinema on a web-looking project",
            "--branch",
            "main",
            "--max-cycles",
            "3",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--coder-agent",
            script_agent_definition(&coder, "coder").to_str().unwrap(),
            "--reviewer-agent",
            script_agent_definition(&reviewer, "reviewer")
                .to_str()
                .unwrap(),
            "--tester-agent",
            script_agent_definition(&tester, "tester").to_str().unwrap(),
            "--evidence-tool",
            "asciinema",
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let run_id = extract_run_id(&stdout);

    let db_path = warden_home.path().join("state.db");
    let pool = warden::db::connect(&db_path).await.unwrap();
    let evidence = warden::db::list_evidence_for_run(&pool, &run_id)
        .await
        .unwrap();
    assert_eq!(evidence.len(), 1);
    assert_eq!(
        evidence[0].evidence.file_path, ".warden/evidence/1/session.cast",
        "the config override must dispatch to asciinema, not Playwright, despite the web marker file"
    );
}

/// Acceptance criteria 3/4: `evidence.store_in_repo` can be turned off
/// (`--evidence-store-in-repo false`), in which case a captured artifact
/// stays on local scratch storage only -- never committed into the repo,
/// and the converged commit stays exactly the coder's own commit.
#[cfg(unix)]
#[tokio::test]
async fn e2e_evidence_store_in_repo_false_keeps_evidence_local_and_never_commits_it() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let scripts_dir = TempDir::new().unwrap();
    let fake_bin_dir = TempDir::new().unwrap();
    write_fake_asciinema(fake_bin_dir.path());
    let path = path_with_fake_bin_first(fake_bin_dir.path());
    let (coder, reviewer, tester) = always_converging_scripts(scripts_dir.path());

    let assert = Command::cargo_bin("warden")
        .unwrap()
        .env("PATH", &path)
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "evidence stays local when store-in-repo is disabled",
            "--branch",
            "main",
            "--max-cycles",
            "3",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--coder-agent",
            script_agent_definition(&coder, "coder").to_str().unwrap(),
            "--reviewer-agent",
            script_agent_definition(&reviewer, "reviewer")
                .to_str()
                .unwrap(),
            "--tester-agent",
            script_agent_definition(&tester, "tester").to_str().unwrap(),
            "--evidence-store-in-repo",
            "false",
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let run_id = extract_run_id(&stdout);

    let db_path = warden_home.path().join("state.db");
    let pool = warden::db::connect(&db_path).await.unwrap();

    // Still captured locally, regardless of store_in_repo.
    let evidence = warden::db::list_evidence_for_run(&pool, &run_id)
        .await
        .unwrap();
    assert_eq!(evidence.len(), 1);
    let scratch_path = warden_home
        .path()
        .join("evidence")
        .join(&run_id)
        .join("1")
        .join("session.cast");
    assert!(
        scratch_path.exists(),
        "evidence must still be staged on local scratch storage: {}",
        scratch_path.display()
    );

    // Never committed into the repo: no evidence ref exists, and the
    // converged commit is exactly the coder's own commit.
    let ref_lookup = SyncCommand::new("git")
        .current_dir(repo.path())
        .args(["rev-parse", &format!("refs/warden/runs/{run_id}/evidence")])
        .output()
        .unwrap();
    assert!(
        !ref_lookup.status.success(),
        "no evidence commit/ref may exist when store_in_repo is false"
    );

    let run = warden::db::get_run(&pool, &run_id).await.unwrap().unwrap();
    let converged_sha = run.converged_commit_sha.unwrap();
    let (cycle_sha,): (Option<String>,) =
        sqlx::query_as("SELECT coder_commit_sha FROM cycles WHERE run_id = ?")
            .bind(&run_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        Some(converged_sha.as_str()),
        cycle_sha.as_deref(),
        "with store_in_repo=false the converged commit must be exactly the coder's commit"
    );
}

/// Acceptance criterion 7: "a missing/failing evidence tool is non-fatal --
/// a converging run still converges", driven end-to-end through the real
/// CLI with genuinely no `asciinema`/Playwright tooling on `PATH` (this
/// sandbox does not have either installed -- see the environment notes in
/// the test report).
#[tokio::test]
async fn e2e_evidence_capture_failure_when_tool_missing_is_non_fatal_and_run_still_converges() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let scripts_dir = TempDir::new().unwrap();
    let (coder, reviewer, tester) = always_converging_scripts(scripts_dir.path());

    // No web markers -> Cli -> asciinema selected, and asciinema is not
    // installed in this environment, so capture must fail but the run must
    // still converge.
    Command::cargo_bin("warden")
        .unwrap()
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "converges even though no evidence tool is installed",
            "--branch",
            "main",
            "--max-cycles",
            "3",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--coder-agent",
            script_agent_definition(&coder, "coder").to_str().unwrap(),
            "--reviewer-agent",
            script_agent_definition(&reviewer, "reviewer")
                .to_str()
                .unwrap(),
            "--tester-agent",
            script_agent_definition(&tester, "tester").to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"))
        .stdout(contains("evidence capture failed"));

    let db_path = warden_home.path().join("state.db");
    let pool = warden::db::connect(&db_path).await.unwrap();
    let (run_id,): (String,) = sqlx::query_as("SELECT id FROM runs LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();

    let evidence = warden::db::list_evidence_for_run(&pool, &run_id)
        .await
        .unwrap();
    assert!(
        evidence.is_empty(),
        "no evidence row should exist when the capture tool is unavailable"
    );

    let run = warden::db::get_run(&pool, &run_id).await.unwrap().unwrap();
    assert_eq!(run.state, RunState::Converged);
}

// ---------------------------------------------------------------------------
// Issue #20 Scope B / ADR-0012: run-intent propagation to spawned agents over
// a warden-managed stdin channel. These drive the real `warden` binary,
// which spawns real `sh` subprocesses through `process::spawn`/`wait` — the
// same real subprocess/stdin path a real coder/reviewer/tester CLI would be
// invoked through, not a call into `Orchestrator`/`AgentInputMessage`
// directly.
// ---------------------------------------------------------------------------

/// Scope B's core promise: "Feed the run intent to the coder through a
/// warden-managed channel ... so the role no longer depends on the user
/// embedding it." Proven by having the coder subprocess itself read and
/// persist its own stdin, then asserting the parsed payload the *coder saw*
/// (version, role, intent) rather than anything warden's own DB records.
#[tokio::test]
async fn e2e_coder_receives_the_run_intent_on_stdin_as_a_versioned_role_tagged_payload() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let scripts_dir = TempDir::new().unwrap();
    let captures = TempDir::new().unwrap();

    let coder = write_script(
        scripts_dir.path(),
        "coder.sh",
        &format!(
            r#"#!/bin/sh
cat > "{captures}/coder_stdin.json"
echo hello >> notes.txt
git add notes.txt
git -c user.email=test@warden.local -c user.name=warden-test commit -q -m "coder cycle"
"#,
            captures = captures.path().display()
        ),
    );
    let reviewer = write_script(scripts_dir.path(), "reviewer.sh", "#!/bin/sh\ntrue\n");
    let tester = write_script(scripts_dir.path(), "tester.sh", "#!/bin/sh\ntrue\n");

    let intent = "implement the widget described in issue 20 scope B";

    Command::cargo_bin("warden")
        .unwrap()
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            intent,
            "--branch",
            "main",
            "--max-cycles",
            "3",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--coder-agent",
            script_agent_definition(&coder, "coder").to_str().unwrap(),
            "--reviewer-agent",
            script_agent_definition(&reviewer, "reviewer")
                .to_str()
                .unwrap(),
            "--tester-agent",
            script_agent_definition(&tester, "tester").to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    let raw = std::fs::read_to_string(captures.path().join("coder_stdin.json"))
        .expect("coder must have received a stdin payload to capture");
    let payload: serde_json::Value = serde_json::from_str(&raw).unwrap_or_else(|error| {
        panic!("coder's captured stdin was not valid JSON: {error}\nraw: {raw:?}")
    });

    // ADR-0013: version 2 -- every payload now carries the role's
    // `system_prompt` from its markdown definition, and a coder payload may
    // carry the findings it must fix (none here: this run converges on its
    // first cycle, so nothing triggered it).
    assert_eq!(payload["version"], 2);
    assert_eq!(payload["role"], "coder");
    assert_eq!(payload["system_prompt"], "You are Warden's coder.");
    assert_eq!(payload["intent"], intent);
    assert!(payload["target_commit"].is_null());
    assert!(payload["diff"].is_null());
    assert_eq!(payload["findings"].as_array().unwrap().len(), 0);
}

/// Scope B's second promise: "Feed the reviewer/tester their context (target
/// commit/diff, prior-cycle findings ...) the same way", plus role identity
/// ("Inject the role identity into the payload so a single runner can serve
/// all three roles"). Proven by having both real reviewer and tester
/// subprocesses capture their own stdin and asserting target_commit/diff/role
/// against what warden itself recorded for that cycle (`cycles.coder_commit_sha`)
/// and the diff the coder actually introduced.
#[tokio::test]
async fn e2e_reviewer_and_tester_receive_target_commit_diff_and_role_on_stdin() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let scripts_dir = TempDir::new().unwrap();
    let captures = TempDir::new().unwrap();

    let coder = write_script(
        scripts_dir.path(),
        "coder.sh",
        r#"#!/bin/sh
echo "distinctive-marker-line" >> notes.txt
git add notes.txt
git -c user.email=test@warden.local -c user.name=warden-test commit -q -m "coder cycle"
"#,
    );
    let reviewer = write_script(
        scripts_dir.path(),
        "reviewer.sh",
        &format!(
            "#!/bin/sh\ncat > \"{}/reviewer_stdin.json\"\ntrue\n",
            captures.path().display()
        ),
    );
    let tester = write_script(
        scripts_dir.path(),
        "tester.sh",
        &format!(
            "#!/bin/sh\ncat > \"{}/tester_stdin.json\"\ntrue\n",
            captures.path().display()
        ),
    );

    let assert = Command::cargo_bin("warden")
        .unwrap()
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "single converging cycle for stdin context propagation",
            "--branch",
            "main",
            "--max-cycles",
            "3",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--coder-agent",
            script_agent_definition(&coder, "coder").to_str().unwrap(),
            "--reviewer-agent",
            script_agent_definition(&reviewer, "reviewer")
                .to_str()
                .unwrap(),
            "--tester-agent",
            script_agent_definition(&tester, "tester").to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let run_id = extract_run_id(&stdout);

    let db_path = warden_home.path().join("state.db");
    let pool = warden::db::connect(&db_path).await.unwrap();
    let (expected_commit,): (String,) =
        sqlx::query_as("SELECT coder_commit_sha FROM cycles WHERE run_id = ?")
            .bind(&run_id)
            .fetch_one(&pool)
            .await
            .unwrap();

    for (role, file) in [
        ("reviewer", "reviewer_stdin.json"),
        ("tester", "tester_stdin.json"),
    ] {
        let raw = std::fs::read_to_string(captures.path().join(file))
            .unwrap_or_else(|error| panic!("{role} must have captured a stdin payload: {error}"));
        let payload: serde_json::Value = serde_json::from_str(&raw)
            .unwrap_or_else(|error| panic!("{role}'s captured stdin was not valid JSON: {error}"));

        assert_eq!(payload["version"], 2);
        assert_eq!(payload["role"], role);
        assert_eq!(
            payload["system_prompt"],
            format!("You are Warden's {role}."),
            "{role} must receive its own definition's system prompt (ADR-0013)"
        );
        assert!(payload["intent"].is_null());
        assert_eq!(
            payload["target_commit"], expected_commit,
            "{role} must receive the exact commit warden recorded for this cycle"
        );
        assert!(
            payload["diff"]
                .as_str()
                .unwrap()
                .contains("distinctive-marker-line"),
            "{role}'s diff must contain the change the coder actually introduced"
        );
        assert_eq!(payload["findings"].as_array().unwrap().len(), 0);
    }
}

/// Scope B's third promise: prior-cycle findings (the ones that triggered a
/// reboucle) must reach the *next* cycle's agents over this same channel —
/// not just be recorded in SQLite. A naive implementation could easily
/// thread the run intent/commit/diff through correctly while leaving
/// `findings` empty or stale; this is the case the task flags as "easy to
/// wire wrong". Extended for A2 (ADR-0013, issue #22): the coder — the role
/// that must actually fix them — is now fed the same list, so it captures
/// its payloads here too. All three agents capture every stdin payload they
/// receive (one per cycle, via a counter file) so this test can inspect
/// cycle 1's (no prior findings) and cycle 2's (the reboucle-triggering
/// finding from cycle 1) payloads independently.
#[tokio::test]
async fn e2e_prior_cycle_findings_from_a_reboucle_reach_the_next_cycles_agents_stdin() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let scripts_dir = TempDir::new().unwrap();
    let captures = TempDir::new().unwrap();

    // Deterministic two-cycle reboucle, mirroring the orchestrator unit
    // test's `flip_status_coder`/`status_gated_reviewer` fixtures: cycle 1
    // leaves status.txt "broken" (reviewer blocks), cycle 2 leaves it
    // "fixed" (reviewer passes).
    let coder = write_script(
        scripts_dir.path(),
        "coder.sh",
        &format!(
            r#"#!/bin/sh
INPUT=$(cat)
N=$(ls "{captures}"/coder_stdin_*.json 2>/dev/null | wc -l | tr -d ' ')
NEXT=$((N + 1))
printf '%s' "$INPUT" > "{captures}/coder_stdin_$NEXT.json"
if [ -f status.txt ] && [ "$(cat status.txt)" = "broken" ]; then
    echo fixed > status.txt
else
    echo broken > status.txt
fi
git add status.txt
git -c user.email=test@warden.local -c user.name=warden-test commit -q -m "coder cycle"
"#,
            captures = captures.path().display()
        ),
    );
    let reviewer = write_script(
        scripts_dir.path(),
        "reviewer.sh",
        &format!(
            r#"#!/bin/sh
INPUT=$(cat)
N=$(ls "{captures}"/reviewer_stdin_*.json 2>/dev/null | wc -l | tr -d ' ')
NEXT=$((N + 1))
printf '%s' "$INPUT" > "{captures}/reviewer_stdin_$NEXT.json"
if [ -f status.txt ] && [ "$(cat status.txt)" = "broken" ]; then
    echo '{{"source":"reviewer","severity":"blocking","description":"status is broken"}}'
fi
"#,
            captures = captures.path().display()
        ),
    );
    let tester = write_script(
        scripts_dir.path(),
        "tester.sh",
        &format!(
            r#"#!/bin/sh
INPUT=$(cat)
N=$(ls "{captures}"/tester_stdin_*.json 2>/dev/null | wc -l | tr -d ' ')
NEXT=$((N + 1))
printf '%s' "$INPUT" > "{captures}/tester_stdin_$NEXT.json"
true
"#,
            captures = captures.path().display()
        ),
    );

    Command::cargo_bin("warden")
        .unwrap()
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            "flip status to fixed via a reboucle",
            "--branch",
            "main",
            "--max-cycles",
            "5",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--coder-agent",
            script_agent_definition(&coder, "coder").to_str().unwrap(),
            "--reviewer-agent",
            script_agent_definition(&reviewer, "reviewer")
                .to_str()
                .unwrap(),
            "--tester-agent",
            script_agent_definition(&tester, "tester").to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    let cycle1: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(captures.path().join("reviewer_stdin_1.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        cycle1["findings"].as_array().unwrap().len(),
        0,
        "cycle 1 has no prior cycle, so the reviewer must see no prior findings"
    );

    let cycle2: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(captures.path().join("reviewer_stdin_2.json")).unwrap(),
    )
    .unwrap();
    let cycle2_findings = cycle2["findings"].as_array().unwrap();
    assert_eq!(
        cycle2_findings.len(),
        1,
        "cycle 2's reviewer must receive exactly the one finding that triggered the reboucle"
    );
    assert_eq!(cycle2_findings[0]["source"], "reviewer");
    assert_eq!(cycle2_findings[0]["severity"], "blocking");
    assert_eq!(cycle2_findings[0]["description"], "status is broken");
    assert_ne!(
        cycle1["target_commit"], cycle2["target_commit"],
        "cycle 2 must be reviewing a different (later) commit than cycle 1"
    );

    // The tester gets the exact same prior-findings context as the reviewer
    // (code-standards.md / ADR-0012: both roles are fed identically) --
    // proven independently rather than assumed from the reviewer's payload.
    let tester_cycle2: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(captures.path().join("tester_stdin_2.json")).unwrap(),
    )
    .unwrap();
    let tester_cycle2_findings = tester_cycle2["findings"].as_array().unwrap();
    assert_eq!(tester_cycle2_findings.len(), 1);
    assert_eq!(tester_cycle2_findings[0]["description"], "status is broken");

    // A2 (ADR-0013, issue #22): the role that must actually *fix* the
    // findings finally receives them. Cycle 1's coder has nothing to fix;
    // cycle 2's gets exactly the finding that triggered the reboucle -- and
    // still no `target_commit`/`diff`, which it can read from its own
    // worktree.
    let coder_cycle1: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(captures.path().join("coder_stdin_1.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        coder_cycle1["findings"].as_array().unwrap().len(),
        0,
        "cycle 1 has no prior cycle, so the coder must see no findings to fix"
    );

    let coder_cycle2: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(captures.path().join("coder_stdin_2.json")).unwrap(),
    )
    .unwrap();
    let coder_cycle2_findings = coder_cycle2["findings"].as_array().unwrap();
    assert_eq!(
        coder_cycle2_findings.len(),
        1,
        "cycle 2's coder must receive exactly the finding it is being asked to fix"
    );
    assert_eq!(coder_cycle2_findings[0]["source"], "reviewer");
    assert_eq!(coder_cycle2_findings[0]["severity"], "blocking");
    assert_eq!(coder_cycle2_findings[0]["description"], "status is broken");
    assert_eq!(
        coder_cycle2["intent"], "flip status to fixed via a reboucle",
        "the run intent must still reach the coder alongside its findings"
    );
    assert!(
        coder_cycle2["target_commit"].is_null(),
        "A2: the coder gets intent + findings only, never a target_commit"
    );
    assert!(
        coder_cycle2["diff"].is_null(),
        "A2: the coder reads its own worktree's diff rather than being sent one"
    );
}

/// Negative counterpart of the above (Architecture.md §10, "Isolation
/// environnement des sous-processus"): the run intent must reach the agent
/// *only* over the stdin channel, never via an inherited/synthesized
/// environment variable or as a CLI argument. `process::spawn` already
/// `env_clear()`s and only ever passes `PATH` through, and `--coder-cmd`'s
/// args are a fixed, user-supplied command line the intent is never spliced
/// into -- this test proves both from the coder subprocess's own point of
/// view (its real `env` output and its real `$0 $*`), with the stdin capture
/// alongside as a positive control so this isn't a vacuous "found nothing
/// because nothing was captured" negative.
#[tokio::test]
async fn e2e_run_intent_never_leaks_via_environment_variables_or_argv() {
    let repo = init_test_repo();
    let warden_home = TempDir::new().unwrap();
    let scripts_dir = TempDir::new().unwrap();
    let captures = TempDir::new().unwrap();

    let coder = write_script(
        scripts_dir.path(),
        "coder.sh",
        &format!(
            r#"#!/bin/sh
env > "{captures}/coder_env.txt"
printf '%s' "$0 $*" > "{captures}/coder_argv.txt"
cat > "{captures}/coder_stdin.json"
echo hello >> notes.txt
git add notes.txt
git -c user.email=test@warden.local -c user.name=warden-test commit -q -m "coder cycle"
"#,
            captures = captures.path().display()
        ),
    );
    let reviewer = write_script(scripts_dir.path(), "reviewer.sh", "#!/bin/sh\ntrue\n");
    let tester = write_script(scripts_dir.path(), "tester.sh", "#!/bin/sh\ntrue\n");

    let marker = "WARDEN_SECRET_INTENT_MARKER_9f3d21";
    let intent = format!("do the thing ({marker})");

    Command::cargo_bin("warden")
        .unwrap()
        .args([
            "run",
            "--repo",
            repo.path().to_str().unwrap(),
            "--intent",
            &intent,
            "--branch",
            "main",
            "--max-cycles",
            "3",
            "--warden-home",
            warden_home.path().to_str().unwrap(),
            "--coder-agent",
            script_agent_definition(&coder, "coder").to_str().unwrap(),
            "--reviewer-agent",
            script_agent_definition(&reviewer, "reviewer")
                .to_str()
                .unwrap(),
            "--tester-agent",
            script_agent_definition(&tester, "tester").to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(contains("finished: Converged"));

    let env_dump = std::fs::read_to_string(captures.path().join("coder_env.txt")).unwrap();
    assert!(
        env_dump.contains("PATH="),
        "sanity check that the env dump actually captured something: {env_dump:?}"
    );
    assert!(
        !env_dump.contains(marker),
        "the run intent must never leak into the coder's environment variables: {env_dump:?}"
    );

    let argv_dump = std::fs::read_to_string(captures.path().join("coder_argv.txt")).unwrap();
    assert!(
        !argv_dump.contains(marker),
        "the run intent must never leak into the coder's argv: {argv_dump:?}"
    );

    // Positive control: the same marker must have arrived over stdin, so an
    // empty/broken capture above wouldn't be mistaken for success.
    let stdin_dump = std::fs::read_to_string(captures.path().join("coder_stdin.json")).unwrap();
    let payload: serde_json::Value = serde_json::from_str(&stdin_dump).unwrap();
    assert_eq!(payload["intent"], intent);
}
