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
    update_run_state(pool, run_id, RunState::RunningStep(0))
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
        RunState::RunningStep(0),
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

    update_run_state(&pool, "run-2", RunState::RunningStep(0))
        .await
        .unwrap();

    let run = get_run(&pool, "run-2").await.unwrap().unwrap();
    assert_eq!(run.state, RunState::RunningStep(0));

    let intermediate = list_intermediate_runs(&pool).await.unwrap();
    assert_eq!(intermediate.len(), 1);
    assert_eq!(intermediate[0].id, "run-2");
}

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
async fn durable_cleanup_migration_queues_existing_failed_runs() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("state.db");
    let migrations: Vec<_> = MIGRATOR.iter().collect();
    let cleanup_migration_index = migrations
        .iter()
        .position(|migration| migration.description.contains("durable cleanup queue"))
        .expect("0015_durable_cleanup_queue.sql must remain in the migration set");
    let previous_version = migrations[cleanup_migration_index - 1].version;

    {
        let options = SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);
        let pool = SqlitePoolOptions::new()
            .connect_with(options)
            .await
            .unwrap();
        MIGRATOR.run_to(previous_version, &pool).await.unwrap();
        insert_run(
            &pool,
            "legacy-failed-run",
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
        update_run_state(&pool, "legacy-failed-run", RunState::Failed)
            .await
            .unwrap();
        pool.close().await;
    }

    let pool = connect(&db_path).await.unwrap();
    let queued: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM run_cleanup_queue WHERE run_id = 'legacy-failed-run'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(queued, 1);
}

#[tokio::test]
async fn converged_run_is_not_listed_as_intermediate() {
    let (_dir, pool) = test_pool().await;
    insert_run(&pool, "run-3", "/tmp/repo", "main", "intent", 3, 3, 3, 5)
        .await
        .unwrap();
    update_run_state(&pool, "run-3", RunState::RunningStep(0))
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
    assert_eq!(usage.cache_read_tokens, Some(10));
    assert_eq!(usage.cache_creation_tokens, Some(3));
}

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

    connect(&db_path).await.unwrap();
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

#[tokio::test]
async fn phase_budgets_migration_remaps_pre_existing_rows_and_legacy_state_strings() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("state.db");

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

#[cfg(unix)]
#[tokio::test]
async fn backup_failure_is_a_typed_error_not_a_silent_fallback() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("state.db");

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

    let original_permissions = std::fs::metadata(dir.path()).unwrap().permissions();
    let mut readonly = original_permissions.clone();
    readonly.set_mode(0o555);
    std::fs::set_permissions(dir.path(), readonly).unwrap();

    let result = backup_before_migration(&db_path, &pool).await;

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

    let first = unique_backup_path(&db_path, timestamp).await.unwrap();
    assert_eq!(first, dir.path().join(format!("state.db.bak-{timestamp}")));

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
    update_run_state(&pool, "run-clean-failed", RunState::RunningStep(0))
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
async fn cleanup_queue_keeps_a_failed_run_pending_until_cleared() {
    let (_dir, pool) = test_pool().await;
    insert_run(
        &pool,
        "run-queued-cleanup",
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

    fail_run_with_pending_cleanup(&pool, "run-queued-cleanup")
        .await
        .unwrap();
    let pending = list_failed_runs_with_pending_cleanup(&pool).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, "run-queued-cleanup");
    assert_eq!(pending[0].state, RunState::Failed);

    clear_run_cleanup_pending(&pool, "run-queued-cleanup")
        .await
        .unwrap();
    assert!(list_failed_runs_with_pending_cleanup(&pool)
        .await
        .unwrap()
        .is_empty());
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

    clear_cycle_worktree_path(&pool, "cycle-recorded-wt", "coder")
        .await
        .unwrap();
    let pending = list_failed_runs_with_pending_cleanup(&pool).await.unwrap();
    assert!(pending.is_empty());
}

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
    update_run_state(&pool, "run-still-running", RunState::RunningStep(0))
        .await
        .unwrap();

    let pending = list_failed_runs_with_pending_cleanup(&pool).await.unwrap();
    assert!(
        pending.is_empty(),
        "a run that isn't Failed yet belongs to list_intermediate_runs, not this query"
    );
}

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

    let events = list_events_for_run(&pool, "run-events", ProgressReplay::Included)
        .await
        .unwrap();
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

    let events = list_events_for_run(&pool, "run-order", ProgressReplay::Included)
        .await
        .unwrap();
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

    let events = list_events_for_run(&pool, "run-no-events", ProgressReplay::Included)
        .await
        .unwrap();
    assert!(events.is_empty());
}

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

    let events = list_events_for_run(&pool, "run-corrupt", ProgressReplay::Included)
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
            max_cycles: 3,
        },
        "2026-07-12T00:00:00+00:00",
    )
    .await
    .unwrap();

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

    let events = list_events_for_run(&pool, "run-mixed", ProgressReplay::Included)
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
            max_cycles: 3,
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

    let events = list_events_for_run(&pool, "run-unknown-kind", ProgressReplay::Included)
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

    let events = list_events_for_run(&pool, "run-pre-26", ProgressReplay::Included)
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

#[tokio::test]
async fn run_started_payload_with_max_cycles_is_decoded() {
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

    let events = list_events_for_run(&pool, "run-pre-43", ProgressReplay::Included)
        .await
        .expect("max_cycles payload must decode");

    assert_eq!(events.len(), 1);
    assert!(matches!(
        &events[0],
        RunEventHistoryEntry::Decoded(record)
            if record.event == RunEvent::RunStarted {
                intent: "do the thing".to_string(),
                branch: "main".to_string(),
                max_cycles: 5,
            }
    ));
}

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

    let events = list_events_for_run(&pool, "run-all-bad", ProgressReplay::Included)
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

/// Issue #108: the tie-break used to be `id ASC`, which is deterministic but arbitrary -- a real
/// `id` is a UUID v4, so on a tied `created_at` the replay order was random with respect to the
/// order the events were actually published in. `rowid ASC` breaks the same tie in `warden`'s own
/// insertion order, which for this append-only table *is* publication order. `event-b` is inserted
/// first here on purpose: an `id ASC` fallback would put `event-a` first and invert them.
#[tokio::test]
async fn rows_sharing_the_same_created_at_replay_in_insertion_order_not_id_order() {
    let (_dir, pool) = test_pool().await;
    insert_run(&pool, "run-tie", "/tmp/repo", "main", "intent", 3, 3, 3, 5)
        .await
        .unwrap();

    let same_timestamp = "2026-07-12T00:00:00+00:00";
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

    let events = list_events_for_run(&pool, "run-tie", ProgressReplay::Included)
        .await
        .unwrap();
    let ids: Vec<&str> = events.iter().map(|e| e.id()).collect();
    assert_eq!(
        ids,
        vec!["event-b", "event-a"],
        "a tied created_at must fall back to insertion order deterministically"
    );
    assert!(matches!(events[0], RunEventHistoryEntry::Decoded(_)));
    assert!(matches!(events[1], RunEventHistoryEntry::Undecodable(_)));
}

#[tokio::test]
async fn insert_events_writes_a_whole_batch_or_none_of_it() {
    let (_dir, pool) = test_pool().await;
    insert_run(
        &pool,
        "run-batch",
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

    let record = |id: &str, run_id: &str, detail: &str| RunEventRecord {
        id: id.to_string(),
        run_id: run_id.to_string(),
        event: RunEvent::AgentProgress {
            role: "implementation".to_string(),
            detail: detail.to_string(),
        },
        created_at: "2026-08-04T00:00:00+00:00".to_string(),
    };

    insert_events(
        &pool,
        &[
            record("b1", "run-batch", "first"),
            record("b2", "run-batch", "second"),
        ],
    )
    .await
    .unwrap();
    let stored = list_events_for_run(&pool, "run-batch", ProgressReplay::Included)
        .await
        .unwrap();
    assert_eq!(
        stored.iter().map(|e| e.id()).collect::<Vec<_>>(),
        ["b1", "b2"]
    );

    // Second row violates the foreign key onto `runs`; the transaction must take the first row
    // down with it rather than leave a half-written batch behind.
    let error = insert_events(
        &pool,
        &[
            record("b3", "run-batch", "third"),
            record("b4", "run-that-does-not-exist", "orphan"),
        ],
    )
    .await
    .unwrap_err();
    assert!(matches!(error, WardenError::Database(_)), "{error:?}");
    assert_eq!(
        list_events_for_run(&pool, "run-batch", ProgressReplay::Included)
            .await
            .unwrap()
            .len(),
        2,
        "a failed batch must leave no partial rows behind"
    );
}

/// Issue #108, replay policy: `agent_progress` is persisted but excluded from replay unless the
/// reader asks for it. Both directions, on the same rows.
#[tokio::test]
async fn agent_progress_is_excluded_from_replay_by_default_and_returned_on_opt_in() {
    let (_dir, pool) = test_pool().await;
    insert_run(&pool, "run-mix", "/tmp/repo", "main", "intent", 3, 3, 3, 5)
        .await
        .unwrap();

    let events = [
        (
            "e1",
            RunEvent::AgentStarted {
                role: "implementation".to_string(),
            },
        ),
        (
            "e2",
            RunEvent::AgentProgress {
                role: "implementation".to_string(),
                detail: "message: reading src/lib.rs".to_string(),
            },
        ),
        (
            "e3",
            RunEvent::AgentProgress {
                role: "implementation".to_string(),
                detail: "tool: Edit".to_string(),
            },
        ),
        (
            "e4",
            RunEvent::AgentFinished {
                role: "implementation".to_string(),
                exit_code: 0,
                usage: None,
            },
        ),
    ];
    for (index, (id, event)) in events.iter().enumerate() {
        insert_event(
            &pool,
            id,
            "run-mix",
            event,
            &format!("2026-08-04T00:00:0{index}+00:00"),
        )
        .await
        .unwrap();
    }

    let default_replay = list_events_for_run(&pool, "run-mix", ProgressReplay::Excluded)
        .await
        .unwrap();
    assert_eq!(
        default_replay.iter().map(|e| e.id()).collect::<Vec<_>>(),
        vec!["e1", "e4"],
        "a default replay must carry the lifecycle events and nothing else"
    );

    let opted_in = list_events_for_run(&pool, "run-mix", ProgressReplay::Included)
        .await
        .unwrap();
    assert_eq!(
        opted_in.iter().map(|e| e.id()).collect::<Vec<_>>(),
        vec!["e1", "e2", "e3", "e4"],
        "opting in must interleave progress back in publication order"
    );
}
