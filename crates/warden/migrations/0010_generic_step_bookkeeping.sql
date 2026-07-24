-- Issue #73 follow-up (trio-unification): the coder/reviewer/tester trio
-- loses its privileged place in the run loop -- every workflow step,
-- built-in or custom, now goes through the exact same execution path and
-- must get the exact same bookkeeping. `cycles.{coder,reviewer,tester}_worktree_path`
-- and the twelve `{coder,reviewer,tester}_{input,output,cache_read,cache_creation}_tokens`
-- columns (migrations/0001_initial.sql, migrations/0008_token_usage.sql) were
-- hardcoded to exactly those three role names -- a fourth, custom role (e.g.
-- `techlead`) had nowhere to record either, which is exactly why a crashed
-- run used to leak a custom step's worktree instead of it being reclaimed by
-- crash recovery like a built-in role's.
--
-- Replaced by two normalized tables, one row per (cycle, role) rather than
-- one column per literal role name -- open to any role a workflow declares,
-- not just three. `agent_processes.role` (migrations/0001_initial.sql) was
-- already a plain TEXT column with no such restriction; only these two
-- needed normalizing.
CREATE TABLE cycle_worktrees (
    cycle_id TEXT NOT NULL REFERENCES cycles (id),
    role TEXT NOT NULL,
    worktree_path TEXT NOT NULL,
    PRIMARY KEY (cycle_id, role)
);

CREATE TABLE cycle_token_usage (
    cycle_id TEXT NOT NULL REFERENCES cycles (id),
    role TEXT NOT NULL,
    input_tokens INTEGER,
    output_tokens INTEGER,
    cache_read_tokens INTEGER,
    cache_creation_tokens INTEGER,
    PRIMARY KEY (cycle_id, role)
);

CREATE INDEX idx_cycle_worktrees_cycle_id ON cycle_worktrees (cycle_id);
CREATE INDEX idx_cycle_token_usage_cycle_id ON cycle_token_usage (cycle_id);

-- Carries forward every existing row's data losslessly (pre-1.0 schema, no
-- cross-version compatibility guarantee otherwise -- migrations/0007's own
-- precedent) before the old columns are dropped below.
INSERT INTO cycle_worktrees (cycle_id, role, worktree_path)
SELECT id, 'coder', coder_worktree_path FROM cycles WHERE coder_worktree_path IS NOT NULL;
INSERT INTO cycle_worktrees (cycle_id, role, worktree_path)
SELECT id, 'reviewer', reviewer_worktree_path FROM cycles WHERE reviewer_worktree_path IS NOT NULL;
INSERT INTO cycle_worktrees (cycle_id, role, worktree_path)
SELECT id, 'tester', tester_worktree_path FROM cycles WHERE tester_worktree_path IS NOT NULL;

INSERT INTO cycle_token_usage (cycle_id, role, input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens)
SELECT id, 'coder', coder_input_tokens, coder_output_tokens, coder_cache_read_tokens, coder_cache_creation_tokens
FROM cycles
WHERE coder_input_tokens IS NOT NULL OR coder_output_tokens IS NOT NULL
   OR coder_cache_read_tokens IS NOT NULL OR coder_cache_creation_tokens IS NOT NULL;
INSERT INTO cycle_token_usage (cycle_id, role, input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens)
SELECT id, 'reviewer', reviewer_input_tokens, reviewer_output_tokens, reviewer_cache_read_tokens, reviewer_cache_creation_tokens
FROM cycles
WHERE reviewer_input_tokens IS NOT NULL OR reviewer_output_tokens IS NOT NULL
   OR reviewer_cache_read_tokens IS NOT NULL OR reviewer_cache_creation_tokens IS NOT NULL;
INSERT INTO cycle_token_usage (cycle_id, role, input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens)
SELECT id, 'tester', tester_input_tokens, tester_output_tokens, tester_cache_read_tokens, tester_cache_creation_tokens
FROM cycles
WHERE tester_input_tokens IS NOT NULL OR tester_output_tokens IS NOT NULL
   OR tester_cache_read_tokens IS NOT NULL OR tester_cache_creation_tokens IS NOT NULL;

ALTER TABLE cycles DROP COLUMN coder_worktree_path;
ALTER TABLE cycles DROP COLUMN reviewer_worktree_path;
ALTER TABLE cycles DROP COLUMN tester_worktree_path;

ALTER TABLE cycles DROP COLUMN coder_input_tokens;
ALTER TABLE cycles DROP COLUMN coder_output_tokens;
ALTER TABLE cycles DROP COLUMN coder_cache_read_tokens;
ALTER TABLE cycles DROP COLUMN coder_cache_creation_tokens;
ALTER TABLE cycles DROP COLUMN reviewer_input_tokens;
ALTER TABLE cycles DROP COLUMN reviewer_output_tokens;
ALTER TABLE cycles DROP COLUMN reviewer_cache_read_tokens;
ALTER TABLE cycles DROP COLUMN reviewer_cache_creation_tokens;
ALTER TABLE cycles DROP COLUMN tester_input_tokens;
ALTER TABLE cycles DROP COLUMN tester_output_tokens;
ALTER TABLE cycles DROP COLUMN tester_cache_read_tokens;
ALTER TABLE cycles DROP COLUMN tester_cache_creation_tokens;
