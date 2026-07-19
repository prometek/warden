-- Issue #43 (#37.4) / ADR-0014: splits the single `max_cycles`/`current_cycle`
-- budget-and-progress pair into two independent per-phase columns. Phase A/B
-- (#41/#42) already gate the tester behind a clean review, but until now both
-- phases still shared one budget and one `AwaitingReviewTest` state
-- (crates/warden-core/src/state.rs's own history). From here on:
--   - `max_review_cycles`/`current_review_cycle` bound coder<->reviewer
--     round trips (`RunState::Reviewing`) -- a scoped re-review triggered by
--     a tester finding's correctif is charged here too (decision #37 Q1),
--     never against the test budget.
--   - `max_test_cycles`/`current_test_cycle` bound how many times the tester
--     actually runs and comes back with a blocking finding
--     (`RunState::Testing`).
-- Existing rows: the prior single budget/progress becomes both phases'
-- starting point (the closest available approximation -- this is a
-- pre-1.0 schema with no compatibility guarantee across this breaking
-- change, matching this run's own `RunState` string values changing
-- alongside it).
ALTER TABLE runs ADD COLUMN max_review_cycles INTEGER NOT NULL DEFAULT 0;
ALTER TABLE runs ADD COLUMN max_test_cycles INTEGER NOT NULL DEFAULT 0;
ALTER TABLE runs ADD COLUMN current_review_cycle INTEGER NOT NULL DEFAULT 0;
ALTER TABLE runs ADD COLUMN current_test_cycle INTEGER NOT NULL DEFAULT 0;

UPDATE runs
SET max_review_cycles = max_cycles,
    max_test_cycles = max_cycles,
    current_review_cycle = current_cycle;

ALTER TABLE runs DROP COLUMN max_cycles;
ALTER TABLE runs DROP COLUMN current_cycle;
