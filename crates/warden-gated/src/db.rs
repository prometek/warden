//! Independent, read-only view of the SQLite database written by `warden`.
//!
//! This module is deliberately **duplicated** from (rather than importing)
//! `warden`'s own `db.rs` -- see Architecture.md §13 ("crate partagée
//! `warden-core` (types seulement, pas la logique de vérification —
//! dupliquée volontairement côté gate)") and ADR-0006. `warden-gated` must
//! never inherit a bug in `warden`'s own query/parsing logic when deciding
//! whether a push reaches `origin`; the two crates only share `warden-core`
//! (the `RunState` type itself), never each other's I/O code.

use std::path::Path;
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use warden_core::RunState;

use crate::error::{GatedError, Result};

/// Matches `warden::db`'s own busy timeout: the gate's read-only connection
/// can still contend with `warden`'s writer under WAL, so this is named and
/// explicit rather than left at whatever sqlx defaults to.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Opens `db_path` strictly **read-only** (code-standards.md, "SQLite &
/// sqlx": "`warden-gated` ... ouvrent la base en lecture seule"). Fails
/// loudly rather than creating the file -- an absent database means a
/// misconfigured path, never a case to silently paper over by creating an
/// empty one.
pub async fn connect_read_only(db_path: &Path) -> Result<SqlitePool> {
    if !db_path.exists() {
        return Err(GatedError::DatabaseNotFound(db_path.to_path_buf()));
    }

    let options = SqliteConnectOptions::new()
        .filename(db_path)
        .read_only(true)
        .busy_timeout(BUSY_TIMEOUT);

    let pool = SqlitePoolOptions::new().connect_with(options).await?;
    Ok(pool)
}

/// The subset of a `runs` row `warden-gated` needs to authorize a push: its
/// current state and the commit it converged on. Re-read from SQLite on
/// every single push attempt -- never cached, never trusted from a prior
/// read or from anything `warden` claims over the notification channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateRunView {
    pub state: RunState,
    pub converged_commit_sha: Option<String>,
}

/// Raw shape of the columns this crate reads, before `state` has been
/// validated into a [`RunState`]. Kept private: [`GateRunView`] is the only
/// form that ever leaves this module (code-standards.md: "toute ligne
/// relue est reparsée en type Rust fort").
struct RunRow {
    state: String,
    converged_commit_sha: Option<String>,
}

pub async fn get_run_view(pool: &SqlitePool, run_id: &str) -> Result<Option<GateRunView>> {
    let row = sqlx::query_as!(
        RunRow,
        r#"SELECT state, converged_commit_sha FROM runs WHERE id = ?"#,
        run_id,
    )
    .fetch_optional(pool)
    .await?;

    row.map(|r| -> Result<GateRunView> {
        Ok(GateRunView {
            state: RunState::parse(&r.state)?,
            converged_commit_sha: r.converged_commit_sha,
        })
    })
    .transpose()
}

/// The subset of a `runs` row the crash-recovery `resume-watch` path needs
/// (issue #15/ADR-0011): its current state (must still be `AwaitingCi` --
/// re-verified here rather than trusted from the caller, same "never trust
/// the caller" principle as [`GateRunView`]/[`get_run_view`]) and the PR
/// number `warden` persisted for it. A dedicated, narrower read rather than
/// growing [`GateRunView`] itself, whose existing fields/tests are specific
/// to the push-authorization path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwaitingCiRunView {
    pub state: RunState,
    pub pr_number: Option<u64>,
}

struct AwaitingCiRunRow {
    state: String,
    pr_number: Option<i64>,
}

pub async fn get_awaiting_ci_run_view(
    pool: &SqlitePool,
    run_id: &str,
) -> Result<Option<AwaitingCiRunView>> {
    let row = sqlx::query_as!(
        AwaitingCiRunRow,
        r#"SELECT state, pr_number FROM runs WHERE id = ?"#,
        run_id,
    )
    .fetch_optional(pool)
    .await?;

    row.map(|r| -> Result<AwaitingCiRunView> {
        let pr_number = r
            .pr_number
            .map(|value| {
                u64::try_from(value).map_err(|_| GatedError::InvalidStoredValue {
                    column: "runs.pr_number",
                    value,
                })
            })
            .transpose()?;
        Ok(AwaitingCiRunView {
            state: RunState::parse(&r.state)?,
            pr_number,
        })
    })
    .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connect_read_only_fails_loudly_when_the_database_does_not_exist() {
        let dir = tempfile::TempDir::new().unwrap();
        let missing_db = dir.path().join("does-not-exist.db");

        let result = connect_read_only(&missing_db).await;
        assert!(matches!(result, Err(GatedError::DatabaseNotFound(_))));
    }

    /// Sets up a real SQLite file with `warden`'s own schema (via a plain
    /// `CREATE TABLE`, since this crate must not depend on `warden`'s
    /// migrations either) and a writable connection to seed rows, mirroring
    /// code-standards.md's "DB de test: SQLite fichier temporaire réel".
    async fn seed_db_with_run(
        dir: &Path,
        run_id: &str,
        state: RunState,
        converged_commit_sha: Option<&str>,
    ) -> std::path::PathBuf {
        let db_path = dir.join("state.db");
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

        sqlx::query("INSERT INTO runs (id, state, converged_commit_sha) VALUES (?, ?, ?)")
            .bind(run_id)
            .bind(state.as_str())
            .bind(converged_commit_sha)
            .execute(&write_pool)
            .await
            .unwrap();

        write_pool.close().await;
        db_path
    }

    #[tokio::test]
    async fn get_run_view_round_trips_a_converged_run_with_its_commit_sha() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path =
            seed_db_with_run(dir.path(), "run-1", RunState::Converged, Some("deadbeef")).await;

        let pool = connect_read_only(&db_path).await.unwrap();
        let view = get_run_view(&pool, "run-1")
            .await
            .unwrap()
            .expect("run-1 exists");

        assert_eq!(view.state, RunState::Converged);
        assert_eq!(view.converged_commit_sha.as_deref(), Some("deadbeef"));
    }

    #[tokio::test]
    async fn get_run_view_returns_none_for_an_unknown_run() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path =
            seed_db_with_run(dir.path(), "run-1", RunState::Converged, Some("deadbeef")).await;

        let pool = connect_read_only(&db_path).await.unwrap();
        let view = get_run_view(&pool, "does-not-exist").await.unwrap();
        assert!(view.is_none());
    }

    /// A dedicated fixture (rather than growing [`seed_db_with_run`]) since
    /// [`get_awaiting_ci_run_view`] reads a column (`pr_number`) the
    /// push-authorization tests above have no reason to know about.
    async fn seed_db_with_awaiting_ci_run(
        dir: &Path,
        run_id: &str,
        state: RunState,
        pr_number: Option<i64>,
    ) -> std::path::PathBuf {
        let db_path = dir.join("state.db");
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
                pr_number INTEGER
            )",
        )
        .execute(&write_pool)
        .await
        .unwrap();

        sqlx::query("INSERT INTO runs (id, state, pr_number) VALUES (?, ?, ?)")
            .bind(run_id)
            .bind(state.as_str())
            .bind(pr_number)
            .execute(&write_pool)
            .await
            .unwrap();

        write_pool.close().await;
        db_path
    }

    #[tokio::test]
    async fn get_awaiting_ci_run_view_round_trips_state_and_pr_number() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path =
            seed_db_with_awaiting_ci_run(dir.path(), "run-1", RunState::AwaitingCi, Some(42)).await;

        let pool = connect_read_only(&db_path).await.unwrap();
        let view = get_awaiting_ci_run_view(&pool, "run-1")
            .await
            .unwrap()
            .expect("run-1 exists");

        assert_eq!(view.state, RunState::AwaitingCi);
        assert_eq!(view.pr_number, Some(42));
    }

    #[tokio::test]
    async fn get_awaiting_ci_run_view_reports_no_pr_number_when_unset() {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path =
            seed_db_with_awaiting_ci_run(dir.path(), "run-1", RunState::Pushed, None).await;

        let pool = connect_read_only(&db_path).await.unwrap();
        let view = get_awaiting_ci_run_view(&pool, "run-1")
            .await
            .unwrap()
            .expect("run-1 exists");

        assert_eq!(view.pr_number, None);
    }
}
