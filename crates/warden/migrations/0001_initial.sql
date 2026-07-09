-- Phase 1 schema (Architecture.md §6): RUNS, CYCLES, FINDINGS, AGENT_PROCESSES.
-- EVENTS and EVIDENCE are out of scope until Phase 8 / Phase 7 respectively.

CREATE TABLE runs (
    id TEXT PRIMARY KEY,
    repo_path TEXT NOT NULL,
    branch TEXT NOT NULL,
    intent TEXT NOT NULL,
    state TEXT NOT NULL,
    max_cycles INTEGER NOT NULL,
    current_cycle INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE cycles (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs (id),
    cycle_number INTEGER NOT NULL,
    coder_worktree_path TEXT,
    reviewer_worktree_path TEXT,
    tester_worktree_path TEXT,
    started_at TEXT NOT NULL,
    ended_at TEXT
);

CREATE TABLE findings (
    id TEXT PRIMARY KEY,
    cycle_id TEXT NOT NULL REFERENCES cycles (id),
    source TEXT NOT NULL,
    severity TEXT NOT NULL,
    file TEXT,
    description TEXT NOT NULL,
    action TEXT,
    resolved_at TEXT
);

CREATE TABLE agent_processes (
    id TEXT PRIMARY KEY,
    cycle_id TEXT NOT NULL REFERENCES cycles (id),
    role TEXT NOT NULL,
    pid INTEGER NOT NULL,
    worktree_path TEXT NOT NULL,
    started_at TEXT NOT NULL,
    ended_at TEXT,
    exit_code INTEGER
);

CREATE INDEX idx_cycles_run_id ON cycles (run_id);
CREATE INDEX idx_findings_cycle_id ON findings (cycle_id);
CREATE INDEX idx_agent_processes_cycle_id ON agent_processes (cycle_id);
CREATE INDEX idx_agent_processes_pid ON agent_processes (pid);
