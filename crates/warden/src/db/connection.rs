use super::*;

/// How long a connection waits on SQLite's own lock before giving up with
/// `SQLITE_BUSY`. Matches sqlx's own default (5s) -- named and set
/// explicitly rather than left implicit, because Phase 2 makes concurrent
/// writers a real, expected case (reviewer and tester findings/worktree-path
/// updates land on the same `cycles`/`agent_processes` rows via
/// `tokio::join!`, see orchestrator.rs), not just a theoretical one.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The compiled-in migration set, named so both `connect` (to run it) and
/// `migrations_pending` (to compare against what's already applied) share
/// the exact same source of truth for "how many migrations exist".
pub(super) static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Opens (creating if needed) the SQLite database at `db_path`, enables WAL
/// mode so `warden-tui`/`warden-gated` can read concurrently (see
/// code-standards.md, "SQLite & sqlx"), backs up the database file if
/// pending migrations are about to run against a pre-existing db (issue #6:
/// crash resilience also covers a botched schema migration, not just a
/// crashed run), and applies those migrations.
pub async fn connect(db_path: &Path) -> Result<SqlitePool> {
    if let Some(parent) = db_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    // Captured *before* `connect_with` below, which creates the file if it's
    // missing (`create_if_missing(true)`) — otherwise a brand-new db would
    // always look "pre-existing" by the time we check.
    let db_existed_before_connect = tokio::fs::try_exists(db_path).await?;

    let options = SqliteConnectOptions::new()
        .filename(db_path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        // Explicit rather than relying on sqlx's default: the `cycles`,
        // `findings`, and `agent_processes` tables all declare `REFERENCES`
        // clauses (see migrations/0001_initial.sql) that are otherwise
        // decorative — SQLite does not enforce foreign keys unless this
        // pragma is on for the connection.
        .foreign_keys(true)
        // Explicit rather than relying on sqlx's default, for the same
        // reason as `foreign_keys` above: with reviewer and tester now
        // writing concurrently (ADR-0003), a `SQLITE_BUSY` under real WAL
        // write contention is a case worth naming and reasoning about, not
        // an implicit library default.
        .busy_timeout(BUSY_TIMEOUT);

    let pool = SqlitePoolOptions::new().connect_with(options).await?;

    if db_existed_before_connect {
        backup_before_migration(db_path, &pool).await?;
    }

    MIGRATOR.run(&pool).await?;

    Ok(pool)
}

/// `true` if applying [`MIGRATOR`] against `pool` would actually run at
/// least one migration. Deliberately conservative rather than bit-for-bit
/// reproducing `Migrator::run`'s own bookkeeping (dirty-version checks,
/// checksum validation, ...): this only needs to answer "is a backup worth
/// taking", not "is the migration state valid" — `MIGRATOR.run` still does
/// the real validation right after.
async fn migrations_pending(pool: &SqlitePool) -> Result<bool> {
    let migrations_table_exists: Option<(String,)> = sqlx::query_as(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_optional(pool)
    .await?;

    let Some(_) = migrations_table_exists else {
        // No migrations have ever been recorded against this db file, so
        // every migration `MIGRATOR` knows about is pending (unless there
        // simply aren't any, e.g. a from-scratch schema with no migrations
        // directory — not our case, but kept correct regardless).
        return Ok(MIGRATOR.iter().next().is_some());
    };

    let (applied_count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM _sqlx_migrations")
        .fetch_one(pool)
        .await?;
    let total_migrations = MIGRATOR.iter().count() as i64;

    Ok(applied_count < total_migrations)
}

/// Copies `db_path` to a timestamped sibling (`state.db.bak-<rfc3339>`)
/// before [`MIGRATOR`] is allowed to touch the schema, but only when a
/// migration is actually about to run (see [`migrations_pending`]) — a
/// fresh db or one already on the current schema has nothing worth backing
/// up.
///
/// Uses `VACUUM INTO` rather than a plain filesystem copy of `db_path`: WAL
/// mode (enabled in [`connect`]) means recently committed writes can live
/// only in the `-wal` sidecar file, not yet checkpointed into `db_path`
/// itself, so a bare `fs::copy` could silently produce a backup missing
/// committed data. `VACUUM INTO` reads the database's current *logical*
/// content (WAL included) and materializes it into a single new, consistent
/// file in one step — no separate checkpoint call needed.
///
/// A failure here aborts the migration (propagated to the caller as
/// [`WardenError::Backup`]) rather than proceeding without a safety net
/// (code-standards.md: "no silent fallback").
pub(super) async fn backup_before_migration(db_path: &Path, pool: &SqlitePool) -> Result<()> {
    if !migrations_pending(pool).await? {
        return Ok(());
    }

    // `:` is valid in Unix filenames but awkward to work with on the
    // command line, so it's stripped from the timestamp purely for
    // readability — RFC3339 ordering is preserved either way.
    let timestamp = now_rfc3339().replace(':', "-");
    let backup_path = unique_backup_path(db_path, &timestamp).await?;

    sqlx::query("VACUUM INTO ?")
        .bind(backup_path.display().to_string())
        .execute(pool)
        .await
        .map_err(|source| WardenError::Backup {
            path: backup_path.clone(),
            source,
        })?;

    tracing::info!(
        backup_path = %backup_path.display(),
        "backed up SQLite database before applying pending migrations"
    );
    Ok(())
}

/// Picks a backup path of the form `<file_name>.bak-<timestamp>`, appending
/// `-1`, `-2`, ... if that name is already taken. `now_rfc3339()`'s
/// resolution isn't guaranteed finer than a second on every platform, so two
/// backups requested within the same second (or a stale leftover file from a
/// previous run sharing the same timestamp) must not collide — `VACUUM INTO`
/// refuses to overwrite an existing file, which would otherwise abort the
/// migration on a spurious naming collision rather than a real backup
/// failure.
pub(super) async fn unique_backup_path(
    db_path: &Path,
    timestamp: &str,
) -> Result<std::path::PathBuf> {
    let file_name = db_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state.db");

    let mut candidate = db_path.with_file_name(format!("{file_name}.bak-{timestamp}"));
    let mut suffix: u32 = 1;
    while tokio::fs::try_exists(&candidate).await? {
        candidate = db_path.with_file_name(format!("{file_name}.bak-{timestamp}-{suffix}"));
        suffix += 1;
    }
    Ok(candidate)
}
