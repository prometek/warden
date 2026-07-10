//! The long-running `warden-gated serve` daemon: accepts relayed
//! `post-receive` payloads over a Unix socket, and for every ref update
//! they contain, independently re-verifies the run via the read-only
//! database before ever touching `origin` (ADR-0002/ADR-0006). This is the
//! only place `origin`'s credentials are exercised in the whole workspace.

use std::path::PathBuf;

use sqlx::SqlitePool;

use crate::db;
use crate::error::Result;
use crate::gate::verify_and_authorize;
use crate::notification::parse_post_receive_line;
use crate::push;
use crate::relay;
use crate::verify::GateDecision;

/// Static configuration for one `serve` invocation.
pub struct ServeConfig {
    /// Unix socket the `notify` relay (invoked from the `post-receive`
    /// hook) connects to.
    pub socket_path: PathBuf,
    /// `warden`'s SQLite database, opened **read-only** (never created,
    /// never migrated by this crate).
    pub db_path: PathBuf,
    /// The local bare gate repo `warden` pushes converged runs into.
    pub bare_repo_path: PathBuf,
    /// Branch name to update on `origin` once a push is authorized.
    pub target_branch: String,
}

/// Runs the accept loop forever (until the process is killed/stopped by its
/// managed service, e.g. systemd/launchd -- see `contrib/`). Each accepted
/// connection is handled to completion before the next `accept()`: hook
/// invocations are not expected to overlap in practice (one push at a
/// time), and serializing them keeps the re-verification/push sequence for
/// a given payload atomic with respect to other notifications.
pub async fn serve(config: ServeConfig) -> Result<()> {
    let pool = db::connect_read_only(&config.db_path).await?;
    let listener = relay::bind(&config.socket_path).await?;

    tracing::info!(
        socket = %config.socket_path.display(),
        db = %config.db_path.display(),
        "warden-gated listening for push notifications"
    );

    loop {
        let (mut stream, _addr) = listener.accept().await?;
        let payload = match relay::read_payload(&mut stream).await {
            Ok(payload) => payload,
            Err(error) => {
                tracing::error!(%error, "failed to read a relayed push notification");
                continue;
            }
        };
        handle_payload(&pool, &config, &payload).await;
    }
}

/// Processes one relayed payload (one or more `post-receive` lines): each
/// line is re-verified and, if authorized, pushed independently. A single
/// malformed line, or a single line whose re-verification/push fails, is
/// logged and **skipped** rather than aborting the rest of the batch -- a
/// multi-ref push could legitimately contain one bad ref alongside a good
/// one, and a boundary-validation failure on untrusted input must never
/// take down otherwise-valid work (code-standards.md: "valider ... à la
/// frontière", not "tout annuler à la moindre entrée invalide").
async fn handle_payload(pool: &SqlitePool, config: &ServeConfig, payload: &str) {
    for line in payload.lines().filter(|line| !line.trim().is_empty()) {
        if let Err(error) = handle_push_notification_line(pool, config, line).await {
            tracing::error!(%error, line, "skipping one malformed/failed push-notification line");
        }
    }
}

/// Re-verifies and, if authorized, pushes the single ref update described by
/// one `post-receive` line.
async fn handle_push_notification_line(
    pool: &SqlitePool,
    config: &ServeConfig,
    line: &str,
) -> Result<()> {
    let notification = parse_post_receive_line(line)?;
    let decision =
        verify_and_authorize(pool, &notification.run_id, &notification.new_commit_sha).await?;

    match decision {
        GateDecision::Allow { commit_sha } => {
            tracing::info!(
                run_id = %notification.run_id,
                %commit_sha,
                "run converged and hash matches; pushing to origin"
            );
            push::push_to_origin(&config.bare_repo_path, &commit_sha, &config.target_branch)
                .await?;
        }
        GateDecision::Blocked(reason) => {
            tracing::warn!(
                run_id = %notification.run_id,
                ?reason,
                "push blocked: independent re-verification against SQLite failed"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::process::Command as SyncCommand;
    use tempfile::TempDir;
    use warden_core::RunState;

    fn run_git(dir: &std::path::Path, args: &[&str]) {
        let status = SyncCommand::new("git")
            .current_dir(dir)
            .args(args)
            .status()
            .expect("spawn git");
        assert!(status.success(), "git {args:?} failed");
    }

    /// Sets up: a real temp SQLite db seeded with one run row, a bare gate
    /// repo containing `commit_sha` reachable from `refs/heads/<branch>`,
    /// and a fake local `origin` repo the gate can push to without network
    /// access.
    async fn test_fixture(
        state: RunState,
        converged_commit_sha: Option<&str>,
    ) -> (TempDir, TempDir, TempDir, PathBuf, String) {
        let db_dir = TempDir::new().unwrap();
        let db_path = db_dir.path().join("state.db");
        let write_options = SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true);
        let write_pool = SqlitePoolOptions::new()
            .connect_with(write_options)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE runs (id TEXT PRIMARY KEY, state TEXT NOT NULL, converged_commit_sha TEXT)",
        )
        .execute(&write_pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO runs (id, state, converged_commit_sha) VALUES ('run-1', ?, ?)")
            .bind(state.as_str())
            .bind(converged_commit_sha)
            .execute(&write_pool)
            .await
            .unwrap();
        write_pool.close().await;

        let origin = TempDir::new().unwrap();
        run_git(origin.path(), &["init", "--bare", "--quiet"]);

        let seed = TempDir::new().unwrap();
        run_git(seed.path(), &["init", "--quiet"]);
        run_git(seed.path(), &["config", "user.email", "test@warden.local"]);
        run_git(seed.path(), &["config", "user.name", "warden-test"]);
        std::fs::write(seed.path().join("f.txt"), "hi\n").unwrap();
        run_git(seed.path(), &["add", "."]);
        run_git(
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

        let gate_repo = TempDir::new().unwrap();
        run_git(
            gate_repo.path(),
            &[
                "clone",
                "--bare",
                "--quiet",
                &seed.path().display().to_string(),
                ".",
            ],
        );
        // `git clone --bare` already sets up an `origin` remote pointing at
        // its source (the seed repo); repoint it at the fake `origin` repo
        // instead of `remote add`, which would fail with "already exists".
        run_git(
            gate_repo.path(),
            &[
                "remote",
                "set-url",
                "origin",
                &origin.path().display().to_string(),
            ],
        );

        (db_dir, origin, gate_repo, db_path, commit_sha)
    }

    /// Acceptance criterion 1 (issue #3): even though the relayed
    /// notification names the exact commit that (in the fixture) is
    /// physically present in the bare gate repo, the run is genuinely
    /// `CoderRunning` in the real SQLite -- simulating `warden` believing
    /// (or asserting) convergence while the ground truth disagrees. No push
    /// must reach `origin`.
    #[tokio::test]
    async fn handle_payload_blocks_the_push_when_the_real_run_state_is_not_converged() {
        let (_db_dir, origin, gate_repo, db_path, commit_sha) =
            test_fixture(RunState::CoderRunning, None).await;

        let pool = db::connect_read_only(&db_path).await.unwrap();
        let config = ServeConfig {
            socket_path: PathBuf::from("/unused/for/this/test"),
            db_path: db_path.clone(),
            bare_repo_path: gate_repo.path().to_path_buf(),
            target_branch: "main".to_string(),
        };

        let payload = format!("0000000 {commit_sha} refs/heads/warden-run/run-1\n");
        handle_payload(&pool, &config, &payload).await;

        // `origin` must still have no `main` ref at all -- nothing was pushed.
        let output = SyncCommand::new("git")
            .current_dir(origin.path())
            .args(["rev-parse", "--verify", "refs/heads/main"])
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "origin must not have received a push for a non-converged run"
        );
    }

    #[tokio::test]
    async fn handle_payload_pushes_to_origin_when_converged_and_hash_matches() {
        let (_db_dir, origin, gate_repo, db_path, commit_sha) =
            test_fixture(RunState::Converged, None).await;

        // The fixture doesn't know its own commit sha ahead of time (it's
        // computed while building the gate repo), so patch it into the
        // seeded row now that we have it.
        let pool = db::connect_read_only(&db_path).await.unwrap();
        {
            let write_options = SqliteConnectOptions::new().filename(&db_path);
            let write_pool = SqlitePoolOptions::new()
                .connect_with(write_options)
                .await
                .unwrap();
            sqlx::query("UPDATE runs SET converged_commit_sha = ? WHERE id = 'run-1'")
                .bind(&commit_sha)
                .execute(&write_pool)
                .await
                .unwrap();
            write_pool.close().await;
        }

        let config = ServeConfig {
            socket_path: PathBuf::from("/unused/for/this/test"),
            db_path: db_path.clone(),
            bare_repo_path: gate_repo.path().to_path_buf(),
            target_branch: "main".to_string(),
        };

        let payload = format!("0000000 {commit_sha} refs/heads/warden-run/run-1\n");
        handle_payload(&pool, &config, &payload).await;

        let output = SyncCommand::new("git")
            .current_dir(origin.path())
            .args(["log", "-1", "--format=%H", "refs/heads/main"])
            .output()
            .unwrap();
        let origin_head = String::from_utf8_lossy(&output.stdout).trim().to_string();
        assert_eq!(origin_head, commit_sha);
    }

    /// LOW finding (issue #3 review): a malformed line earlier in the same
    /// payload must not prevent a legitimate, later line from still being
    /// processed -- a multi-ref push could contain one bad ref alongside a
    /// good one, and boundary-validation failures are per-line, not
    /// batch-wide.
    #[tokio::test]
    async fn a_malformed_line_does_not_prevent_a_later_valid_line_from_being_pushed() {
        let (_db_dir, origin, gate_repo, db_path, commit_sha) =
            test_fixture(RunState::Converged, None).await;

        let pool = db::connect_read_only(&db_path).await.unwrap();
        {
            let write_options = SqliteConnectOptions::new().filename(&db_path);
            let write_pool = SqlitePoolOptions::new()
                .connect_with(write_options)
                .await
                .unwrap();
            sqlx::query("UPDATE runs SET converged_commit_sha = ? WHERE id = 'run-1'")
                .bind(&commit_sha)
                .execute(&write_pool)
                .await
                .unwrap();
            write_pool.close().await;
        }

        let config = ServeConfig {
            socket_path: PathBuf::from("/unused/for/this/test"),
            db_path: db_path.clone(),
            bare_repo_path: gate_repo.path().to_path_buf(),
            target_branch: "main".to_string(),
        };

        // First line is deliberately malformed (wrong ref prefix); second
        // line is the real, valid, converged notification.
        let payload = format!(
            "old111 new222 refs/heads/not-a-warden-ref\n0000000 {commit_sha} refs/heads/warden-run/run-1\n"
        );
        handle_payload(&pool, &config, &payload).await;

        let output = SyncCommand::new("git")
            .current_dir(origin.path())
            .args(["log", "-1", "--format=%H", "refs/heads/main"])
            .output()
            .unwrap();
        let origin_head = String::from_utf8_lossy(&output.stdout).trim().to_string();
        assert_eq!(
            origin_head, commit_sha,
            "the valid second line must still have been pushed despite the malformed first line"
        );
    }
}
