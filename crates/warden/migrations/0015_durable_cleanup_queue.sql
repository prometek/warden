CREATE TABLE run_cleanup_queue (
    run_id TEXT PRIMARY KEY REFERENCES runs(id) ON DELETE CASCADE
);

INSERT INTO run_cleanup_queue (run_id)
SELECT id FROM runs WHERE state = 'failed';
