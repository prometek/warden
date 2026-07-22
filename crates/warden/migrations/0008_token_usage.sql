-- Issue #53: per-role token usage on each cycle, and its running total on
-- the owning run -- the persistence half of "display token count per
-- agent/cycle/run". All columns are nullable with no default: `NULL` means
-- "no usage was ever recorded here" (a tool that reports nothing, or a role
-- that hasn't run yet), rendered "n/a" by every reader -- never `0`, which
-- would misreport "unknown" as "known to be zero"
-- (warden_core::TokenUsage's own docs). Cache columns are additionally
-- independent of the input/output columns for the same reason: prompt
-- caching is a distinct, not universally reported, dimension of the same
-- report.
--
-- One (input, output, cache_read, cache_creation) group per role on
-- `cycles`, mirroring the existing per-role `*_worktree_path` columns
-- (migrations/0001_initial.sql) rather than a single normalized "who ran
-- what" table -- consistent with how this schema already models "one
-- cycle, three roles" everywhere else.
ALTER TABLE cycles ADD COLUMN coder_input_tokens INTEGER;
ALTER TABLE cycles ADD COLUMN coder_output_tokens INTEGER;
ALTER TABLE cycles ADD COLUMN coder_cache_read_tokens INTEGER;
ALTER TABLE cycles ADD COLUMN coder_cache_creation_tokens INTEGER;

ALTER TABLE cycles ADD COLUMN reviewer_input_tokens INTEGER;
ALTER TABLE cycles ADD COLUMN reviewer_output_tokens INTEGER;
ALTER TABLE cycles ADD COLUMN reviewer_cache_read_tokens INTEGER;
ALTER TABLE cycles ADD COLUMN reviewer_cache_creation_tokens INTEGER;

ALTER TABLE cycles ADD COLUMN tester_input_tokens INTEGER;
ALTER TABLE cycles ADD COLUMN tester_output_tokens INTEGER;
ALTER TABLE cycles ADD COLUMN tester_cache_read_tokens INTEGER;
ALTER TABLE cycles ADD COLUMN tester_cache_creation_tokens INTEGER;

-- The run-level running total, accumulated alongside each cycle-level write
-- (`db::add_cycle_role_token_usage` / `db::add_run_token_usage`, always
-- called together from the same call site) -- read directly rather than
-- re-summed from `cycles` on every access.
ALTER TABLE runs ADD COLUMN total_input_tokens INTEGER;
ALTER TABLE runs ADD COLUMN total_output_tokens INTEGER;
ALTER TABLE runs ADD COLUMN total_cache_read_tokens INTEGER;
ALTER TABLE runs ADD COLUMN total_cache_creation_tokens INTEGER;
