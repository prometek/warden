-- Issue #73: the pipeline moves from hardcoded code (a closed coder -> gate
-- review -> gate test sequence) to a user-definable `.warden/workflow.yaml`
-- (`warden_core::workflow::Workflow`). `RunState` is now step-indexed
-- (`RunState::RunningStep(u32)`/`RunState::StepCyclesExceeded(u32)`) instead
-- of the closed `Reviewing`/`Testing`/`MaxReviewCyclesExceeded`/
-- `MaxTestCyclesExceeded` pair ADR-0014 introduced -- see
-- `crates/warden-core/src/state.rs`'s own docs.
--
-- `total_steps` is how many steps the run's own resolved workflow has
-- (`workflow.steps.len()`), persisted once at `insert_run` time and read
-- back alongside `state` every time `RunState::validate_transition` needs to
-- decide whether a step is the workflow's *last* one (`warden::orchestrator`'s
-- single `transition` seam, and crash recovery's own `validate_transition`
-- calls). Every run this codebase has ever created before this migration
-- only ever ran the built-in three-step pipeline (coder, reviewer, tester),
-- so `DEFAULT 3` is not a guess for existing rows -- it is exactly what they
-- already were.
--
-- `max_extra_step_cycles`/`current_extra_step_cycle` are the single shared
-- cycle budget/counter for any workflow step beyond the built-in reviewer/
-- tester pair (e.g. a custom `techlead` step) -- the built-in pair keeps its
-- own existing, unchanged `max_review_cycles`/`max_test_cycles`/
-- `current_review_cycle`/`current_test_cycle` columns. `DEFAULT 5` matches
-- this crate's own CLI default for `--max-cycles` (`main.rs`); existing rows
-- never had any extra step to begin with, so the default is inert for them.
ALTER TABLE runs ADD COLUMN total_steps INTEGER NOT NULL DEFAULT 3;
ALTER TABLE runs ADD COLUMN max_extra_step_cycles INTEGER NOT NULL DEFAULT 5;
ALTER TABLE runs ADD COLUMN current_extra_step_cycle INTEGER NOT NULL DEFAULT 0;

-- A run persisted mid-flight in one of the removed state strings must still
-- parse after this upgrade, or crash recovery (`list_intermediate_runs`)
-- would silently stop seeing it -- the same concern migrations/0007's own
-- "code review of this migration's first commit (LOW)" note already raised
-- for the `AwaitingReviewTest` -> `Reviewing`/`Testing` split. `reviewing`/
-- `testing` are exactly `running_step:1`/`running_step:2` under the new
-- step-indexed scheme (the built-in default workflow's own reviewer/tester
-- indices), and `max_review_cycles_exceeded`/`max_test_cycles_exceeded` are
-- exactly `step_cycles_exceeded:1`/`step_cycles_exceeded:2` -- a lossless,
-- exact remap, not an approximation.
UPDATE runs SET state = 'running_step:1' WHERE state = 'reviewing';
UPDATE runs SET state = 'running_step:2' WHERE state = 'testing';
UPDATE runs SET state = 'step_cycles_exceeded:1' WHERE state = 'max_review_cycles_exceeded';
UPDATE runs SET state = 'step_cycles_exceeded:2' WHERE state = 'max_test_cycles_exceeded';
