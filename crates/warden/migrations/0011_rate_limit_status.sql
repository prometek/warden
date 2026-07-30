-- Issue #84: persists the last-known rate-limit/quota status a run's tool
-- CLI reported (`claude --output-format stream-json`'s `rate_limit_event`,
-- warden::tool_adapter::ToolAdapter::extract_rate_limit). Overwritten by the
-- most recent report (`db::set_run_rate_limit_status`), never accumulated --
-- unlike migrations/0008_token_usage.sql's running totals, this is a
-- point-in-time quota snapshot, not something to sum across invocations.
--
-- All columns nullable with no default: NULL (all six, always together --
-- `set_run_rate_limit_status` always writes them as one unit) means "no
-- quota signal has ever been reported for this run" (a tool that never
-- exposes this at all), rendered "n/a" by every reader -- never a fabricated
-- zero/false/empty string.
ALTER TABLE runs ADD COLUMN rate_limit_status TEXT;
ALTER TABLE runs ADD COLUMN rate_limit_type TEXT;
ALTER TABLE runs ADD COLUMN rate_limit_utilization REAL;
ALTER TABLE runs ADD COLUMN rate_limit_is_using_overage INTEGER;
ALTER TABLE runs ADD COLUMN rate_limit_surpassed_threshold REAL;
ALTER TABLE runs ADD COLUMN rate_limit_resets_at INTEGER;
