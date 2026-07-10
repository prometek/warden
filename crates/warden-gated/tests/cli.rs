//! Integration tests against the compiled `warden-gated` binary
//! (code-standards.md: "Tests d'intégration dans `tests/`. `assert_cmd` +
//! `predicates` pour tester les binaires compilés").

use std::path::Path;
use std::process::Command as SyncCommand;

use assert_cmd::Command;
use predicates::prelude::*;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use tempfile::TempDir;

async fn seed_db(
    dir: &Path,
    run_id: &str,
    state: &str,
    converged_commit_sha: Option<&str>,
) -> std::path::PathBuf {
    let db_path = dir.join("state.db");
    let options = SqliteConnectOptions::new()
        .filename(&db_path)
        .create_if_missing(true);
    let pool = SqlitePoolOptions::new()
        .connect_with(options)
        .await
        .unwrap();

    sqlx::query(
        "CREATE TABLE runs (id TEXT PRIMARY KEY, state TEXT NOT NULL, converged_commit_sha TEXT)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO runs (id, state, converged_commit_sha) VALUES (?, ?, ?)")
        .bind(run_id)
        .bind(state)
        .bind(converged_commit_sha)
        .execute(&pool)
        .await
        .unwrap();

    pool.close().await;
    db_path
}

/// Acceptance criterion 1 (issue #3), exercised against the actual compiled
/// binary: a run that is genuinely `coder_running` in the real SQLite file
/// is blocked by `verify-run`, even when the `--commit` argument (standing
/// in for whatever `warden` might claim) names a plausible-looking commit.
#[tokio::test]
async fn verify_run_blocks_and_exits_non_zero_when_the_real_state_is_not_converged() {
    let dir = TempDir::new().unwrap();
    let db_path = seed_db(dir.path(), "run-1", "coder_running", None).await;

    Command::cargo_bin("warden-gated")
        .unwrap()
        .args([
            "verify-run",
            "--db",
            db_path.to_str().unwrap(),
            "--run-id",
            "run-1",
            "--commit",
            "commit-warden-claims-is-converged",
        ])
        .assert()
        .failure()
        .stdout(predicate::str::contains("BLOCKED"))
        .stdout(predicate::str::contains("NotConverged"));
}

#[tokio::test]
async fn verify_run_allows_and_exits_zero_when_converged_and_hash_matches() {
    let dir = TempDir::new().unwrap();
    let db_path = seed_db(dir.path(), "run-1", "converged", Some("abc123")).await;

    Command::cargo_bin("warden-gated")
        .unwrap()
        .args([
            "verify-run",
            "--db",
            db_path.to_str().unwrap(),
            "--run-id",
            "run-1",
            "--commit",
            "abc123",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("ALLOW"));
}

#[tokio::test]
async fn verify_run_fails_loudly_when_the_database_does_not_exist() {
    let dir = TempDir::new().unwrap();
    let missing_db = dir.path().join("does-not-exist.db");

    Command::cargo_bin("warden-gated")
        .unwrap()
        .args([
            "verify-run",
            "--db",
            missing_db.to_str().unwrap(),
            "--run-id",
            "run-1",
            "--commit",
            "abc123",
        ])
        .assert()
        .failure();
}

/// End-to-end: `init-bare` installs a hook script that, when git actually
/// runs it on a real push, relays the notification to a listening `serve`
/// daemon, which independently re-verifies against SQLite and (since the
/// seeded run is not converged) must not push anything to `origin`.
#[tokio::test]
async fn a_real_git_push_through_the_installed_hook_is_blocked_end_to_end() {
    let warden_home = TempDir::new().unwrap();
    let db_path = seed_db(warden_home.path(), "run-1", "coder_running", None).await;

    let bare_repo = warden_home.path().join("gate.git");
    let socket_path = warden_home.path().join("gated.sock");
    let origin = TempDir::new().unwrap();
    run_git_sync(origin.path(), &["init", "--bare", "--quiet"]);

    let bin_path = assert_cmd::cargo::cargo_bin("warden-gated");

    Command::cargo_bin("warden-gated")
        .unwrap()
        .args([
            "init-bare",
            "--bare-repo",
            bare_repo.to_str().unwrap(),
            "--bin",
            bin_path.to_str().unwrap(),
            "--socket",
            socket_path.to_str().unwrap(),
            "--origin-url",
            origin.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    // Start the daemon in the background, targeting the same bare repo/db.
    let mut serve_child = std::process::Command::new(bin_path)
        .args([
            "serve",
            "--socket",
            socket_path.to_str().unwrap(),
            "--db",
            db_path.to_str().unwrap(),
            "--bare-repo",
            bare_repo.to_str().unwrap(),
            "--branch",
            "main",
        ])
        .spawn()
        .unwrap();

    // Give the daemon a moment to bind its socket before pushing.
    wait_for_socket(&socket_path);

    // A seed working copy that pushes into the bare gate repo under the
    // `refs/heads/warden-run/run-1` convention the hook/notification parser
    // expects -- this is what triggers the real `post-receive` hook.
    let seed = TempDir::new().unwrap();
    run_git_sync(seed.path(), &["init", "--quiet"]);
    run_git_sync(seed.path(), &["config", "user.email", "test@warden.local"]);
    run_git_sync(seed.path(), &["config", "user.name", "warden-test"]);
    std::fs::write(seed.path().join("f.txt"), "content\n").unwrap();
    run_git_sync(seed.path(), &["add", "."]);
    run_git_sync(seed.path(), &["commit", "--quiet", "-m", "coder commit"]);
    run_git_sync(
        seed.path(),
        &[
            "push",
            &bare_repo.display().to_string(),
            "HEAD:refs/heads/warden-run/run-1",
        ],
    );

    // Give the hook a moment to relay + the daemon a moment to process.
    std::thread::sleep(std::time::Duration::from_millis(300));

    let origin_head = std::process::Command::new("git")
        .current_dir(origin.path())
        .args(["rev-parse", "--verify", "refs/heads/main"])
        .output()
        .unwrap();
    assert!(
        !origin_head.status.success(),
        "origin must not have received a push for a non-converged run"
    );

    let _ = serve_child.kill();
    let _ = serve_child.wait();
}

/// End-to-end positive case (issue #3, acceptance criterion 2): when the run
/// genuinely is `converged` in the real SQLite and the commit that git
/// actually wrote into the bare gate repo matches `converged_commit_sha`,
/// the real installed `post-receive` hook, relayed to a real running `serve`
/// daemon, must relay the push all the way to `origin`.
#[tokio::test]
async fn a_real_git_push_through_the_installed_hook_reaches_origin_when_converged_and_hash_matches()
{
    let warden_home = TempDir::new().unwrap();

    let bare_repo = warden_home.path().join("gate.git");
    let socket_path = warden_home.path().join("gated.sock");
    let origin = TempDir::new().unwrap();
    run_git_sync(origin.path(), &["init", "--bare", "--quiet"]);

    let bin_path = assert_cmd::cargo::cargo_bin("warden-gated");

    Command::cargo_bin("warden-gated")
        .unwrap()
        .args([
            "init-bare",
            "--bare-repo",
            bare_repo.to_str().unwrap(),
            "--bin",
            bin_path.to_str().unwrap(),
            "--socket",
            socket_path.to_str().unwrap(),
            "--origin-url",
            origin.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    // Build the commit that will be pushed *before* seeding the DB, so the
    // seeded `converged_commit_sha` can be the real sha rather than a
    // guessed value.
    let seed = TempDir::new().unwrap();
    run_git_sync(seed.path(), &["init", "--quiet"]);
    run_git_sync(seed.path(), &["config", "user.email", "test@warden.local"]);
    run_git_sync(seed.path(), &["config", "user.name", "warden-test"]);
    std::fs::write(seed.path().join("f.txt"), "content\n").unwrap();
    run_git_sync(seed.path(), &["add", "."]);
    run_git_sync(
        seed.path(),
        &["commit", "--quiet", "-m", "converged commit"],
    );
    let commit_sha_output = SyncCommand::new("git")
        .current_dir(seed.path())
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    let commit_sha = String::from_utf8_lossy(&commit_sha_output.stdout)
        .trim()
        .to_string();

    let db_path = seed_db(
        warden_home.path(),
        "run-1",
        "converged",
        Some(commit_sha.as_str()),
    )
    .await;

    let mut serve_child = std::process::Command::new(&bin_path)
        .args([
            "serve",
            "--socket",
            socket_path.to_str().unwrap(),
            "--db",
            db_path.to_str().unwrap(),
            "--bare-repo",
            bare_repo.to_str().unwrap(),
            "--branch",
            "main",
        ])
        .spawn()
        .unwrap();

    wait_for_socket(&socket_path);

    run_git_sync(
        seed.path(),
        &[
            "push",
            &bare_repo.display().to_string(),
            "HEAD:refs/heads/warden-run/run-1",
        ],
    );

    std::thread::sleep(std::time::Duration::from_millis(300));

    let origin_head = std::process::Command::new("git")
        .current_dir(origin.path())
        .args(["log", "-1", "--format=%H", "refs/heads/main"])
        .output()
        .unwrap();
    assert!(
        origin_head.status.success(),
        "origin should have received the push for a converged run with a matching hash"
    );
    let origin_head_sha = String::from_utf8_lossy(&origin_head.stdout)
        .trim()
        .to_string();
    assert_eq!(origin_head_sha, commit_sha);

    let _ = serve_child.kill();
    let _ = serve_child.wait();
}

/// End-to-end negative case (issue #3, acceptance criterion 2): the run is
/// genuinely `converged`, but the commit actually written into the bare gate
/// repo by this push does not match the persisted `converged_commit_sha`
/// (standing in for a stale/tampered validated hash). The real installed
/// hook must still not let anything reach `origin`.
#[tokio::test]
async fn a_real_git_push_through_the_installed_hook_is_blocked_when_hash_does_not_match() {
    let warden_home = TempDir::new().unwrap();
    let db_path = seed_db(
        warden_home.path(),
        "run-1",
        "converged",
        Some("some-other-validated-sha-not-what-gets-pushed"),
    )
    .await;

    let bare_repo = warden_home.path().join("gate.git");
    let socket_path = warden_home.path().join("gated.sock");
    let origin = TempDir::new().unwrap();
    run_git_sync(origin.path(), &["init", "--bare", "--quiet"]);

    let bin_path = assert_cmd::cargo::cargo_bin("warden-gated");

    Command::cargo_bin("warden-gated")
        .unwrap()
        .args([
            "init-bare",
            "--bare-repo",
            bare_repo.to_str().unwrap(),
            "--bin",
            bin_path.to_str().unwrap(),
            "--socket",
            socket_path.to_str().unwrap(),
            "--origin-url",
            origin.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let mut serve_child = std::process::Command::new(bin_path)
        .args([
            "serve",
            "--socket",
            socket_path.to_str().unwrap(),
            "--db",
            db_path.to_str().unwrap(),
            "--bare-repo",
            bare_repo.to_str().unwrap(),
            "--branch",
            "main",
        ])
        .spawn()
        .unwrap();

    wait_for_socket(&socket_path);

    let seed = TempDir::new().unwrap();
    run_git_sync(seed.path(), &["init", "--quiet"]);
    run_git_sync(seed.path(), &["config", "user.email", "test@warden.local"]);
    run_git_sync(seed.path(), &["config", "user.name", "warden-test"]);
    std::fs::write(seed.path().join("f.txt"), "content\n").unwrap();
    run_git_sync(seed.path(), &["add", "."]);
    run_git_sync(seed.path(), &["commit", "--quiet", "-m", "coder commit"]);
    run_git_sync(
        seed.path(),
        &[
            "push",
            &bare_repo.display().to_string(),
            "HEAD:refs/heads/warden-run/run-1",
        ],
    );

    std::thread::sleep(std::time::Duration::from_millis(300));

    let origin_head = std::process::Command::new("git")
        .current_dir(origin.path())
        .args(["rev-parse", "--verify", "refs/heads/main"])
        .output()
        .unwrap();
    assert!(
        !origin_head.status.success(),
        "origin must not have received a push when the pushed commit doesn't match the validated hash"
    );

    let _ = serve_child.kill();
    let _ = serve_child.wait();
}

fn run_git_sync(dir: &Path, args: &[&str]) {
    let status = SyncCommand::new("git")
        .current_dir(dir)
        .args(args)
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed");
}

fn wait_for_socket(socket_path: &Path) {
    for _ in 0..50 {
        if socket_path.exists() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!(
        "daemon never created its socket at {}",
        socket_path.display()
    );
}
