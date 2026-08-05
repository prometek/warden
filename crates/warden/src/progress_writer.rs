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
//!   the end of an agent invocation, *before* `AgentFinished` is persisted, so a replay reads an
//!   invocation's progress where it actually happened.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use sqlx::SqlitePool;
use tokio::sync::{mpsc, oneshot};
use warden_core::RunEventRecord;

use crate::db;

/// How many progress events may sit unwritten before [`ProgressWriter::record`] starts dropping.
///
/// Sized to absorb a whole burst rather than to buffer a run: at [`WRITE_BATCH_SIZE`] rows per
/// transaction the writer empties a full queue in ~16 commits, and the cap below never lets more
/// than [`MAX_PERSISTED_PROGRESS_EVENTS_PER_INVOCATION`] events enter it per invocation anyway.
/// Reaching this bound means SQLite is genuinely stalled, which is exactly when dropping beats
/// blocking.
const PROGRESS_QUEUE_CAPACITY: usize = 1024;

/// Hard cap on persisted progress events per **agent invocation**.
///
/// One invocation, not one workflow step: the convergence loop re-enters `run_agent` for the same
/// step on every cycle it reboucles into, and each entry opens a fresh budget
/// ([`ProgressWriter::begin_invocation`]). A step that loops `n` times may therefore persist up to
/// `n * MAX_PERSISTED_PROGRESS_EVENTS_PER_INVOCATION` rows, and a run's own bound is this cap times
/// its number of agent invocations -- bounded, but *not* by this number alone.
///
/// `ToolAdapter::parse_progress_line` emits one event per assistant turn, `tool_use` blocks
/// included: a talkative invocation produces several hundred where every other event kind combined
/// produces a few dozen. 500 covers an ordinary invocation end to end while bounding the worst case
/// of a runaway agent to a known number of rows instead of an unbounded one. Beyond it, progress
/// stays live-only for the rest of that invocation -- the degradation an unbounded table would
/// otherwise inflict on every later replay.
pub const MAX_PERSISTED_PROGRESS_EVENTS_PER_INVOCATION: u32 = 500;

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

/// Per-invocation budget and drop bookkeeping. Agent invocations run one at a time (the convergence
/// loop advances a single `next_step_index`, one invocation per cycle), so one set of counters per
/// run is exactly one set per invocation.
#[derive(Default)]
struct InvocationBudget {
    /// Events accepted into the queue. **Not** a count of rows written: a batch that fails to write
    /// is logged in `write_batch` and never reported back here (see the module docs).
    queued: AtomicU32,
    dropped_over_cap: AtomicU32,
    dropped_undeliverable: AtomicU32,
    /// Both `*_reported` flags exist so the cap and a saturated queue are each logged **once per
    /// invocation** -- at several hundred events per invocation, logging per drop is log spam, not
    /// signal.
    cap_reported: AtomicBool,
    drop_reported: AtomicBool,
}

/// Handle onto a run's progress writer task. Dropping it closes the queue; the task then writes
/// whatever it still holds and exits.
pub struct ProgressWriter {
    sender: mpsc::Sender<WriterMessage>,
    /// The run every record queued through this handle belongs to -- one writer per run. Held so
    /// every log below carries it, including the ones that fire once the record itself is gone.
    run_id: String,
    invocation: InvocationBudget,
}

impl ProgressWriter {
    /// Spawns the writer task that drains this handle's queue into `pool`.
    pub fn spawn(pool: SqlitePool, run_id: impl Into<String>) -> Self {
        let (sender, receiver) = mpsc::channel(PROGRESS_QUEUE_CAPACITY);
        tokio::spawn(write_loop(pool, receiver));
        Self {
            sender,
            run_id: run_id.into(),
            invocation: InvocationBudget::default(),
        }
    }

    /// A writer with no task behind it, so every [`ProgressWriter::record`] drops. Lets a test pin
    /// what the *publication* path does when persistence fails outright.
    #[cfg(test)]
    pub(crate) fn disconnected(run_id: impl Into<String>) -> Self {
        let (sender, _dropped) = mpsc::channel(1);
        Self {
            sender,
            run_id: run_id.into(),
            invocation: InvocationBudget::default(),
        }
    }

    /// Queues `record` for persistence, or drops it -- **never blocks, never fails**. Called from
    /// the synchronous stdout callback; see the module docs.
    ///
    /// Takes the record by value: the caller has already published it and holds no further use for
    /// it, so moving it into the queue spares one `RunEventRecord` clone per agent turn on a path
    /// that runs several hundred times per invocation.
    pub fn record(&self, record: RunEventRecord) {
        if self.invocation.queued.load(Ordering::Relaxed)
            >= MAX_PERSISTED_PROGRESS_EVENTS_PER_INVOCATION
        {
            self.invocation
                .dropped_over_cap
                .fetch_add(1, Ordering::Relaxed);
            if !self.invocation.cap_reported.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    run_id = %self.run_id,
                    cap = MAX_PERSISTED_PROGRESS_EVENTS_PER_INVOCATION,
                    "per-invocation cap on persisted agent progress reached; the rest of this \
                     agent invocation's progress stays live-only (logged once per invocation)"
                );
            }
            return;
        }

        match self
            .sender
            .try_send(WriterMessage::Record(Box::new(record)))
        {
            Ok(()) => {
                self.invocation.queued.fetch_add(1, Ordering::Relaxed);
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.invocation
                    .dropped_undeliverable
                    .fetch_add(1, Ordering::Relaxed);
                if !self.invocation.drop_reported.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        run_id = %self.run_id,
                        capacity = PROGRESS_QUEUE_CAPACITY,
                        "agent progress queue is saturated; dropping progress events rather than \
                         pausing the agent's stdout (logged once per invocation)"
                    );
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.invocation
                    .dropped_undeliverable
                    .fetch_add(1, Ordering::Relaxed);
                if !self.invocation.drop_reported.swap(true, Ordering::Relaxed) {
                    tracing::error!(
                        run_id = %self.run_id,
                        "agent progress writer task is gone; this run's progress will not be \
                         persisted (logged once per invocation; the run itself is unaffected)"
                    );
                }
            }
        }
    }

    /// Opens a fresh budget for one agent invocation: the cap of
    /// [`MAX_PERSISTED_PROGRESS_EVENTS_PER_INVOCATION`] applies to a single entry into `run_agent`,
    /// so a step the convergence loop reboucles into gets a new budget on every cycle.
    pub fn begin_invocation(&self) {
        self.invocation.queued.store(0, Ordering::Relaxed);
        self.invocation.dropped_over_cap.store(0, Ordering::Relaxed);
        self.invocation
            .dropped_undeliverable
            .store(0, Ordering::Relaxed);
        self.invocation.cap_reported.store(false, Ordering::Relaxed);
        self.invocation
            .drop_reported
            .store(false, Ordering::Relaxed);
    }

    /// Waits until everything queued so far has been written, then reports what this invocation
    /// dropped.
    ///
    /// Called at the end of an agent invocation, before `AgentFinished` is persisted, so replay
    /// order matches publication order. Deliberately awaits with no timeout: cutting the wait short
    /// would let `AgentFinished` be written *ahead* of progress that belongs before it, which is the
    /// one ordering guarantee this writer exists to keep. It runs on the orchestrator's own task,
    /// after the agent process has already exited -- never on the agent's stdout path.
    pub async fn flush(&self) {
        let (ack, acked) = oneshot::channel();
        if self.sender.send(WriterMessage::Flush(ack)).await.is_err() {
            tracing::error!(
                run_id = %self.run_id,
                "agent progress writer task is gone; this invocation's queued progress events were \
                 never persisted (the run itself is unaffected)"
            );
        } else if acked.await.is_err() {
            tracing::error!(
                run_id = %self.run_id,
                "agent progress writer task ended before acknowledging the end-of-invocation \
                 flush; some of this invocation's progress events may be missing (the run itself \
                 is unaffected)"
            );
        }
        self.report_invocation_drops();
    }

    fn report_invocation_drops(&self) {
        let over_cap = self.invocation.dropped_over_cap.load(Ordering::Relaxed);
        let undeliverable = self
            .invocation
            .dropped_undeliverable
            .load(Ordering::Relaxed);
        if over_cap == 0 && undeliverable == 0 {
            return;
        }
        // `queued`, not `persisted`: the writer task reports a failed batch at `error!` and has no
        // channel back to say so here -- growing one would hand a write failure a path into the run,
        // which this module exists to deny. An operator correlates the two lines.
        tracing::warn!(
            run_id = %self.run_id,
            queued = self.invocation.queued.load(Ordering::Relaxed),
            dropped_over_cap = over_cap,
            dropped_undeliverable = undeliverable,
            "agent progress events were dropped during this agent invocation; every one of them \
             was still delivered live to subscribers"
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
                run_id: "run-1".to_string(),
                invocation: InvocationBudget::default(),
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
            writer.record(progress_record(&index.to_string(), "run-1", "line"));
        }

        assert_eq!(
            writer.invocation.queued.load(Ordering::Relaxed),
            4,
            "only what fits in the queue may be accepted"
        );
        assert_eq!(
            writer
                .invocation
                .dropped_undeliverable
                .load(Ordering::Relaxed),
            6,
            "every event past capacity must be counted as dropped, not awaited"
        );
        assert_eq!(
            writer.invocation.dropped_over_cap.load(Ordering::Relaxed),
            0
        );
        assert!(
            writer.invocation.drop_reported.load(Ordering::Relaxed),
            "the first drop of an invocation must be reported"
        );
    }

    #[tokio::test]
    async fn a_dead_writer_task_drops_without_failing_the_caller() {
        let (writer, receiver) = undrained(4);
        drop(receiver);

        writer.record(progress_record("1", "run-1", "line"));

        assert_eq!(writer.invocation.queued.load(Ordering::Relaxed), 0);
        assert_eq!(
            writer
                .invocation
                .dropped_undeliverable
                .load(Ordering::Relaxed),
            1
        );
    }

    #[tokio::test]
    async fn the_per_invocation_cap_bounds_how_much_of_one_invocation_is_persisted() {
        let over_cap = MAX_PERSISTED_PROGRESS_EVENTS_PER_INVOCATION + 120;
        let (writer, receiver) = undrained(over_cap as usize * 2);

        for index in 0..over_cap {
            writer.record(progress_record(&index.to_string(), "run-1", "line"));
        }

        assert_eq!(
            receiver.len() as u32,
            MAX_PERSISTED_PROGRESS_EVENTS_PER_INVOCATION,
            "the queue must never receive more than the cap allows, even with room to spare"
        );
        assert_eq!(
            writer.invocation.dropped_over_cap.load(Ordering::Relaxed),
            120
        );
        assert!(writer.invocation.cap_reported.load(Ordering::Relaxed));
    }

    /// The budget is scoped to one entry into `run_agent`, which the convergence loop repeats for
    /// the same step on every cycle it reboucles into -- so the same step gets the whole cap again,
    /// and a run's bound is `cap * invocations`, never `cap` alone.
    #[tokio::test]
    async fn the_cap_is_per_invocation_not_per_run_or_per_step() {
        let (writer, receiver) = undrained(4096);
        for index in 0..MAX_PERSISTED_PROGRESS_EVENTS_PER_INVOCATION {
            writer.record(progress_record(&index.to_string(), "run-1", "cycle-1"));
        }

        writer.begin_invocation();
        writer.record(progress_record("next", "run-1", "cycle-2"));

        assert_eq!(
            receiver.len() as u32,
            MAX_PERSISTED_PROGRESS_EVENTS_PER_INVOCATION + 1,
            "re-entering the same step must open a fresh budget"
        );
        assert_eq!(
            writer.invocation.dropped_over_cap.load(Ordering::Relaxed),
            0
        );
    }

    #[tokio::test]
    async fn flush_persists_everything_queued_before_it_in_publication_order() {
        let (_dir, pool) = seeded_pool("run-1").await;
        let writer = ProgressWriter::spawn(pool.clone(), "run-1");

        writer.begin_invocation();
        for index in 0..200 {
            writer.record(progress_record(
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
    /// go -- but pin it against a real failing write: an `events` row whose `id` is already taken
    /// violates the primary key, and the batch's transaction rolls back whole. One writer bound to
    /// one run throughout, as production spawns it: every record it carries is that run's.
    #[tokio::test]
    async fn a_failing_write_neither_surfaces_an_error_nor_stops_the_writer() {
        let (_dir, pool) = seeded_pool("run-1").await;
        db::insert_event(
            &pool,
            "1",
            "run-1",
            &RunEvent::RunStarted {
                intent: "intent".to_string(),
                branch: "main".to_string(),
                max_cycles: 3,
            },
            "2026-08-04T00:00:00+00:00",
        )
        .await
        .unwrap();
        let writer = ProgressWriter::spawn(pool.clone(), "run-1");

        writer.begin_invocation();
        writer.record(progress_record("1", "run-1", "lost"));
        writer.flush().await;

        assert!(
            persisted_details(&pool, "run-1").await.is_empty(),
            "the failing batch must not have been written"
        );
        assert_eq!(
            db::get_run(&pool, "run-1").await.unwrap().unwrap().state,
            RunState::Pending,
            "a progress write failure must leave the run's own state untouched"
        );

        writer.begin_invocation();
        writer.record(progress_record("2", "run-1", "kept"));
        writer.flush().await;
        assert_eq!(
            persisted_details(&pool, "run-1").await,
            vec!["kept".to_string()],
            "the writer must survive a failed batch and keep persisting later ones"
        );
    }
}
