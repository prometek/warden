-- Issue #86: a quota suspension exits the orchestrator process, so every
-- in-memory convergence value needed to resume at the next workflow boundary
-- must survive that exit. One checkpoint belongs to one run and is replaced
-- atomically on every re-suspension.
CREATE TABLE quota_continuations (
    run_id TEXT PRIMARY KEY REFERENCES runs (id),
    config_json TEXT NOT NULL,
    state_json TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
