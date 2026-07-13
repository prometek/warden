-- Phase 7 schema (Architecture.md §6, ADR-0009): EVIDENCE, the tangible
-- proof (Playwright screenshot/video, asciinema recording) an evidence
-- capture adapter produces after a cycle's tester agent reports a
-- successful e2e run. `finding_id` is nullable -- evidence documents either
-- a general "it works" observation for the cycle (nominal case, no finding
-- attached) or the resolution of one specific finding.
CREATE TABLE evidence (
    id TEXT PRIMARY KEY,
    cycle_id TEXT NOT NULL REFERENCES cycles (id),
    finding_id TEXT REFERENCES findings (id),
    type TEXT NOT NULL,
    file_path TEXT NOT NULL,
    description TEXT NOT NULL,
    captured_at TEXT NOT NULL
);

CREATE INDEX idx_evidence_cycle_id ON evidence (cycle_id);
CREATE INDEX idx_evidence_finding_id ON evidence (finding_id);
