-- Adds pid_started_at_unix to agent_processes so crash recovery can tell a
-- still-running process apart from an unrelated process that reused the
-- same PID after a reboot: PIDs are a small, recycled namespace, so a bare
-- PID match is not sufficient (Architecture.md §9, Disaster Recovery).
--
-- 0 is the sentinel for "unknown" (see warden::process::UNKNOWN_START_TIME)
-- — never a real Unix start time in practice, since that would be 1970.
ALTER TABLE agent_processes ADD COLUMN pid_started_at_unix INTEGER NOT NULL DEFAULT 0;
