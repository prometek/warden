-- Persists the commit SHA produced by each cycle's coder, and the SHA a
-- run converged on, so that SHA remains discoverable once its worktree is
-- removed (Phase 3's git gate reads runs.converged_commit_sha to know what
-- to push). The orchestrator additionally protects each of these commits
-- with a local ref (refs/warden/runs/<run_id>/cycle-<n>) so they stay
-- reachable and safe from `git gc` after the worktree that produced them is
-- gone — worktrees share the main repository's object store, so a commit
-- with nothing pointing at it becomes ordinary unreachable garbage the
-- moment its worktree is removed.
ALTER TABLE cycles ADD COLUMN coder_commit_sha TEXT;
ALTER TABLE runs ADD COLUMN converged_commit_sha TEXT;
