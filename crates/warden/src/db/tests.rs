use super::*;
use tempfile::TempDir;

async fn test_pool() -> (TempDir, SqlitePool) {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("state.db");
    let pool = connect(&db_path).await.unwrap();
    (dir, pool)
}

async fn suspend_test_quota_run(
    pool: &SqlitePool,
    run_id: &str,
    resets_at: i64,
    config_json: &str,
    state_json: &str,
) {
    insert_run(
        pool,
        run_id,
        "/tmp/repo",
        "main",
        "quota recovery",
        3,
        3,
        3,
        5,
    )
    .await
    .unwrap();
    update_run_state(pool, run_id, RunState::CoderRunning)
        .await
        .unwrap();
    suspend_run_with_quota_continuation(pool, run_id, resets_at, config_json, state_json)
        .await
        .unwrap();
}

#[test]
fn intermediate_state_literals_match_run_state_is_intermediate() {
    for state in [
        RunState::Pending,
        RunState::CoderRunning,
        RunState::RunningStep(1),
        RunState::RunningStep(2),
        RunState::RunningStep(7),
        RunState::Converged,
        RunState::Pushed,
        RunState::AwaitingCi,
        RunState::AwaitingQuotaReset {
            resets_at: 1_800_000_000,
        },
        RunState::ResumingQuota,
        RunState::Done,
        RunState::StepCyclesExceeded(1),
        RunState::StepCyclesExceeded(2),
        RunState::Failed,
    ] {
        let literal_says_intermediate = state.as_str() == "coder_running"
            || state.as_str() == "awaiting_ci"
            || state.as_str() == "resuming_quota"
            || state.as_str().starts_with("running_step:");
        assert_eq!(
                literal_says_intermediate,
                state.is_intermediate(),
                "state {state:?} disagrees between list_intermediate_runs' literals and RunState::is_intermediate",
            );
    }
}

#[tokio::test]
async fn run_round_trips_through_insert_and_get() {
    let (_dir, pool) = test_pool().await;
    insert_run(
        &pool,
        "run-1",
        "/tmp/repo",
        "main",
        "do the thing",
        5,
        4,
        3,
        5,
    )
    .await
    .unwrap();

    let run = get_run(&pool, "run-1").await.unwrap().unwrap();
    assert_eq!(run.state, RunState::Pending);
    assert_eq!(run.max_review_cycles, 5);
    assert_eq!(run.max_test_cycles, 4);
    assert_eq!(run.current_review_cycle, 0);
    assert_eq!(run.current_test_cycle, 0);
    assert_eq!(run.intent, "do the thing");
}

#[tokio::test]
async fn pr_number_is_none_until_set_then_round_trips() {
    let (_dir, pool) = test_pool().await;
    insert_run(&pool, "run-pr", "/tmp/repo", "main", "intent", 3, 3, 3, 5)
        .await
        .unwrap();

    let run = get_run(&pool, "run-pr").await.unwrap().unwrap();
    assert_eq!(run.pr_number, None);

    set_run_pr_number(&pool, "run-pr", 42).await.unwrap();

    let run = get_run(&pool, "run-pr").await.unwrap().unwrap();
    assert_eq!(run.pr_number, Some(42));
}

/// Issue #15 review, L2: an overflowing pr_number must be reported with
/// its own real value, not a misleading placeholder like `i64::MAX`.
#[tokio::test]
async fn set_run_pr_number_overflow_reports_the_real_value() {
    let (_dir, pool) = test_pool().await;
    insert_run(
        &pool,
        "run-pr-overflow",
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

    let overflowing = u64::try_from(i64::MAX).unwrap() + 1;
    let result = set_run_pr_number(&pool, "run-pr-overflow", overflowing).await;

    assert!(matches!(
        result,
        Err(WardenError::PrNumberOverflow { pr_number }) if pr_number == overflowing
    ));
}

#[tokio::test]
async fn update_run_state_persists_and_list_intermediate_finds_it() {
    let (_dir, pool) = test_pool().await;
    insert_run(&pool, "run-2", "/tmp/repo", "main", "intent", 3, 3, 3, 5)
        .await
        .unwrap();

    update_run_state(&pool, "run-2", RunState::CoderRunning)
        .await
        .unwrap();

    let run = get_run(&pool, "run-2").await.unwrap().unwrap();
    assert_eq!(run.state, RunState::CoderRunning);

    let intermediate = list_intermediate_runs(&pool).await.unwrap();
    assert_eq!(intermediate.len(), 1);
    assert_eq!(intermediate[0].id, "run-2");
}

/// Issue #85: quota suspension must durably retain its reset time, while
/// later ordinary state changes clear that quota-specific column.
#[tokio::test]
async fn quota_suspension_persists_and_clears_its_queryable_reset_time() {
    let (_dir, pool) = test_pool().await;
    insert_run(
        &pool,
        "run-quota-reset",
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

    update_run_state(
        &pool,
        "run-quota-reset",
        RunState::AwaitingQuotaReset {
            resets_at: 1_800_000_000,
        },
    )
    .await
    .unwrap();

    let stored: Option<i64> =
        sqlx::query_scalar("SELECT quota_resets_at FROM runs WHERE id = 'run-quota-reset'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored, Some(1_800_000_000));
    assert_eq!(
        get_run(&pool, "run-quota-reset")
            .await
            .unwrap()
            .unwrap()
            .state,
        RunState::AwaitingQuotaReset {
            resets_at: 1_800_000_000,
        }
    );

    update_run_state(&pool, "run-quota-reset", RunState::Failed)
        .await
        .unwrap();
    let cleared: Option<i64> =
        sqlx::query_scalar("SELECT quota_resets_at FROM runs WHERE id = 'run-quota-reset'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(cleared, None);
}

#[tokio::test]
async fn quota_resume_claim_is_due_at_the_exact_reset_boundary() {
    const RESET_BOUNDARY: i64 = 1_800_000_000;
    let (_dir, pool) = test_pool().await;
    suspend_test_quota_run(
        &pool,
        "exact-boundary-run",
        RESET_BOUNDARY,
        r#"{"config":"original"}"#,
        r#"{"state":"original"}"#,
    )
    .await;

    assert!(list_due_quota_continuations(&pool, RESET_BOUNDARY - 1)
        .await
        .unwrap()
        .is_empty());
    let candidates = list_due_quota_continuations(&pool, RESET_BOUNDARY)
        .await
        .unwrap();
    assert_eq!(candidates.len(), 1);
    assert!(
        claim_due_quota_continuation(&pool, &candidates[0], RESET_BOUNDARY)
            .await
            .unwrap()
    );
    assert_eq!(
        get_run(&pool, "exact-boundary-run")
            .await
            .unwrap()
            .unwrap()
            .state,
        RunState::ResumingQuota
    );
}

#[tokio::test]
async fn quota_resume_claim_has_exactly_one_concurrent_owner() {
    const RESET_BOUNDARY: i64 = 1_800_000_000;
    let (_dir, pool) = test_pool().await;
    suspend_test_quota_run(
        &pool,
        "atomic-claim-run",
        RESET_BOUNDARY,
        r#"{"config":"original"}"#,
        r#"{"state":"original"}"#,
    )
    .await;
    let candidate = list_due_quota_continuations(&pool, RESET_BOUNDARY)
        .await
        .unwrap()
        .pop()
        .unwrap();

    let (first, second) = tokio::join!(
        claim_due_quota_continuation(&pool, &candidate, RESET_BOUNDARY),
        claim_due_quota_continuation(&pool, &candidate, RESET_BOUNDARY),
    );

    assert_eq!(
        [first.unwrap(), second.unwrap()]
            .into_iter()
            .filter(|claimed| *claimed)
            .count(),
        1
    );
    assert_eq!(
        get_run(&pool, "atomic-claim-run")
            .await
            .unwrap()
            .unwrap()
            .state,
        RunState::ResumingQuota
    );
}

#[tokio::test]
async fn re_suspension_retains_the_new_checkpoint_and_rejects_the_stale_claim() {
    const FIRST_RESET: i64 = 1_800_000_000;
    const SECOND_RESET: i64 = 1_800_003_600;
    let (_dir, pool) = test_pool().await;
    suspend_test_quota_run(
        &pool,
        "re-suspended-run",
        FIRST_RESET,
        r#"{"config":"first"}"#,
        r#"{"state":"first"}"#,
    )
    .await;
    let stale_candidate = list_due_quota_continuations(&pool, FIRST_RESET)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert!(
        claim_due_quota_continuation(&pool, &stale_candidate, FIRST_RESET)
            .await
            .unwrap()
    );

    suspend_run_with_quota_continuation(
        &pool,
        "re-suspended-run",
        SECOND_RESET,
        r#"{"config":"second"}"#,
        r#"{"state":"second"}"#,
    )
    .await
    .unwrap();

    assert!(
        !claim_due_quota_continuation(&pool, &stale_candidate, SECOND_RESET)
            .await
            .unwrap()
    );
    assert!(list_due_quota_continuations(&pool, SECOND_RESET - 1)
        .await
        .unwrap()
        .is_empty());
    let current = get_quota_continuation(&pool, "re-suspended-run")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.config_json, r#"{"config":"second"}"#);
    assert_eq!(current.state_json, r#"{"state":"second"}"#);
    assert_eq!(
        get_run(&pool, "re-suspended-run")
            .await
            .unwrap()
            .unwrap()
            .state,
        RunState::AwaitingQuotaReset {
            resets_at: SECOND_RESET
        }
    );
}

/// A database created before migration 0012 has no quota column and no
/// quota-suspended state. Applying the migration must preserve that
/// legacy run and represent its absent reset time as NULL.
#[tokio::test]
async fn awaiting_quota_reset_migration_keeps_legacy_runs_coherent() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("state.db");
    let migrations: Vec<_> = MIGRATOR.iter().collect();
    let quota_migration_index = migrations
        .iter()
        .position(|migration| migration.description.contains("awaiting quota reset"))
        .expect("0012_awaiting_quota_reset.sql must remain in the migration set");
    let pre_quota_migration_version = migrations[quota_migration_index - 1].version;

    {
        let options = SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);
        let pool = SqlitePoolOptions::new()
            .connect_with(options)
            .await
            .unwrap();
        MIGRATOR
            .run_to(pre_quota_migration_version, &pool)
            .await
            .unwrap();
        insert_run(
            &pool,
            "legacy-run",
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
        pool.close().await;
    }

    let pool = connect(&db_path).await.unwrap();
    let legacy = get_run(&pool, "legacy-run").await.unwrap().unwrap();
    assert_eq!(legacy.state, RunState::Pending);
    let reset_time: Option<i64> =
        sqlx::query_scalar("SELECT quota_resets_at FROM runs WHERE id = 'legacy-run'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(reset_time, None);
}

#[tokio::test]
async fn converged_run_is_not_listed_as_intermediate() {
    let (_dir, pool) = test_pool().await;
    insert_run(&pool, "run-3", "/tmp/repo", "main", "intent", 3, 3, 3, 5)
        .await
        .unwrap();
    update_run_state(&pool, "run-3", RunState::CoderRunning)
        .await
        .unwrap();
    update_run_state(&pool, "run-3", RunState::RunningStep(1))
        .await
        .unwrap();
    update_run_state(&pool, "run-3", RunState::RunningStep(2))
        .await
        .unwrap();
    update_run_state(&pool, "run-3", RunState::Converged)
        .await
        .unwrap();

    let intermediate = list_intermediate_runs(&pool).await.unwrap();
    assert!(intermediate.is_empty());
}

#[tokio::test]
async fn cycle_and_finding_round_trip() {
    let (_dir, pool) = test_pool().await;
    insert_run(&pool, "run-4", "/tmp/repo", "main", "intent", 3, 3, 3, 5)
        .await
        .unwrap();
    insert_cycle(&pool, "cycle-1", "run-4", 1).await.unwrap();
    set_cycle_worktree_path(&pool, "cycle-1", "coder", "/tmp/wt/coder")
        .await
        .unwrap();

    let finding = Finding {
        source: FindingSource::role("reviewer"),
        severity: Severity::Blocking,
        file: Some("src/lib.rs".to_string()),
        description: "missing test".to_string(),
        action: Some("add one".to_string()),
    };
    insert_finding(&pool, "finding-1", "cycle-1", &finding)
        .await
        .unwrap();

    let findings = list_findings_for_cycle(&pool, "cycle-1").await.unwrap();
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0], finding);

    close_cycle(&pool, "cycle-1").await.unwrap();
}

// -----------------------------------------------------------------
// Token usage (issue #53)
// -----------------------------------------------------------------

#[tokio::test]
async fn cycle_role_token_usage_is_none_until_something_is_recorded() {
    let (_dir, pool) = test_pool().await;
    insert_run(
        &pool,
        "run-usage-none",
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
    insert_cycle(&pool, "cycle-usage-none", "run-usage-none", 1)
        .await
        .unwrap();

    let usage = get_cycle_role_token_usage(&pool, "cycle-usage-none", "coder")
        .await
        .unwrap();
    assert_eq!(
        usage, None,
        "no usage was ever recorded -- must be n/a, not zero"
    );
}

#[tokio::test]
async fn add_cycle_role_token_usage_accumulates_across_multiple_invocations() {
    let (_dir, pool) = test_pool().await;
    insert_run(
        &pool,
        "run-usage",
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
    insert_cycle(&pool, "cycle-usage", "run-usage", 1)
        .await
        .unwrap();

    add_cycle_role_token_usage(
        &pool,
        "cycle-usage",
        "coder",
        &TokenUsage::new(100, 50, Some(10), None),
    )
    .await
    .unwrap();
    add_cycle_role_token_usage(
        &pool,
        "cycle-usage",
        "coder",
        &TokenUsage::new(20, 10, None, Some(3)),
    )
    .await
    .unwrap();

    let usage = get_cycle_role_token_usage(&pool, "cycle-usage", "coder")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(usage.input_tokens, 120);
    assert_eq!(usage.output_tokens, 60);
    // The second invocation didn't report `cache_read_tokens` -- must not
    // reset the first invocation's own reported total.
    assert_eq!(usage.cache_read_tokens, Some(10));
    assert_eq!(usage.cache_creation_tokens, Some(3));
}

/// Each role's own running total on the same cycle must be tracked
/// independently -- recording the coder's usage must never leak into the
/// reviewer's columns on that same row.
#[tokio::test]
async fn add_cycle_role_token_usage_keeps_each_role_independent() {
    let (_dir, pool) = test_pool().await;
    insert_run(
        &pool,
        "run-usage-roles",
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
    insert_cycle(&pool, "cycle-usage-roles", "run-usage-roles", 1)
        .await
        .unwrap();

    add_cycle_role_token_usage(
        &pool,
        "cycle-usage-roles",
        "coder",
        &TokenUsage::new(100, 50, None, None),
    )
    .await
    .unwrap();
    add_cycle_role_token_usage(
        &pool,
        "cycle-usage-roles",
        "reviewer",
        &TokenUsage::new(7, 3, None, None),
    )
    .await
    .unwrap();

    let coder_usage = get_cycle_role_token_usage(&pool, "cycle-usage-roles", "coder")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(coder_usage.input_tokens, 100);

    let tester_usage = get_cycle_role_token_usage(&pool, "cycle-usage-roles", "tester")
        .await
        .unwrap();
    assert_eq!(
        tester_usage, None,
        "the tester never ran on this cycle -- must stay n/a"
    );
}

#[tokio::test]
async fn run_token_usage_is_none_until_something_is_recorded_then_accumulates() {
    let (_dir, pool) = test_pool().await;
    insert_run(
        &pool,
        "run-total-usage",
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

    assert_eq!(
        get_run_token_usage(&pool, "run-total-usage").await.unwrap(),
        None
    );

    add_run_token_usage(
        &pool,
        "run-total-usage",
        &TokenUsage::new(100, 50, Some(10), None),
    )
    .await
    .unwrap();
    add_run_token_usage(
        &pool,
        "run-total-usage",
        &TokenUsage::new(20, 10, Some(5), None),
    )
    .await
    .unwrap();

    let usage = get_run_token_usage(&pool, "run-total-usage")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(usage.input_tokens, 120);
    assert_eq!(usage.output_tokens, 60);
    assert_eq!(usage.cache_read_tokens, Some(15));
}

/// Issue #53: a `u64` token count too large for SQLite's native `i64`
/// column must surface as a typed `WardenError::TokenCountOverflow`
/// naming the real value that failed to convert -- never silently
/// truncated/clamped (same "no silent fallback" contract
/// `set_run_pr_number_overflow_reports_the_real_value` already pins for
/// `runs.pr_number`).
#[tokio::test]
async fn add_run_token_usage_overflow_reports_the_real_value() {
    let (_dir, pool) = test_pool().await;
    insert_run(
        &pool,
        "run-usage-overflow",
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

    let overflowing = u64::try_from(i64::MAX).unwrap() + 1;
    let result = add_run_token_usage(
        &pool,
        "run-usage-overflow",
        &TokenUsage::new(overflowing, 0, None, None),
    )
    .await;

    assert!(matches!(
        result,
        Err(WardenError::TokenCountOverflow { value, .. }) if value == overflowing
    ));
}

/// Same contract as
/// [`add_run_token_usage_overflow_reports_the_real_value`], for the
/// per-cycle-role columns [`add_cycle_role_token_usage`] writes.
#[tokio::test]
async fn add_cycle_role_token_usage_overflow_reports_the_real_value() {
    let (_dir, pool) = test_pool().await;
    insert_run(
        &pool,
        "run-cycle-usage-overflow",
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
    insert_cycle(&pool, "cycle-usage-overflow", "run-cycle-usage-overflow", 1)
        .await
        .unwrap();

    let overflowing = u64::try_from(i64::MAX).unwrap() + 1;
    let result = add_cycle_role_token_usage(
        &pool,
        "cycle-usage-overflow",
        "coder",
        &TokenUsage::new(overflowing, 0, None, None),
    )
    .await;

    assert!(matches!(
        result,
        Err(WardenError::TokenCountOverflow { value, .. }) if value == overflowing
    ));
}

// -----------------------------------------------------------------
// `set_run_rate_limit_status` / `get_run_rate_limit_status` (issue #84)
// -----------------------------------------------------------------

#[tokio::test]
async fn run_rate_limit_status_is_none_until_something_is_recorded_then_overwritten() {
    let (_dir, pool) = test_pool().await;
    insert_run(
        &pool,
        "run-rate-limit",
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

    assert_eq!(
        get_run_rate_limit_status(&pool, "run-rate-limit")
            .await
            .unwrap(),
        None
    );

    set_run_rate_limit_status(
        &pool,
        "run-rate-limit",
        &RateLimitStatus::new(
            RateLimitState::AllowedWarning,
            RateLimitWindow::SevenDay,
            0.93,
            false,
            0.75,
            1785686400,
        ),
    )
    .await
    .unwrap();

    let status = get_run_rate_limit_status(&pool, "run-rate-limit")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.status, RateLimitState::AllowedWarning);
    assert_eq!(status.rate_limit_type, RateLimitWindow::SevenDay);
    assert_eq!(status.utilization, 0.93);
    assert!(!status.is_using_overage);
    assert_eq!(status.surpassed_threshold, 0.75);
    assert_eq!(status.resets_at, 1785686400);

    // Issue #84: this is a snapshot, not a running total (unlike token
    // usage) -- a second report must overwrite the first, not merge with
    // it.
    set_run_rate_limit_status(
        &pool,
        "run-rate-limit",
        &RateLimitStatus::new(
            RateLimitState::AllowedWarning,
            RateLimitWindow::SevenDay,
            0.94,
            false,
            0.75,
            1785686400,
        ),
    )
    .await
    .unwrap();

    let status = get_run_rate_limit_status(&pool, "run-rate-limit")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.utilization, 0.94);
}

/// An unrecognized `status`/`rate_limit_type` value must round-trip
/// through the database unharmed -- both columns are plain `TEXT`, and
/// `RateLimitState`/`RateLimitWindow`'s own tolerance for unknown values
/// applies just as much on the read-back side as on the wire.
#[tokio::test]
async fn run_rate_limit_status_round_trips_an_unrecognized_status_and_type() {
    let (_dir, pool) = test_pool().await;
    insert_run(
        &pool,
        "run-rate-limit-unknown",
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

    set_run_rate_limit_status(
        &pool,
        "run-rate-limit-unknown",
        &RateLimitStatus::new(
            RateLimitState::Other("blocked".to_string()),
            RateLimitWindow::Other("five_hour".to_string()),
            1.0,
            true,
            0.97,
            1785686400,
        ),
    )
    .await
    .unwrap();

    let status = get_run_rate_limit_status(&pool, "run-rate-limit-unknown")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.status, RateLimitState::Other("blocked".to_string()));
    assert_eq!(
        status.rate_limit_type,
        RateLimitWindow::Other("five_hour".to_string())
    );
    assert!(status.is_using_overage);
}

/// A row with some, but not all, of the six `rate_limit_*` columns set
/// is corrupted -- `set_run_rate_limit_status` never writes a partial
/// row, so this can only happen from something other than this code
/// (code-standards.md: "no silent fallback"). Must surface as a typed
/// error, never silently reconstructed from whichever columns happen to
/// be present.
#[tokio::test]
async fn a_partially_populated_rate_limit_row_is_a_typed_error_not_a_silent_guess() {
    let (_dir, pool) = test_pool().await;
    insert_run(
        &pool,
        "run-rate-limit-corrupt",
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

    sqlx::query!(
        "UPDATE runs SET rate_limit_status = ? WHERE id = ?",
        "allowed_warning",
        "run-rate-limit-corrupt",
    )
    .execute(&pool)
    .await
    .unwrap();

    let result = get_run_rate_limit_status(&pool, "run-rate-limit-corrupt").await;
    assert!(matches!(
        result,
        Err(WardenError::CorruptRateLimitStatusRow { run_id }) if run_id == "run-rate-limit-corrupt"
    ));
}

#[tokio::test]
async fn get_run_returns_none_for_an_unknown_id() {
    let (_dir, pool) = test_pool().await;
    let run = get_run(&pool, "does-not-exist").await.unwrap();
    assert!(run.is_none());
}

#[tokio::test]
async fn inserting_a_run_with_a_duplicate_id_is_a_typed_error_not_a_panic() {
    let (_dir, pool) = test_pool().await;
    insert_run(&pool, "dup-run", "/tmp/repo", "main", "intent", 3, 3, 3, 5)
        .await
        .unwrap();

    let result = insert_run(
        &pool,
        "dup-run",
        "/tmp/repo",
        "main",
        "intent again",
        3,
        3,
        3,
        5,
    )
    .await;
    assert!(matches!(result, Err(WardenError::Database(_))));

    // The original row must be untouched by the failed duplicate insert.
    let run = get_run(&pool, "dup-run").await.unwrap().unwrap();
    assert_eq!(run.intent, "intent");
}

#[tokio::test]
async fn list_findings_for_cycle_with_no_findings_is_empty_not_an_error() {
    let (_dir, pool) = test_pool().await;
    insert_run(
        &pool,
        "run-empty",
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
    insert_cycle(&pool, "cycle-empty", "run-empty", 1)
        .await
        .unwrap();

    let findings = list_findings_for_cycle(&pool, "cycle-empty").await.unwrap();
    assert!(findings.is_empty());
}

/// Re-test cycle (issue #20 review fix, fdcaa4e): `ORDER BY id ASC`
/// must actually determine the returned order, not merely happen to
/// agree with insertion order. Deliberately inserts the
/// lexicographically-later id first, so a query without the `ORDER BY`
/// clause (which SQLite would otherwise satisfy via a plain rowid/
/// insertion-order table scan here, since neither `cycle_id` nor `id`
/// has a covering index driving this query) would return the rows in
/// the opposite order from what's asserted here.
#[tokio::test]
async fn list_findings_for_cycle_orders_findings_by_id_ascending_not_insertion_order() {
    let (_dir, pool) = test_pool().await;
    insert_run(
        &pool,
        "run-order",
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
    insert_cycle(&pool, "cycle-order", "run-order", 1)
        .await
        .unwrap();

    let finding_z = Finding {
        source: FindingSource::role("reviewer"),
        severity: Severity::Blocking,
        file: None,
        description: "inserted first, id sorts last".to_string(),
        action: None,
    };
    let finding_a = Finding {
        source: FindingSource::role("tester"),
        severity: Severity::Blocking,
        file: None,
        description: "inserted second, id sorts first".to_string(),
        action: None,
    };

    insert_finding(&pool, "zzz-finding", "cycle-order", &finding_z)
        .await
        .unwrap();
    insert_finding(&pool, "aaa-finding", "cycle-order", &finding_a)
        .await
        .unwrap();

    let findings = list_findings_for_cycle(&pool, "cycle-order").await.unwrap();
    assert_eq!(
        findings,
        vec![finding_a, finding_z],
        "findings must be ordered by id ascending (aaa- before zzz-), regardless of \
             insertion order"
    );
}

#[tokio::test]
async fn latest_open_agent_process_is_none_when_run_has_no_processes() {
    let (_dir, pool) = test_pool().await;
    insert_run(
        &pool,
        "run-no-proc",
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

    let open = latest_open_agent_process_for_run(&pool, "run-no-proc")
        .await
        .unwrap();
    assert!(open.is_none());
}

#[tokio::test]
async fn open_agent_process_is_found_until_marked_ended() {
    let (_dir, pool) = test_pool().await;
    insert_run(&pool, "run-5", "/tmp/repo", "main", "intent", 3, 3, 3, 5)
        .await
        .unwrap();
    insert_cycle(&pool, "cycle-5", "run-5", 1).await.unwrap();
    insert_agent_process(&pool, "proc-1", "cycle-5", "coder", 424242, "/tmp/wt/coder")
        .await
        .unwrap();

    let open = latest_open_agent_process_for_run(&pool, "run-5")
        .await
        .unwrap();
    assert!(open.is_some());
    assert_eq!(open.unwrap().pid, 424242);

    mark_agent_process_ended(&pool, "proc-1", 0).await.unwrap();

    let open = latest_open_agent_process_for_run(&pool, "run-5")
        .await
        .unwrap();
    assert!(open.is_none());
}

#[tokio::test]
async fn list_open_agent_processes_returns_every_open_row_not_just_the_latest() {
    let (_dir, pool) = test_pool().await;
    insert_run(&pool, "run-6", "/tmp/repo", "main", "intent", 3, 3, 3, 5)
        .await
        .unwrap();
    insert_cycle(&pool, "cycle-6", "run-6", 1).await.unwrap();

    // Reviewer and tester open concurrently (ADR-0003): both rows must
    // come back, not just whichever sorts last.
    insert_agent_process(
        &pool,
        "proc-reviewer",
        "cycle-6",
        "reviewer",
        111,
        "/tmp/wt/reviewer",
    )
    .await
    .unwrap();
    insert_agent_process(
        &pool,
        "proc-tester",
        "cycle-6",
        "tester",
        222,
        "/tmp/wt/tester",
    )
    .await
    .unwrap();
    // Already closed: must not be returned.
    insert_agent_process(
        &pool,
        "proc-coder",
        "cycle-6",
        "coder",
        333,
        "/tmp/wt/coder",
    )
    .await
    .unwrap();
    mark_agent_process_ended(&pool, "proc-coder", 0)
        .await
        .unwrap();

    let mut open = list_open_agent_processes_for_run(&pool, "run-6")
        .await
        .unwrap();
    open.sort_by_key(|p| p.pid);
    let pids: Vec<u32> = open.iter().map(|p| p.pid).collect();
    assert_eq!(pids, vec![111, 222]);
}

#[tokio::test]
async fn list_open_agent_processes_is_empty_for_a_run_with_no_processes() {
    let (_dir, pool) = test_pool().await;
    insert_run(&pool, "run-7", "/tmp/repo", "main", "intent", 3, 3, 3, 5)
        .await
        .unwrap();

    let open = list_open_agent_processes_for_run(&pool, "run-7")
        .await
        .unwrap();
    assert!(open.is_empty());
}

#[tokio::test]
async fn list_worktree_paths_collects_distinct_non_null_paths_across_cycles() {
    let (_dir, pool) = test_pool().await;
    insert_run(&pool, "run-8", "/tmp/repo", "main", "intent", 3, 3, 3, 5)
        .await
        .unwrap();
    insert_cycle(&pool, "cycle-8a", "run-8", 1).await.unwrap();
    insert_cycle(&pool, "cycle-8b", "run-8", 2).await.unwrap();

    set_cycle_worktree_path(&pool, "cycle-8a", "coder", "/tmp/wt/coder-1")
        .await
        .unwrap();
    set_cycle_worktree_path(&pool, "cycle-8a", "reviewer", "/tmp/wt/reviewer-1")
        .await
        .unwrap();
    set_cycle_worktree_path(&pool, "cycle-8b", "coder", "/tmp/wt/coder-2")
        .await
        .unwrap();
    // Tester path left unset for both cycles — must not appear as a
    // spurious empty/None entry.

    let mut paths = list_worktree_paths_for_run(&pool, "run-8").await.unwrap();
    paths.sort();
    assert_eq!(
        paths,
        vec![
            "/tmp/wt/coder-1".to_string(),
            "/tmp/wt/coder-2".to_string(),
            "/tmp/wt/reviewer-1".to_string(),
        ]
    );
}

#[tokio::test]
async fn list_worktree_paths_is_empty_for_a_run_with_no_cycles() {
    let (_dir, pool) = test_pool().await;
    insert_run(&pool, "run-9", "/tmp/repo", "main", "intent", 3, 3, 3, 5)
        .await
        .unwrap();

    let paths = list_worktree_paths_for_run(&pool, "run-9").await.unwrap();
    assert!(paths.is_empty());
}

#[tokio::test]
async fn connect_does_not_back_up_a_brand_new_database_file() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("state.db");

    connect(&db_path).await.unwrap();

    let backups: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().contains(".bak-"))
        .collect();
    assert!(
        backups.is_empty(),
        "a freshly created db must not be backed up: {backups:?}"
    );
}

#[tokio::test]
async fn connect_does_not_back_up_when_the_schema_is_already_current() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("state.db");

    // First connect creates the file and applies every migration.
    connect(&db_path).await.unwrap();
    // Second connect against the same file: schema is already current,
    // so no migration is about to run — nothing worth backing up.
    connect(&db_path).await.unwrap();

    let backups: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().contains(".bak-"))
        .collect();
    assert!(
        backups.is_empty(),
        "reconnecting to an up-to-date schema must not produce a backup: {backups:?}"
    );
}

#[tokio::test]
async fn connect_backs_up_a_pre_existing_database_before_applying_pending_migrations() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("state.db");

    // Simulate an older Warden installation: only the first migration
    // has ever been applied (`Migrator::run_to`, sqlx's own supported
    // way to stop partway through), so the rest are still pending on
    // the next `connect`.
    {
        let options = SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);
        let pool = SqlitePoolOptions::new()
            .connect_with(options)
            .await
            .unwrap();

        let first_migration_version = MIGRATOR.iter().next().unwrap().version;
        MIGRATOR
            .run_to(first_migration_version, &pool)
            .await
            .unwrap();
        pool.close().await;
    }

    connect(&db_path).await.unwrap();

    let backups: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().contains(".bak-"))
        .collect();
    assert_eq!(
        backups.len(),
        1,
        "a pre-existing db with pending migrations must be backed up exactly once: {backups:?}"
    );
}

/// Issue #43 (#37.4): `0007_phase_budgets.sql` must not just add the new
/// per-phase columns -- it also has to carry forward rows already
/// sitting on the pre-#43 schema (single `max_cycles`/`current_cycle`,
/// and `RunState` string values only the removed
/// `AwaitingReviewTest`/`MaxCyclesExceeded` variants ever wrote:
/// `awaiting_review_test`/`max_cycles_exceeded`). Every other test in
/// this module goes through `connect`/`test_pool`, which always starts
/// from an empty file and so applies migration 0007 against zero rows --
/// it would pass even if the `UPDATE runs SET state = ...` remap
/// statements were deleted entirely. This test instead seeds a row on
/// the *pre-0007* schema (mirroring
/// `connect_backs_up_a_pre_existing_database_before_applying_pending_migrations`'s
/// `Migrator::run_to` technique to stop short of 0007), then lets
/// `MIGRATOR.run` apply 0007 for real and checks the row lands exactly
/// where the migration's own comments say it should.
#[tokio::test]
async fn phase_budgets_migration_remaps_pre_existing_rows_and_legacy_state_strings() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("state.db");

    // Issue #53 review: found by description, not by position relative
    // to the end of the migration list -- `migrations.len() - 2` (this
    // test's original technique) silently pointed at the wrong migration
    // the moment 0008 was appended after 0007, since "second-to-last"
    // stopped meaning "the one right before phase_budgets". Robust to any
    // number of migrations appended after 0007, as long as it keeps this
    // description.
    let migrations: Vec<_> = MIGRATOR.iter().collect();
    let phase_budgets_index = migrations
        .iter()
        .position(|migration| migration.description.contains("phase budgets"))
        .expect("0007_phase_budgets.sql must still be a migration in this set");
    let pre_phase_budgets_version = migrations[phase_budgets_index - 1].version;

    {
        let options = SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);
        let pool = SqlitePoolOptions::new()
            .connect_with(options)
            .await
            .unwrap();

        MIGRATOR
            .run_to(pre_phase_budgets_version, &pool)
            .await
            .unwrap();

        // Two rows, each pinned on a distinct legacy `state` string the
        // migration must remap, both still on the single `max_cycles`/
        // `current_cycle` pair 0007 replaces.
        sqlx::query(
                "INSERT INTO runs (id, repo_path, branch, intent, state, max_cycles, current_cycle, created_at, updated_at) \
                 VALUES ('run-mid-cycle', '/tmp/repo', 'main', 'legacy mid-cycle run', 'awaiting_review_test', 5, 3, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            )
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
                "INSERT INTO runs (id, repo_path, branch, intent, state, max_cycles, current_cycle, created_at, updated_at) \
                 VALUES ('run-exhausted', '/tmp/repo', 'main', 'legacy exhausted run', 'max_cycles_exceeded', 4, 4, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
            )
            .execute(&pool)
            .await
            .unwrap();

        pool.close().await;
    }

    // Re-`connect` applies every remaining pending migration, including
    // 0007, against the seeded rows above.
    let pool = connect(&db_path).await.unwrap();

    let mid_cycle = get_run(&pool, "run-mid-cycle").await.unwrap().unwrap();
    assert_eq!(
        mid_cycle.state,
        RunState::RunningStep(1),
        "a legacy 'awaiting_review_test' row must remap onto 'reviewing' (0007), then onto \
             RunningStep(1) once 0009's own remap runs on top of it -- the specific phase can't \
             be recovered from the string alone, but every RunningStep index is equally \
             is_intermediate so crash recovery behaves the same regardless"
    );
    assert_eq!(
        mid_cycle.max_review_cycles, 5,
        "the old single max_cycles becomes both phases' starting budget"
    );
    assert_eq!(mid_cycle.max_test_cycles, 5);
    assert_eq!(
        mid_cycle.current_review_cycle, 3,
        "the old single current_cycle becomes the review phase's starting progress"
    );
    assert_eq!(
        mid_cycle.current_test_cycle, 0,
        "current_test_cycle has no legacy equivalent to carry forward, so it starts at 0"
    );

    let exhausted = get_run(&pool, "run-exhausted").await.unwrap().unwrap();
    assert_eq!(
        exhausted.state,
        RunState::StepCyclesExceeded(1),
        "a legacy 'max_cycles_exceeded' row must remap onto 'max_review_cycles_exceeded' \
             (0007), then onto StepCyclesExceeded(1) once 0009's own remap runs on top of it"
    );
    assert_eq!(exhausted.max_review_cycles, 4);
    assert_eq!(exhausted.max_test_cycles, 4);
    assert_eq!(exhausted.current_review_cycle, 4);
    assert_eq!(exhausted.current_test_cycle, 0);
}

/// Issue #73: `0009_generic_workflow_state.sql`'s own remap of the four
/// legacy `state` strings the closed `Reviewing`/`Testing`/
/// `MaxReviewCyclesExceeded`/`MaxTestCyclesExceeded` variants used to
/// write (`crates/warden/migrations/0009_generic_workflow_state.sql`'s
/// own comment: "a lossless, exact remap, not an approximation") --
/// `reviewing` -> `running_step:1`, `testing` -> `running_step:2`,
/// `max_review_cycles_exceeded` -> `step_cycles_exceeded:1`,
/// `max_test_cycles_exceeded` -> `step_cycles_exceeded:2`.
///
/// [`phase_budgets_migration_remaps_pre_existing_rows_and_legacy_state_strings`]
/// above already drives rows through 0007's own remap and on into 0009,
/// but every row it seeds starts from the *pre-0007* schema, so it only
/// ever lands on `reviewing`/`max_review_cycles_exceeded` (index 1) by
/// the time 0009 sees them -- `testing`/`max_test_cycles_exceeded`
/// (index 2) is never exercised there at all. This test seeds all four
/// legacy strings directly, on the schema exactly as 0007/0008 leave it
/// (`run_to` the migration immediately before 0009), so every one of
/// 0009's four `UPDATE` statements is independently exercised.
#[tokio::test]
async fn generic_workflow_state_migration_remaps_every_legacy_state_string() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("state.db");

    let migrations: Vec<_> = MIGRATOR.iter().collect();
    let generic_workflow_state_index = migrations
        .iter()
        .position(|migration| migration.description.contains("generic workflow state"))
        .expect("0009_generic_workflow_state.sql must still be a migration in this set");
    let pre_generic_workflow_state_version = migrations[generic_workflow_state_index - 1].version;

    {
        let options = SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);
        let pool = SqlitePoolOptions::new()
            .connect_with(options)
            .await
            .unwrap();

        MIGRATOR
            .run_to(pre_generic_workflow_state_version, &pool)
            .await
            .unwrap();

        // Four rows, one per legacy state string 0009 must remap, seeded
        // directly on the post-0007/0008 schema (`max_review_cycles`/
        // `max_test_cycles`/`current_review_cycle`/`current_test_cycle`,
        // no more single `max_cycles`/`current_cycle` pair).
        for (id, legacy_state) in [
            ("run-reviewing", "reviewing"),
            ("run-testing", "testing"),
            ("run-max-review-exceeded", "max_review_cycles_exceeded"),
            ("run-max-test-exceeded", "max_test_cycles_exceeded"),
        ] {
            sqlx::query(
                    "INSERT INTO runs (id, repo_path, branch, intent, state, max_review_cycles, max_test_cycles, current_review_cycle, current_test_cycle, created_at, updated_at) \
                     VALUES (?, '/tmp/repo', 'main', 'legacy run', ?, 5, 5, 1, 1, '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
                )
                .bind(id)
                .bind(legacy_state)
                .execute(&pool)
                .await
                .unwrap();
        }

        pool.close().await;
    }

    // Re-`connect` applies every remaining pending migration, including
    // 0009, against the seeded rows above.
    let pool = connect(&db_path).await.unwrap();

    let reviewing = get_run(&pool, "run-reviewing").await.unwrap().unwrap();
    assert_eq!(
        reviewing.state,
        RunState::RunningStep(1),
        "legacy 'reviewing' must remap onto RunningStep(1)"
    );

    let testing = get_run(&pool, "run-testing").await.unwrap().unwrap();
    assert_eq!(
        testing.state,
        RunState::RunningStep(2),
        "legacy 'testing' must remap onto RunningStep(2)"
    );

    let max_review_exceeded = get_run(&pool, "run-max-review-exceeded")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        max_review_exceeded.state,
        RunState::StepCyclesExceeded(1),
        "legacy 'max_review_cycles_exceeded' must remap onto StepCyclesExceeded(1)"
    );

    let max_test_exceeded = get_run(&pool, "run-max-test-exceeded")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        max_test_exceeded.state,
        RunState::StepCyclesExceeded(2),
        "legacy 'max_test_cycles_exceeded' must remap onto StepCyclesExceeded(2)"
    );
}

/// Issue #6: "a failed backup aborts migration (fails loud) rather than
/// proceeding". Forces `VACUUM INTO` to fail by revoking write
/// permission on the directory the backup file would be created in
/// *after* the pool (and its `-wal`/`-shm` sidecars) already exist —
/// so the failure genuinely comes from the backup step itself, not from
/// merely opening the database. `backup_before_migration` is private but
/// reachable here via `super::*`, letting this test target the exact
/// failure point without needing a full second `connect` (which would
/// hit the same permission error earlier, at WAL setup, and not prove
/// anything about the backup step specifically).
#[cfg(unix)]
#[tokio::test]
async fn backup_failure_is_a_typed_error_not_a_silent_fallback() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("state.db");

    // Simulate an older Warden installation with only the first
    // migration applied, so a migration is genuinely pending and a
    // backup is attempted (mirrors the "pending migrations" test above).
    let options = SqliteConnectOptions::new()
        .filename(&db_path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = SqlitePoolOptions::new()
        .connect_with(options)
        .await
        .unwrap();
    let first_migration_version = MIGRATOR.iter().next().unwrap().version;
    MIGRATOR
        .run_to(first_migration_version, &pool)
        .await
        .unwrap();

    // Revoke write permission on the directory only now, after the pool
    // and its WAL sidecars already exist -- `VACUUM INTO` must fail
    // trying to create the *new* backup file in a directory it can no
    // longer write to.
    let original_permissions = std::fs::metadata(dir.path()).unwrap().permissions();
    let mut readonly = original_permissions.clone();
    readonly.set_mode(0o555);
    std::fs::set_permissions(dir.path(), readonly).unwrap();

    let result = backup_before_migration(&db_path, &pool).await;

    // Restore permissions before the TempDir is dropped, regardless of
    // the assertion outcome, so cleanup doesn't itself fail.
    std::fs::set_permissions(dir.path(), original_permissions).unwrap();

    assert!(
        matches!(result, Err(WardenError::Backup { .. })),
        "expected a typed Backup error when VACUUM INTO cannot write its target, got: {result:?}"
    );

    pool.close().await;
}

#[tokio::test]
async fn unique_backup_path_appends_a_suffix_on_a_same_timestamp_collision() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("state.db");
    let timestamp = "2026-07-11T00-00-00+00-00";

    // Nothing on disk yet: the plain, unsuffixed name is used.
    let first = unique_backup_path(&db_path, timestamp).await.unwrap();
    assert_eq!(first, dir.path().join(format!("state.db.bak-{timestamp}")));

    // Simulates a leftover/duplicate backup sharing the same timestamp
    // (e.g. two restarts within the same second) -- `VACUUM INTO` would
    // otherwise abort on a spurious naming collision rather than a real
    // backup failure.
    std::fs::write(&first, b"pre-existing backup").unwrap();
    let second = unique_backup_path(&db_path, timestamp).await.unwrap();
    assert_eq!(
        second,
        dir.path().join(format!("state.db.bak-{timestamp}-1"))
    );

    std::fs::write(&second, b"pre-existing backup").unwrap();
    let third = unique_backup_path(&db_path, timestamp).await.unwrap();
    assert_eq!(
        third,
        dir.path().join(format!("state.db.bak-{timestamp}-2"))
    );
}

#[tokio::test]
async fn clear_cycle_worktree_path_nulls_out_only_the_given_role() {
    let (_dir, pool) = test_pool().await;
    insert_run(
        &pool,
        "run-clear",
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
    insert_cycle(&pool, "cycle-clear", "run-clear", 1)
        .await
        .unwrap();
    set_cycle_worktree_path(&pool, "cycle-clear", "coder", "/tmp/wt/coder")
        .await
        .unwrap();
    set_cycle_worktree_path(&pool, "cycle-clear", "reviewer", "/tmp/wt/reviewer")
        .await
        .unwrap();

    clear_cycle_worktree_path(&pool, "cycle-clear", "coder")
        .await
        .unwrap();

    let entries = list_cycle_worktree_entries_for_run(&pool, "run-clear")
        .await
        .unwrap();
    assert_eq!(entries.len(), 1, "only the reviewer path should remain");
    assert_eq!(entries[0].role, "reviewer");
    assert_eq!(entries[0].path, "/tmp/wt/reviewer");
}

#[tokio::test]
async fn failed_run_with_no_open_process_and_no_recorded_worktree_needs_no_cleanup() {
    let (_dir, pool) = test_pool().await;
    insert_run(
        &pool,
        "run-clean-failed",
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
    update_run_state(&pool, "run-clean-failed", RunState::CoderRunning)
        .await
        .unwrap();
    update_run_state(&pool, "run-clean-failed", RunState::Failed)
        .await
        .unwrap();

    let pending = list_failed_runs_with_pending_cleanup(&pool).await.unwrap();
    assert!(
        pending.is_empty(),
        "a Failed run with nothing recorded to clean up must not be returned"
    );
}

#[tokio::test]
async fn failed_run_with_an_open_agent_process_needs_cleanup() {
    let (_dir, pool) = test_pool().await;
    insert_run(
        &pool,
        "run-open-proc",
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
    insert_cycle(&pool, "cycle-open-proc", "run-open-proc", 1)
        .await
        .unwrap();
    insert_agent_process(
        &pool,
        "proc-open",
        "cycle-open-proc",
        "coder",
        999_999_998,
        "/tmp/wt",
    )
    .await
    .unwrap();
    update_run_state(&pool, "run-open-proc", RunState::Failed)
        .await
        .unwrap();

    let pending = list_failed_runs_with_pending_cleanup(&pool).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, "run-open-proc");
}

#[tokio::test]
async fn failed_run_with_a_recorded_worktree_path_needs_cleanup() {
    let (_dir, pool) = test_pool().await;
    insert_run(
        &pool,
        "run-recorded-wt",
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
    insert_cycle(&pool, "cycle-recorded-wt", "run-recorded-wt", 1)
        .await
        .unwrap();
    set_cycle_worktree_path(&pool, "cycle-recorded-wt", "coder", "/tmp/wt/coder")
        .await
        .unwrap();
    update_run_state(&pool, "run-recorded-wt", RunState::Failed)
        .await
        .unwrap();

    let pending = list_failed_runs_with_pending_cleanup(&pool).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, "run-recorded-wt");

    // Once the path is cleared (simulating a successful removal), the
    // run must stop being returned -- no separate "cleanup done" flag,
    // the recorded path itself is the signal.
    clear_cycle_worktree_path(&pool, "cycle-recorded-wt", "coder")
        .await
        .unwrap();
    let pending = list_failed_runs_with_pending_cleanup(&pool).await.unwrap();
    assert!(pending.is_empty());
}

// -----------------------------------------------------------------
// EVIDENCE entity (ADR-0009, issue #7): insert + query back
// (migration 0004_evidence).
// -----------------------------------------------------------------

#[tokio::test]
async fn evidence_round_trips_through_insert_and_list_evidence_for_run() {
    let (_dir, pool) = test_pool().await;
    insert_run(
        &pool,
        "run-evidence",
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
    insert_cycle(&pool, "cycle-evidence", "run-evidence", 1)
        .await
        .unwrap();

    insert_evidence(
        &pool,
        "evidence-1",
        "cycle-evidence",
        None,
        EvidenceType::Image,
        ".warden/evidence/1/screenshot.png",
        "Playwright capture from the cycle's e2e test run",
    )
    .await
    .unwrap();

    let evidence = list_evidence_for_run(&pool, "run-evidence").await.unwrap();
    assert_eq!(evidence.len(), 1);
    assert_eq!(evidence[0].cycle_number, 1);
    assert_eq!(evidence[0].evidence.id, "evidence-1");
    assert_eq!(evidence[0].evidence.cycle_id, "cycle-evidence");
    assert_eq!(evidence[0].evidence.finding_id, None);
    assert_eq!(evidence[0].evidence.evidence_type, EvidenceType::Image);
    assert_eq!(
        evidence[0].evidence.file_path,
        ".warden/evidence/1/screenshot.png"
    );
    assert_eq!(
        evidence[0].evidence.description,
        "Playwright capture from the cycle's e2e test run"
    );
}

#[tokio::test]
async fn evidence_can_be_linked_to_a_specific_finding() {
    let (_dir, pool) = test_pool().await;
    insert_run(
        &pool,
        "run-evidence-finding",
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
    insert_cycle(&pool, "cycle-evidence-finding", "run-evidence-finding", 1)
        .await
        .unwrap();
    let finding = Finding {
        source: FindingSource::role("tester"),
        severity: Severity::Blocking,
        file: Some("src/lib.rs".to_string()),
        description: "flaky button".to_string(),
        action: Some("fix it".to_string()),
    };
    insert_finding(
        &pool,
        "finding-evidence",
        "cycle-evidence-finding",
        &finding,
    )
    .await
    .unwrap();

    insert_evidence(
        &pool,
        "evidence-linked",
        "cycle-evidence-finding",
        Some("finding-evidence"),
        EvidenceType::Video,
        ".warden/evidence/1/failure.webm",
        "video of the observed failure",
    )
    .await
    .unwrap();

    let evidence = list_evidence_for_run(&pool, "run-evidence-finding")
        .await
        .unwrap();
    assert_eq!(evidence.len(), 1);
    assert_eq!(
        evidence[0].evidence.finding_id.as_deref(),
        Some("finding-evidence")
    );
}

#[tokio::test]
async fn list_evidence_for_run_is_empty_when_no_evidence_was_captured() {
    let (_dir, pool) = test_pool().await;
    insert_run(
        &pool,
        "run-no-evidence",
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
    insert_cycle(&pool, "cycle-no-evidence", "run-no-evidence", 1)
        .await
        .unwrap();

    let evidence = list_evidence_for_run(&pool, "run-no-evidence")
        .await
        .unwrap();
    assert!(evidence.is_empty());
}

#[tokio::test]
async fn list_evidence_for_run_orders_by_cycle_number_then_capture_time() {
    let (_dir, pool) = test_pool().await;
    insert_run(
        &pool,
        "run-evidence-order",
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
    insert_cycle(&pool, "cycle-2", "run-evidence-order", 2)
        .await
        .unwrap();
    insert_cycle(&pool, "cycle-1", "run-evidence-order", 1)
        .await
        .unwrap();

    // Deliberately inserted out of cycle order: cycle 2's evidence
    // lands in the table first, but must still be listed *after*
    // cycle 1's when read back.
    insert_evidence(
        &pool,
        "evidence-cycle-2",
        "cycle-2",
        None,
        EvidenceType::Other,
        ".warden/evidence/2/session.cast",
        "cycle 2 recording",
    )
    .await
    .unwrap();
    insert_evidence(
        &pool,
        "evidence-cycle-1",
        "cycle-1",
        None,
        EvidenceType::Other,
        ".warden/evidence/1/session.cast",
        "cycle 1 recording",
    )
    .await
    .unwrap();

    let evidence = list_evidence_for_run(&pool, "run-evidence-order")
        .await
        .unwrap();
    assert_eq!(evidence.len(), 2);
    assert_eq!(evidence[0].cycle_number, 1);
    assert_eq!(evidence[1].cycle_number, 2);
}

#[tokio::test]
async fn intermediate_runs_are_not_returned_by_the_failed_cleanup_query() {
    let (_dir, pool) = test_pool().await;
    insert_run(
        &pool,
        "run-still-running",
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
    update_run_state(&pool, "run-still-running", RunState::CoderRunning)
        .await
        .unwrap();

    let pending = list_failed_runs_with_pending_cleanup(&pool).await.unwrap();
    assert!(
        pending.is_empty(),
        "a run that isn't Failed yet belongs to list_intermediate_runs, not this query"
    );
}

// ---- events (Phase 8, issue #8) ----------------------------------------

#[tokio::test]
async fn event_round_trips_through_insert_and_list() {
    let (_dir, pool) = test_pool().await;
    insert_run(
        &pool,
        "run-events",
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

    let event = RunEvent::CycleStarted { cycle_number: 1 };
    insert_event(
        &pool,
        "event-1",
        "run-events",
        &event,
        "2026-07-12T00:00:00+00:00",
    )
    .await
    .unwrap();

    let events = list_events_for_run(&pool, "run-events").await.unwrap();
    assert_eq!(events.len(), 1);
    let record = events[0].decoded().expect("well-formed row must decode");
    assert_eq!(record.id, "event-1");
    assert_eq!(record.run_id, "run-events");
    assert_eq!(record.event, event);
    assert_eq!(record.created_at, "2026-07-12T00:00:00+00:00");
}

#[tokio::test]
async fn list_events_for_run_orders_oldest_first() {
    let (_dir, pool) = test_pool().await;
    insert_run(
        &pool,
        "run-order",
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

    insert_event(
        &pool,
        "event-b",
        "run-order",
        &RunEvent::CycleStarted { cycle_number: 2 },
        "2026-07-12T00:00:02+00:00",
    )
    .await
    .unwrap();
    insert_event(
        &pool,
        "event-a",
        "run-order",
        &RunEvent::CycleStarted { cycle_number: 1 },
        "2026-07-12T00:00:01+00:00",
    )
    .await
    .unwrap();

    let events = list_events_for_run(&pool, "run-order").await.unwrap();
    let ids: Vec<&str> = events.iter().map(|e| e.id()).collect();
    assert_eq!(ids, vec!["event-a", "event-b"]);
}

#[tokio::test]
async fn list_events_for_run_is_empty_for_a_run_with_no_events() {
    let (_dir, pool) = test_pool().await;
    insert_run(
        &pool,
        "run-no-events",
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

    let events = list_events_for_run(&pool, "run-no-events").await.unwrap();
    assert!(events.is_empty());
}

/// code-standards.md: "toute ligne relue est reparsée en type Rust
/// fort" -- a row whose `event_type` column disagrees with what its own
/// `payload_json` decodes to (corruption, or a write from something
/// other than `insert_event`) must never be silently trusted as
/// whichever of the two the reader happens to pick. Issue #58: this must
/// no longer fail the whole query -- it's surfaced as a typed
/// `Undecodable` entry instead.
#[tokio::test]
async fn mismatched_event_type_and_payload_kind_is_an_undecodable_entry_not_a_failed_query() {
    let (_dir, pool) = test_pool().await;
    insert_run(
        &pool,
        "run-corrupt",
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

    let payload_json = serde_json::to_string(&RunEvent::CycleStarted { cycle_number: 1 }).unwrap();
    sqlx::query!(
            "INSERT INTO events (id, run_id, event_type, payload_json, created_at) VALUES (?, ?, ?, ?, ?)",
            "event-corrupt",
            "run-corrupt",
            "run_finished",
            payload_json,
            "2026-07-12T00:00:00+00:00",
        )
        .execute(&pool)
        .await
        .unwrap();

    let events = list_events_for_run(&pool, "run-corrupt")
        .await
        .expect("one bad row must never fail the whole query");
    assert_eq!(events.len(), 1);
    match &events[0] {
        RunEventHistoryEntry::Undecodable(event) => {
            assert_eq!(event.id, "event-corrupt");
            assert_eq!(event.event_type, "run_finished");
            assert_eq!(
                event.reason,
                UndecodableReason::KindMismatch {
                    payload_kind: "cycle_started".to_string()
                }
            );
        }
        RunEventHistoryEntry::Decoded(record) => {
            panic!("expected an Undecodable entry, got a decoded record: {record:?}")
        }
    }
}

/// Issue #58 acceptance: a run whose history includes one row with
/// malformed `payload_json` *and* one row with a kind-mismatched
/// `event_type` must still return the full history -- the good events
/// intact, both bad rows surfaced as typed `Undecodable` markers, never
/// dropped and never a reason for the whole query to fail.
#[tokio::test]
async fn history_with_a_malformed_payload_and_a_kind_mismatch_still_returns_every_good_event() {
    let (_dir, pool) = test_pool().await;
    insert_run(
        &pool,
        "run-mixed",
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

    insert_event(
        &pool,
        "event-good-1",
        "run-mixed",
        &RunEvent::RunStarted {
            intent: "intent".to_string(),
            branch: "main".to_string(),
            max_review_cycles: 3,
            max_test_cycles: 3,
        },
        "2026-07-12T00:00:00+00:00",
    )
    .await
    .unwrap();

    // Malformed `payload_json` -- not even valid JSON for any `RunEvent`
    // variant (simulates a reshape that changed the payload shape
    // without a rewrite migration, issue #58's own motivating scenario).
    sqlx::query!(
            "INSERT INTO events (id, run_id, event_type, payload_json, created_at) VALUES (?, ?, ?, ?, ?)",
            "event-malformed",
            "run-mixed",
            "cycle_started",
            "{ not json",
            "2026-07-12T00:00:01+00:00",
        )
        .execute(&pool)
        .await
        .unwrap();

    // Kind-mismatched row: valid JSON, but for the wrong `event_type`.
    let mismatched_payload =
        serde_json::to_string(&RunEvent::CycleStarted { cycle_number: 1 }).unwrap();
    sqlx::query!(
            "INSERT INTO events (id, run_id, event_type, payload_json, created_at) VALUES (?, ?, ?, ?, ?)",
            "event-mismatched",
            "run-mixed",
            "run_finished",
            mismatched_payload,
            "2026-07-12T00:00:02+00:00",
        )
        .execute(&pool)
        .await
        .unwrap();

    insert_event(
        &pool,
        "event-good-2",
        "run-mixed",
        &RunEvent::CycleStarted { cycle_number: 2 },
        "2026-07-12T00:00:03+00:00",
    )
    .await
    .unwrap();

    let events = list_events_for_run(&pool, "run-mixed")
        .await
        .expect("undecodable rows must never fail the whole query");

    assert_eq!(events.len(), 4, "{events:?}");
    let ids: Vec<&str> = events.iter().map(|e| e.id()).collect();
    assert_eq!(
        ids,
        vec![
            "event-good-1",
            "event-malformed",
            "event-mismatched",
            "event-good-2",
        ],
        "order (created_at ASC, id ASC) must be preserved even with bad rows interleaved"
    );

    assert!(matches!(
        events[0],
        RunEventHistoryEntry::Decoded(ref record) if record.event == RunEvent::RunStarted {
            intent: "intent".to_string(),
            branch: "main".to_string(),
            max_review_cycles: 3,
            max_test_cycles: 3,
        }
    ));
    assert!(matches!(
        events[1],
        RunEventHistoryEntry::Undecodable(ref event) if event.event_type == "cycle_started"
    ));
    assert!(matches!(
        events[2],
        RunEventHistoryEntry::Undecodable(ref event) if event.event_type == "run_finished"
    ));
    assert!(matches!(
        events[3],
        RunEventHistoryEntry::Decoded(ref record)
            if record.event == RunEvent::CycleStarted { cycle_number: 2 }
    ));
}

/// Issue #58 test gap: an `event_type` column this binary's own
/// [`EventKind::parse`] doesn't recognize at all (e.g. an older
/// `warden-tui`/`warden` reading a database a newer writer already
/// advanced with an event kind this binary predates) must decode as
/// [`UndecodableReason::UnknownEventType`] specifically -- distinct from
/// [`UndecodableReason::PayloadDeserialize`]/`KindMismatch`, which both
/// require `event_type` to parse successfully first. This branch of
/// `row_to_history_entry` had no covering test before this change.
#[tokio::test]
async fn unrecognized_event_type_column_is_an_unknown_event_type_undecodable_entry() {
    let (_dir, pool) = test_pool().await;
    insert_run(
        &pool,
        "run-unknown-kind",
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

    sqlx::query!(
            "INSERT INTO events (id, run_id, event_type, payload_json, created_at) VALUES (?, ?, ?, ?, ?)",
            "event-future-kind",
            "run-unknown-kind",
            "workflow_step_added",
            r#"{"kind":"workflow_step_added","step":"techlead"}"#,
            "2026-07-12T00:00:00+00:00",
        )
        .execute(&pool)
        .await
        .unwrap();

    let events = list_events_for_run(&pool, "run-unknown-kind")
        .await
        .expect("an unrecognized event_type must never fail the whole query");

    assert_eq!(events.len(), 1);
    match &events[0] {
        RunEventHistoryEntry::Undecodable(event) => {
            assert_eq!(event.id, "event-future-kind");
            assert_eq!(event.event_type, "workflow_step_added");
            assert_eq!(event.reason, UndecodableReason::UnknownEventType);
        }
        RunEventHistoryEntry::Decoded(record) => {
            panic!("expected an Undecodable entry, got a decoded record: {record:?}")
        }
    }
}

/// Issue #58's own motivating real-world scenario, issue #26's reshape:
/// a `UntrustedAgentDefinitionUsed` row persisted *before* that issue
/// added `canonical_path` has no such key in its `payload_json` at all
/// (unlike issue #53's `AgentFinished.usage`, `canonical_path` carries
/// no `#[serde(default)]` -- see `warden_core::event`'s own docs) -- this
/// must decode as `Undecodable`, not panic or silently invent a value.
#[tokio::test]
async fn pre_issue_26_untrusted_agent_definition_used_payload_missing_canonical_path_is_undecodable(
) {
    let (_dir, pool) = test_pool().await;
    insert_run(
        &pool,
        "run-pre-26",
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

    let pre_issue_26_payload = r#"{"kind":"untrusted_agent_definition_used","role":"reviewer","path":"/repo/.warden/agents/reviewer.md"}"#;
    sqlx::query!(
            "INSERT INTO events (id, run_id, event_type, payload_json, created_at) VALUES (?, ?, ?, ?, ?)",
            "event-pre-26",
            "run-pre-26",
            "untrusted_agent_definition_used",
            pre_issue_26_payload,
            "2026-07-12T00:00:00+00:00",
        )
        .execute(&pool)
        .await
        .unwrap();

    let events = list_events_for_run(&pool, "run-pre-26")
        .await
        .expect("a stale pre-issue-26 payload must never fail the whole query");

    assert_eq!(events.len(), 1);
    match &events[0] {
        RunEventHistoryEntry::Undecodable(event) => {
            assert_eq!(event.id, "event-pre-26");
            assert_eq!(event.event_type, "untrusted_agent_definition_used");
            assert_eq!(event.reason, UndecodableReason::PayloadDeserialize);
        }
        RunEventHistoryEntry::Decoded(record) => {
            panic!("expected an Undecodable entry, got a decoded record: {record:?}")
        }
    }
}

/// Issue #58's own motivating real-world scenario, issue #43's reshape:
/// a `RunStarted` row persisted before that issue split the single
/// `max_cycles` field into `max_review_cycles`/`max_test_cycles` has
/// neither of the two new keys -- this must decode as `Undecodable`, not
/// be coerced into a fabricated budget.
#[tokio::test]
async fn pre_issue_43_run_started_payload_with_a_single_max_cycles_field_is_undecodable() {
    let (_dir, pool) = test_pool().await;
    insert_run(
        &pool,
        "run-pre-43",
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

    let pre_issue_43_payload =
        r#"{"kind":"run_started","intent":"do the thing","branch":"main","max_cycles":5}"#;
    sqlx::query!(
            "INSERT INTO events (id, run_id, event_type, payload_json, created_at) VALUES (?, ?, ?, ?, ?)",
            "event-pre-43",
            "run-pre-43",
            "run_started",
            pre_issue_43_payload,
            "2026-07-12T00:00:00+00:00",
        )
        .execute(&pool)
        .await
        .unwrap();

    let events = list_events_for_run(&pool, "run-pre-43")
        .await
        .expect("a stale pre-issue-43 payload must never fail the whole query");

    assert_eq!(events.len(), 1);
    match &events[0] {
        RunEventHistoryEntry::Undecodable(event) => {
            assert_eq!(event.id, "event-pre-43");
            assert_eq!(event.event_type, "run_started");
            assert_eq!(event.reason, UndecodableReason::PayloadDeserialize);
        }
        RunEventHistoryEntry::Decoded(record) => {
            panic!("expected an Undecodable entry, got a decoded record: {record:?}")
        }
    }
}

/// Issue #58 test gap: a run whose *every* row is undecodable must still
/// return the full set (none dropped) rather than failing or returning
/// an empty history that looks indistinguishable from "no events at
/// all" (`list_events_for_run_is_empty_for_a_run_with_no_events`).
#[tokio::test]
async fn history_where_every_row_is_undecodable_returns_all_of_them() {
    let (_dir, pool) = test_pool().await;
    insert_run(
        &pool,
        "run-all-bad",
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

    sqlx::query!(
            "INSERT INTO events (id, run_id, event_type, payload_json, created_at) VALUES (?, ?, ?, ?, ?)",
            "event-1-unknown-kind",
            "run-all-bad",
            "workflow_step_added",
            "{}",
            "2026-07-12T00:00:00+00:00",
        )
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query!(
            "INSERT INTO events (id, run_id, event_type, payload_json, created_at) VALUES (?, ?, ?, ?, ?)",
            "event-2-malformed",
            "run-all-bad",
            "cycle_started",
            "{ not json",
            "2026-07-12T00:00:01+00:00",
        )
        .execute(&pool)
        .await
        .unwrap();
    let mismatched_payload =
        serde_json::to_string(&RunEvent::CycleStarted { cycle_number: 1 }).unwrap();
    sqlx::query!(
            "INSERT INTO events (id, run_id, event_type, payload_json, created_at) VALUES (?, ?, ?, ?, ?)",
            "event-3-mismatched",
            "run-all-bad",
            "run_finished",
            mismatched_payload,
            "2026-07-12T00:00:02+00:00",
        )
        .execute(&pool)
        .await
        .unwrap();

    let events = list_events_for_run(&pool, "run-all-bad")
        .await
        .expect("an all-undecodable history must never fail the whole query");

    assert_eq!(events.len(), 3, "{events:?}");
    assert!(
        events
            .iter()
            .all(|entry| matches!(entry, RunEventHistoryEntry::Undecodable(_))),
        "{events:?}"
    );
    let ids: Vec<&str> = events.iter().map(|e| e.id()).collect();
    assert_eq!(
        ids,
        vec![
            "event-1-unknown-kind",
            "event-2-malformed",
            "event-3-mismatched",
        ],
        "order must still be created_at ASC even with no good rows at all"
    );
}

/// Issue #58 test gap: two rows sharing the exact same `created_at`
/// (second-resolution timestamps collide easily) -- one decodable, one
/// not -- must still come back in a stable, deterministic order (`id`
/// ASC as the tiebreaker, per `list_events_for_run`'s own `ORDER BY`),
/// regardless of which of the two is undecodable.
#[tokio::test]
async fn undecodable_and_decoded_rows_sharing_the_same_created_at_are_ordered_by_id() {
    let (_dir, pool) = test_pool().await;
    insert_run(&pool, "run-tie", "/tmp/repo", "main", "intent", 3, 3, 3, 5)
        .await
        .unwrap();

    let same_timestamp = "2026-07-12T00:00:00+00:00";
    // "event-a" (undecodable) sorts before "event-b" (decoded) by id --
    // inserted in the opposite order here to prove the ordering comes
    // from the SQL `ORDER BY`, not insertion order.
    insert_event(
        &pool,
        "event-b",
        "run-tie",
        &RunEvent::CycleStarted { cycle_number: 1 },
        same_timestamp,
    )
    .await
    .unwrap();
    sqlx::query!(
            "INSERT INTO events (id, run_id, event_type, payload_json, created_at) VALUES (?, ?, ?, ?, ?)",
            "event-a",
            "run-tie",
            "cycle_started",
            "{ not json",
            same_timestamp,
        )
        .execute(&pool)
        .await
        .unwrap();

    let events = list_events_for_run(&pool, "run-tie").await.unwrap();
    let ids: Vec<&str> = events.iter().map(|e| e.id()).collect();
    assert_eq!(
        ids,
        vec!["event-a", "event-b"],
        "a tied created_at must fall back to id ASC deterministically"
    );
    assert!(matches!(events[0], RunEventHistoryEntry::Undecodable(_)));
    assert!(matches!(events[1], RunEventHistoryEntry::Decoded(_)));
}
