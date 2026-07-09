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
use warden_core::{AgentRole, RunState};

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
