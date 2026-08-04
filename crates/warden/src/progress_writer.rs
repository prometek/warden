//! Persistence of [`warden_core::RunEvent::AgentProgress`], decoupled from its publication
//! (issue #108).
//!
//! Progress is published from `on_stdout_line`, a **synchronous, infallible** callback whose
//! signature `warden_sandbox` imposes (`Fn(&str) + Send + Sync`): it can neither `await` a SQLite
//! write nor propagate an error. So persistence cannot happen there. [`ProgressWriter::record`]
//! hands the already-published record to a bounded channel drained by a dedicated task that writes
//! in batches.
//!
//! Three properties hold *by construction*, and they are the whole point of this module:
//!
//! - **It never blocks the agent.** `record` is a `try_send`. A saturated queue drops the event and
//!   says so; it never pauses the loop reading the agent's stdout.
//! - **It never fails the run.** `record` and [`ProgressWriter::flush`] both return `()`. A write
//!   error is logged and goes no further -- it cannot reach the convergence loop, the run's state,
//!   or its verdict. Progress is an observation signal; losing it breaks nothing (the probative
//!   source stays the evidence of ADR-0009).
//! - **It stays ordered against lifecycle events.** [`ProgressWriter::flush`] drains the queue at
//!   the end of a step, *before* `AgentFinished` is persisted, so a replay reads a step's progress
//!   where it actually happened.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use sqlx::SqlitePool;
use tokio::sync::{mpsc, oneshot};
use warden_core::RunEventRecord;

use crate::db;

/// How many progress events may sit unwritten before [`ProgressWriter::record`] starts dropping.
///
/// Sized to absorb a whole burst rather than to buffer a run: at [`WRITE_BATCH_SIZE`] rows per
/// transaction the writer empties a full queue in ~16 commits, and the cap below never lets more
/// than [`MAX_PERSISTED_PROGRESS_EVENTS_PER_STEP`] events enter it per step anyway. Reaching this
/// bound means SQLite is genuinely stalled, which is exactly when dropping beats blocking.
const PROGRESS_QUEUE_CAPACITY: usize = 1024;

/// Hard cap on persisted progress events per `(run, step)`.
///
/// `ToolAdapter::parse_progress_line` emits one event per assistant turn, `tool_use` blocks
/// included: a talkative step produces several hundred where every other event kind combined
/// produces a few dozen. 500 covers an ordinary step end to end while bounding the worst case of a
/// runaway agent to a known number of rows per step instead of an unbounded one. Beyond it,
/// progress stays live-only for the rest of the step -- the degradation an unbounded table would
/// otherwise inflict on every later replay.
pub const MAX_PERSISTED_PROGRESS_EVENTS_PER_STEP: u32 = 500;

/// Rows written per transaction, so one fsync is amortized over a whole batch instead of paid per
/// progress line.
const WRITE_BATCH_SIZE: usize = 64;

enum WriterMessage {
    /// `Box`ed to keep the channel's per-slot size small: `RunEventRecord` is by far the largest
    /// variant, and the queue holds [`PROGRESS_QUEUE_CAPACITY`] slots.
    Record(Box<RunEventRecord>),
    /// Answered once every message queued *before* it has been written (or has failed to write).
    Flush(oneshot::Sender<()>),
}

/// Per-step budget and drop bookkeeping. Steps run one at a time (the convergence loop advances a
/// single `next_step_index`), so one set of counters per run is exactly one set per `(run, step)`.
#[derive(Default)]
struct StepBudget {
    queued: AtomicU32,
    dropped_over_cap: AtomicU32,
    dropped_undeliverable: AtomicU32,
    /// Both `*_reported` flags exist so the cap and a saturated queue are each logged **once per
    /// step** -- at several hundred events per step, logging per drop is log spam, not signal.
    cap_reported: AtomicBool,
    drop_reported: AtomicBool,
}

/// Handle onto a run's progress writer task. Dropping it closes the queue; the task then writes
/// whatever it still holds and exits.
pub struct ProgressWriter {
    sender: mpsc::Sender<WriterMessage>,
    step: StepBudget,
}

impl ProgressWriter {
    /// Spawns the writer task that drains this handle's queue into `pool`.
    pub fn spawn(pool: SqlitePool) -> Self {
        let (sender, receiver) = mpsc::channel(PROGRESS_QUEUE_CAPACITY);
        tokio::spawn(write_loop(pool, receiver));
        Self {
            sender,
            step: StepBudget::default(),
        }
    }

    /// A writer with no task behind it, so every [`ProgressWriter::record`] drops. Lets a test pin
    /// what the *publication* path does when persistence fails outright.
    #[cfg(test)]
    pub(crate) fn disconnected() -> Self {
        let (sender, _dropped) = mpsc::channel(1);
        Self {
            sender,
            step: StepBudget::default(),
        }
    }

    /// Queues `record` for persistence, or drops it -- **never blocks, never fails**. Called from
    /// the synchronous stdout callback; see the module docs.
    pub fn record(&self, record: &RunEventRecord) {
        if self.step.queued.load(Ordering::Relaxed) >= MAX_PERSISTED_PROGRESS_EVENTS_PER_STEP {
            self.step.dropped_over_cap.fetch_add(1, Ordering::Relaxed);
            if !self.step.cap_reported.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    run_id = %record.run_id,
                    cap = MAX_PERSISTED_PROGRESS_EVENTS_PER_STEP,
                    "per-step cap on persisted agent progress reached; the rest of this step's \
                     progress stays live-only (logged once per step)"
                );
            }
            return;
        }

        match self
            .sender
            .try_send(WriterMessage::Record(Box::new(record.clone())))
        {
            Ok(()) => {
                self.step.queued.fetch_add(1, Ordering::Relaxed);
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.step
                    .dropped_undeliverable
                    .fetch_add(1, Ordering::Relaxed);
                if !self.step.drop_reported.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        run_id = %record.run_id,
                        capacity = PROGRESS_QUEUE_CAPACITY,
                        "agent progress queue is saturated; dropping progress events rather than \
                         pausing the agent's stdout (logged once per step)"
                    );
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.step
                    .dropped_undeliverable
                    .fetch_add(1, Ordering::Relaxed);
                if !self.step.drop_reported.swap(true, Ordering::Relaxed) {
                    tracing::error!(
                        run_id = %record.run_id,
                        "agent progress writer task is gone; this run's progress will not be \
                         persisted (logged once per step; the run itself is unaffected)"
                    );
                }
            }
        }
    }

    /// Opens a fresh per-step budget: the cap of [`MAX_PERSISTED_PROGRESS_EVENTS_PER_STEP`] applies
    /// per `(run, step)`, not per run.
    pub fn begin_step(&self) {
        self.step.queued.store(0, Ordering::Relaxed);
        self.step.dropped_over_cap.store(0, Ordering::Relaxed);
        self.step.dropped_undeliverable.store(0, Ordering::Relaxed);
        self.step.cap_reported.store(false, Ordering::Relaxed);
        self.step.drop_reported.store(false, Ordering::Relaxed);
    }

    /// Waits until everything queued so far has been written, then reports what this step dropped.
    ///
    /// Called at the end of a step, before `AgentFinished` is persisted, so replay order matches
    /// publication order. Deliberately awaits with no timeout: cutting the wait short would let
    /// `AgentFinished` be written *ahead* of progress that belongs before it, which is the one
    /// ordering guarantee this writer exists to keep. It runs on the orchestrator's own task, after
    /// the agent process has already exited -- never on the agent's stdout path.
    pub async fn flush(&self) {
        let (ack, acked) = oneshot::channel();
        if self.sender.send(WriterMessage::Flush(ack)).await.is_err() {
            tracing::error!(
                "agent progress writer task is gone; this step's queued progress events were \
                 never persisted (the run itself is unaffected)"
            );
        } else if acked.await.is_err() {
            tracing::error!(
                "agent progress writer task ended before acknowledging the end-of-step flush; \
                 some of this step's progress events may be missing (the run itself is unaffected)"
            );
        }
        self.report_step_drops();
    }

    fn report_step_drops(&self) {
        let over_cap = self.step.dropped_over_cap.load(Ordering::Relaxed);
        let undeliverable = self.step.dropped_undeliverable.load(Ordering::Relaxed);
        if over_cap == 0 && undeliverable == 0 {
            return;
        }
        tracing::warn!(
            persisted = self.step.queued.load(Ordering::Relaxed),
            dropped_over_cap = over_cap,
            dropped_undeliverable = undeliverable,
            "agent progress events were dropped during this step; every one of them was still \
             delivered live to subscribers"
        );
    }
}

/// Drains `receiver` until the last [`ProgressWriter`] handle is dropped, writing in batches.
async fn write_loop(pool: SqlitePool, mut receiver: mpsc::Receiver<WriterMessage>) {
    let mut batch: Vec<RunEventRecord> = Vec::with_capacity(WRITE_BATCH_SIZE);
    while let Some(message) = receiver.recv().await {
        let mut ack = None;
        match message {
            WriterMessage::Record(record) => batch.push(*record),
            WriterMessage::Flush(sender) => ack = Some(sender),
        }
        // Opportunistic, never awaited: whatever already arrived joins this batch, and a flush
        // marker ends it immediately so nothing queued before the marker is left unwritten.
        while ack.is_none() && batch.len() < WRITE_BATCH_SIZE {
            match receiver.try_recv() {
                Ok(WriterMessage::Record(record)) => batch.push(*record),
                Ok(WriterMessage::Flush(sender)) => ack = Some(sender),
                Err(_) => break,
            }
        }
        write_batch(&pool, &mut batch).await;
        if let Some(sender) = ack {
            // An `Err` here only means the flusher stopped waiting; there is no failure to report.
            let _ = sender.send(());
        }
    }
}

/// Writes `batch` in one transaction and empties it. A failure is logged and dropped on purpose:
/// this task has no channel back to the run, and must not grow one (see module docs).
async fn write_batch(pool: &SqlitePool, batch: &mut Vec<RunEventRecord>) {
    if batch.is_empty() {
        return;
    }
    if let Err(error) = db::insert_events(pool, batch).await {
        tracing::error!(
            %error,
            dropped = batch.len(),
            "failed to persist a batch of agent progress events; they are lost, and the run \
             carries on unaffected"
        );
    }
    batch.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use warden_core::{ProgressReplay, RunEvent, RunState};

    /// A handle whose queue nothing drains, so saturation is reachable without a single sleep.
    fn undrained(capacity: usize) -> (ProgressWriter, mpsc::Receiver<WriterMessage>) {
        let (sender, receiver) = mpsc::channel(capacity);
        (
            ProgressWriter {
                sender,
                step: StepBudget::default(),
            },
            receiver,
        )
    }

    fn progress_record(id: &str, run_id: &str, detail: &str) -> RunEventRecord {
        RunEventRecord {
            id: id.to_string(),
            run_id: run_id.to_string(),
            event: RunEvent::AgentProgress {
                role: "implementation".to_string(),
                detail: detail.to_string(),
            },
            created_at: format!("2026-08-04T00:00:00.{id:0>9}+00:00"),
        }
    }

    async fn seeded_pool(run_id: &str) -> (TempDir, SqlitePool) {
        let dir = TempDir::new().unwrap();
        let pool = db::connect(&dir.path().join("state.db")).await.unwrap();
        db::insert_run(&pool, run_id, "/tmp/repo", "main", "intent", 3, 3, 1, 3)
            .await
            .unwrap();
        (dir, pool)
    }

    async fn persisted_details(pool: &SqlitePool, run_id: &str) -> Vec<String> {
        db::list_events_for_run(pool, run_id, ProgressReplay::Included)
            .await
            .unwrap()
            .into_iter()
            .filter_map(|entry| match entry.event() {
                Some(RunEvent::AgentProgress { detail, .. }) => Some(detail.clone()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn a_saturated_queue_drops_instead_of_blocking_the_caller() {
        let (writer, _receiver) = undrained(4);

        for index in 0..10 {
            writer.record(&progress_record(&index.to_string(), "run-1", "line"));
        }

        assert_eq!(
            writer.step.queued.load(Ordering::Relaxed),
            4,
            "only what fits in the queue may be accepted"
        );
        assert_eq!(
            writer.step.dropped_undeliverable.load(Ordering::Relaxed),
            6,
            "every event past capacity must be counted as dropped, not awaited"
        );
        assert_eq!(writer.step.dropped_over_cap.load(Ordering::Relaxed), 0);
        assert!(
            writer.step.drop_reported.load(Ordering::Relaxed),
            "the first drop of a step must be reported"
        );
    }

    #[tokio::test]
    async fn a_dead_writer_task_drops_without_failing_the_caller() {
        let (writer, receiver) = undrained(4);
        drop(receiver);

        writer.record(&progress_record("1", "run-1", "line"));

        assert_eq!(writer.step.queued.load(Ordering::Relaxed), 0);
        assert_eq!(writer.step.dropped_undeliverable.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn the_per_step_cap_bounds_how_much_of_one_step_is_persisted() {
        let over_cap = MAX_PERSISTED_PROGRESS_EVENTS_PER_STEP + 120;
        let (writer, receiver) = undrained(over_cap as usize * 2);

        for index in 0..over_cap {
            writer.record(&progress_record(&index.to_string(), "run-1", "line"));
        }

        assert_eq!(
            receiver.len() as u32,
            MAX_PERSISTED_PROGRESS_EVENTS_PER_STEP,
            "the queue must never receive more than the cap allows, even with room to spare"
        );
        assert_eq!(writer.step.dropped_over_cap.load(Ordering::Relaxed), 120);
        assert!(writer.step.cap_reported.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn the_cap_is_per_step_not_per_run() {
        let (writer, receiver) = undrained(4096);
        for index in 0..MAX_PERSISTED_PROGRESS_EVENTS_PER_STEP {
            writer.record(&progress_record(&index.to_string(), "run-1", "step-1"));
        }

        writer.begin_step();
        writer.record(&progress_record("next", "run-1", "step-2"));

        assert_eq!(
            receiver.len() as u32,
            MAX_PERSISTED_PROGRESS_EVENTS_PER_STEP + 1,
            "a new step must open a fresh budget"
        );
        assert_eq!(writer.step.dropped_over_cap.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn flush_persists_everything_queued_before_it_in_publication_order() {
        let (_dir, pool) = seeded_pool("run-1").await;
        let writer = ProgressWriter::spawn(pool.clone());

        writer.begin_step();
        for index in 0..200 {
            writer.record(&progress_record(
                &index.to_string(),
                "run-1",
                &format!("line-{index}"),
            ));
        }
        writer.flush().await;

        let details = persisted_details(&pool, "run-1").await;
        let expected: Vec<String> = (0..200).map(|index| format!("line-{index}")).collect();
        assert_eq!(details, expected);
    }

    /// The guarantee is structural -- `record`/`flush` return `()`, so a write error has nowhere to
    /// go -- but pin it against a real failing write: an `events` row whose `run_id` violates the
    /// foreign key onto `runs`.
    #[tokio::test]
    async fn a_failing_write_neither_surfaces_an_error_nor_stops_the_writer() {
        let (_dir, pool) = seeded_pool("run-1").await;
        let writer = ProgressWriter::spawn(pool.clone());

        writer.begin_step();
        writer.record(&progress_record("1", "run-that-does-not-exist", "lost"));
        writer.flush().await;

        assert!(
            persisted_details(&pool, "run-that-does-not-exist")
                .await
                .is_empty(),
            "the failing batch must not have been written"
        );
        assert_eq!(
            db::get_run(&pool, "run-1").await.unwrap().unwrap().state,
            RunState::Pending,
            "a progress write failure must leave the run's own state untouched"
        );

        writer.begin_step();
        writer.record(&progress_record("2", "run-1", "kept"));
        writer.flush().await;
        assert_eq!(
            persisted_details(&pool, "run-1").await,
            vec!["kept".to_string()],
            "the writer must survive a failed batch and keep persisting later ones"
        );
    }
}
