-- Issue #108: `agent_progress` is now persisted like every other `RunEvent`, which multiplies this
-- table's row count by an order of magnitude (hundreds of progress rows per run against a few dozen
-- lifecycle ones). Replay therefore *excludes* `agent_progress` unless the reader opts in
-- (`warden_core::ProgressReplay`), and that exclusion must not degenerate into a scan: `event_type`
-- belonged to no index at all (migration 0005 indexed `(run_id, created_at)` only).
--
-- `(run_id, created_at, event_type)` rather than `(run_id, event_type, created_at)`: every replay
-- reads one run ordered by `created_at`, so keeping `created_at` second preserves the ordering the
-- previous index already served (no sort added), while appending `event_type` makes the exclusion an
-- index-only test -- a progress row is skipped without its `payload_json` ever being read off the
-- table.
--
-- Replaces `idx_events_run_id_created_at` instead of adding to it: the new index has the old one's
-- exact key prefix, so keeping both would only add write amplification on the insert path this very
-- issue makes hotter.
DROP INDEX IF EXISTS idx_events_run_id_created_at;

CREATE INDEX idx_events_run_id_created_at_type ON events (run_id, created_at, event_type);
