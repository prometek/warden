use super::*;

/// How long a connection waits on SQLite's own lock before giving up with `SQLITE_BUSY`.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

pub async fn connect(db_path: &Path) -> Result<SqlitePool> {
    if let Some(parent) = db_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let db_existed_before_connect = tokio::fs::try_exists(db_path).await?;

    let options = SqliteConnectOptions::new()
        .filename(db_path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true)
        .busy_timeout(BUSY_TIMEOUT);

    let pool = SqlitePoolOptions::new().connect_with(options).await?;

    if db_existed_before_connect {
        backup_before_migration(db_path, &pool).await?;
    }

    MIGRATOR.run(&pool).await?;

    Ok(pool)
}

/// `true` if applying [`MIGRATOR`] against `pool` would actually run at least one migration.
async fn migrations_pending(pool: &SqlitePool) -> Result<bool> {
    let migrations_table_exists: Option<(String,)> = sqlx::query_as(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_optional(pool)
    .await?;

    let Some(_) = migrations_table_exists else {
        return Ok(MIGRATOR.iter().next().is_some());
    };

    let (applied_count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM _sqlx_migrations")
        .fetch_one(pool)
        .await?;
    let total_migrations = MIGRATOR.iter().count() as i64;

    Ok(applied_count < total_migrations)
}

pub(super) async fn backup_before_migration(db_path: &Path, pool: &SqlitePool) -> Result<()> {
    if !migrations_pending(pool).await? {
        return Ok(());
    }

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

/// Picks a backup path of the form `<file_name>.bak-<timestamp>`, appending `-1`, `-2`,...
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
