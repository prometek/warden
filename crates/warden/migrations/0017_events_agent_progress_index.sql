-- Issue #108: `agent_progress` is now persisted like every other `RunEvent`, which multiplies this
-- table's row count by an order of magnitude (hundreds of progress rows per run against a few dozen
-- lifecycle ones). Replay therefore *excludes* `agent_progress` unless the reader opts in
-- (`warden_core::ProgressReplay`), and that exclusion must not degenerate into a scan: `event_type`
-- belonged to no index at all (migration 0005 indexed `(run_id, created_at)` only).
--
-- `(run_id, created_at, event_type)` rather than `(run_id, event_type, created_at)`: every replay
-- reads one run ordered by `created_at`, so `created_at` stays second, and appending `event_type`
-- makes the exclusion an *in-index* test -- an `agent_progress` row is skipped without its table row,
-- `payload_json` included, ever being read.
--
-- That third column is not free, and the trade is deliberate. SQLite appends `rowid` to every index
-- key, so `(run_id, created_at)` natively satisfied `ORDER BY created_at ASC, rowid ASC`; interposing
-- `event_type` no longer does. Measured on sqlite3 3.51.0, 20 000 rows, after `ANALYZE`, both replay
-- variants (progress included and excluded) now plan as:
--     SEARCH events USING INDEX idx_events_run_id_created_at_type (run_id=?)
--     USE TEMP B-TREE FOR LAST TERM OF ORDER BY
-- i.e. a *partial* sort of the last ORDER BY term only, within each `created_at` group. Those groups
-- hold one row in practice (a tie is the rare case the `rowid` tie-break exists for), so the sort is
-- paid on nothing, while the skipped table lookups are paid on the majority of rows -- progress
-- outnumbers every other kind by an order of magnitude. No index buys both properties: SQLite refuses
-- `rowid` as an index column outright (`no such column: rowid`).
--
-- Replaces `idx_events_run_id_created_at` instead of adding to it: the new index has the old one's
-- exact key prefix, so keeping both would only add write amplification on the insert path this very
-- issue makes hotter.
DROP INDEX IF EXISTS idx_events_run_id_created_at;

CREATE INDEX idx_events_run_id_created_at_type ON events (run_id, created_at, event_type);
