-- Issue #15 / ADR-0011: persists the PR number `warden-gated` opens for a
-- run's post-Converged tail (skeleton + OpenDraft + Finalize), so a
-- crash-recovery resume can re-derive which PR to re-watch without any
-- watch-state memory of its own (GitHub is the source of truth, not this
-- column) -- `warden-gated` only ever reads it back read-only, `warden` is
-- still the sole writer. Nullable: unset until the PR is actually opened.
ALTER TABLE runs ADD COLUMN pr_number INTEGER;
