-- Phase 8 (Architecture.md §6, ADR-0008, issue #8): persists every event
-- published on the Event Bus, so a late-attaching `warden-tui` can replay a
-- run's full history from here before switching over to the live socket
-- stream, with no gap between replay and live (issue #8 acceptance
-- criterion 1). `event_type` duplicates the discriminant already carried by
-- `payload_json` (see warden_core::RunEvent's own serde tag) so it can be
-- filtered/indexed in SQL without deserializing every row.
CREATE TABLE events (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs (id),
    event_type TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE INDEX idx_events_run_id_created_at ON events (run_id, created_at);
