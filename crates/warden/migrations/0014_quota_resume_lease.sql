-- Issue #86 final review: `resuming_quota` can be visible before the
-- resumed workflow has spawned its first agent. Persist the claiming Warden
-- process so a concurrent startup can distinguish that live resume from a
-- crashed claim.
ALTER TABLE runs ADD COLUMN quota_resume_owner_pid INTEGER;
ALTER TABLE runs ADD COLUMN quota_resume_owner_started_at_unix INTEGER;
ALTER TABLE runs ADD COLUMN quota_resume_claimed_at TEXT;
