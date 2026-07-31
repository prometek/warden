-- Issue #85: a quota suspension is durable state, not an agent failure.
-- `state` carries the timestamp as part of its stable wire value; this column
-- makes the reset time queryable without parsing that value and keeps legacy
-- runs coherent (`NULL` means they were never quota-suspended).
ALTER TABLE runs ADD COLUMN quota_resets_at INTEGER;
