//! Ties together the read-only database access (`db.rs`, I/O) and the pure
//! authorization rule (`verify.rs`) into the one entry point both the
//! `serve` daemon and the `verify-run` CLI diagnostic call: independently
//! re-verify a run before authorizing a push (ADR-0002/ADR-0006).

use sqlx::SqlitePool;

use crate::db;
use crate::error::Result;
use crate::verify::{decide, GateDecision};

/// Re-reads `run_id` from the read-only database and decides whether
/// `pushed_commit_sha` (the commit that was actually written into the local
/// bare gate repo -- see `notification::parse_post_receive_line`) may be
/// relayed to `origin`.
///
/// This is the only place `warden-gated` ever consults SQLite to make a
/// push decision, and it never accepts a `RunState`/hash as a parameter --
/// only a `run_id` and the commit sha to check against, both of which are
/// independently verifiable (the run_id names a real row or it doesn't; the
/// commit sha is what git itself just wrote to the bare repo).
pub async fn verify_and_authorize(
    pool: &SqlitePool,
    run_id: &str,
    pushed_commit_sha: &str,
) -> Result<GateDecision> {
    let run = db::get_run_view(pool, run_id).await?;
    Ok(decide(run_id, run.as_ref(), pushed_commit_sha))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use tempfile::TempDir;
    use warden_core::RunState;

    use crate::verify::GateBlockReason;

    /// A real, temporary SQLite database seeded directly with a plain
    /// `INSERT` (code-standards.md: "DB de test: SQLite fichier temporaire
    /// réel, jamais de mock") -- this stands in for the file `warden` would
    /// have written, without depending on the `warden` crate itself.
    async fn seeded_db(state: RunState, converged_commit_sha: Option<&str>) -> (TempDir, String) {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("state.db");

        let write_options = SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true);
        let write_pool = SqlitePoolOptions::new()
            .connect_with(write_options)
            .await
            .unwrap();

        sqlx::query(
            "CREATE TABLE runs (
                id TEXT PRIMARY KEY,
                state TEXT NOT NULL,
                converged_commit_sha TEXT
            )",
        )
        .execute(&write_pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO runs (id, state, converged_commit_sha) VALUES ('run-under-test', ?, ?)",
        )
        .bind(state.as_str())
        .bind(converged_commit_sha)
        .execute(&write_pool)
        .await
        .unwrap();

        write_pool.close().await;
        (dir, db_path.display().to_string())
    }

    /// Acceptance criterion 1 (issue #3), exercised end-to-end against a
    /// real read-only SQLite connection: the run genuinely sitting in
    /// SQLite is `CoderRunning` (never advanced further -- simulating a
    /// crashed/stuck/still-in-progress run), while the *notification*
    /// passed in claims a commit as though the run had converged on it
    /// (standing in for whatever a compromised or buggy `warden` might
    /// assert). `verify_and_authorize` must ignore that claim and block,
    /// because it re-derives the state itself rather than trusting the
    /// caller.
    #[tokio::test]
    async fn push_is_blocked_when_the_real_db_disagrees_with_what_the_notification_claims() {
        let (_dir, db_path) = seeded_db(RunState::CoderRunning, None).await;
        let ro_pool = db::connect_read_only(std::path::Path::new(&db_path))
            .await
            .unwrap();

        let decision = verify_and_authorize(
            &ro_pool,
            "run-under-test",
            "commit-warden-claims-is-converged",
        )
        .await
        .unwrap();

        assert_eq!(
            decision,
            GateDecision::Blocked(GateBlockReason::NotConverged {
                actual_state: RunState::CoderRunning
            })
        );
    }

    #[tokio::test]
    async fn push_is_allowed_when_converged_and_hash_matches() {
        let (_dir, db_path) = seeded_db(RunState::Converged, Some("abc123")).await;
        let ro_pool = db::connect_read_only(std::path::Path::new(&db_path))
            .await
            .unwrap();

        let decision = verify_and_authorize(&ro_pool, "run-under-test", "abc123")
            .await
            .unwrap();

        assert_eq!(
            decision,
            GateDecision::Allow {
                commit_sha: "abc123".to_string()
            }
        );
    }

    #[tokio::test]
    async fn push_is_blocked_when_converged_but_hash_does_not_match() {
        let (_dir, db_path) = seeded_db(RunState::Converged, Some("real-converged-sha")).await;
        let ro_pool = db::connect_read_only(std::path::Path::new(&db_path))
            .await
            .unwrap();

        let decision = verify_and_authorize(&ro_pool, "run-under-test", "tampered-sha")
            .await
            .unwrap();

        assert_eq!(
            decision,
            GateDecision::Blocked(GateBlockReason::HashMismatch {
                validated: Some("real-converged-sha".to_string()),
                pushed: "tampered-sha".to_string(),
            })
        );
    }

    #[tokio::test]
    async fn push_is_blocked_for_a_run_id_absent_from_the_real_database() {
        let (_dir, db_path) = seeded_db(RunState::Converged, Some("abc123")).await;
        let ro_pool = db::connect_read_only(std::path::Path::new(&db_path))
            .await
            .unwrap();

        let decision = verify_and_authorize(&ro_pool, "no-such-run", "abc123")
            .await
            .unwrap();

        assert_eq!(
            decision,
            GateDecision::Blocked(GateBlockReason::RunNotFound {
                run_id: "no-such-run".to_string()
            })
        );
    }
}
