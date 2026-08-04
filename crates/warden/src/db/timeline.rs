use super::*;

pub async fn insert_event(
    pool: &SqlitePool,
    id: &str,
    run_id: &str,
    event: &RunEvent,
    created_at: &str,
) -> Result<()> {
    let event_type = event.kind().as_str();
    let payload_json = serde_json::to_string(event)?;
    sqlx::query!(
        "INSERT INTO events (id, run_id, event_type, payload_json, created_at) VALUES (?, ?, ?, ?, ?)",
        id,
        run_id,
        event_type,
        payload_json,
        created_at,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Inserts a whole batch of already-published records in **one transaction** -- the batched
/// counterpart of [`insert_event`], used by [`crate::progress_writer`] so one fsync is amortized
/// over a burst of agent progress instead of paid per line.
///
/// Insertion order inside the batch is preserved, and so is the batch's `rowid` order: that is what
/// [`list_events_for_run`] falls back on to break a tie on `created_at`.
pub async fn insert_events(pool: &SqlitePool, records: &[RunEventRecord]) -> Result<()> {
    let mut tx = pool.begin().await?;
    for record in records {
        let event_type = record.event.kind().as_str();
        let payload_json = serde_json::to_string(&record.event)?;
        sqlx::query!(
            "INSERT INTO events (id, run_id, event_type, payload_json, created_at) VALUES (?, ?, ?, ?, ?)",
            record.id,
            record.run_id,
            event_type,
            payload_json,
            record.created_at,
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// A `evidence` row, with `type` already validated into [`EvidenceType`].
#[derive(Debug, Clone)]
pub struct Evidence {
    pub id: String,
    pub cycle_id: String,
    pub finding_id: Option<String>,
    pub evidence_type: EvidenceType,
    pub file_path: String,
    pub description: String,
    pub captured_at: String,
}

/// Records one artifact an evidence capture adapter produced for `cycle_id`.
#[allow(clippy::too_many_arguments)]
pub async fn insert_evidence(
    pool: &SqlitePool,
    id: &str,
    cycle_id: &str,
    finding_id: Option<&str>,
    evidence_type: EvidenceType,
    file_path: &str,
    description: &str,
) -> Result<()> {
    let now = now_rfc3339();
    let evidence_type = evidence_type.as_str();
    sqlx::query!(
        "INSERT INTO evidence (id, cycle_id, finding_id, type, file_path, description, captured_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
        id,
        cycle_id,
        finding_id,
        evidence_type,
        file_path,
        description,
        now,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Raw shape of an `events` row as decoded by sqlx, before `event_type` and `payload_json` have
/// been validated into a [`RunEvent`].
struct EventRow {
    id: String,
    run_id: String,
    event_type: String,
    payload_json: String,
    created_at: String,
}

/// Validates one row's `event_type`/`payload_json` into a [`RunEventHistoryEntry`].
fn row_to_history_entry(row: EventRow) -> RunEventHistoryEntry {
    let reason = match EventKind::parse(&row.event_type) {
        Ok(declared_kind) => match serde_json::from_str::<RunEvent>(&row.payload_json) {
            Ok(event) if event.kind() == declared_kind => {
                return RunEventHistoryEntry::Decoded(RunEventRecord {
                    id: row.id,
                    run_id: row.run_id,
                    event,
                    created_at: row.created_at,
                });
            }
            Ok(event) => UndecodableReason::KindMismatch {
                payload_kind: event.kind().as_str().to_string(),
            },
            Err(_) => UndecodableReason::PayloadDeserialize,
        },
        Err(_) => UndecodableReason::UnknownEventType,
    };
    RunEventHistoryEntry::Undecodable(UndecodableEvent {
        id: row.id,
        run_id: row.run_id,
        event_type: row.event_type,
        reason,
        created_at: row.created_at,
    })
}

/// Every event recorded for `run_id`, oldest first -- the full history a late-attaching `warden-
/// tui` replays before switching to the live socket stream (Architecture.md §5.4).
///
/// `progress` decides whether `agent_progress` rows are part of that history; they are not, unless
/// the caller opts in (issue #108, see [`warden_core::ProgressReplay`]).
///
/// **Ordering.** `created_at ASC` is publication order: the timestamp is stamped where the event is
/// published, not where it is written, so a progress event batched to disk later than a lifecycle
/// event published after it still replays in the right place. `rowid ASC` breaks a tie
/// deterministically *in insertion order* -- a random `id` (UUID v4) would break it arbitrarily
/// instead, and `warden` being the table's only writer makes rowid order the run's own write order.
pub async fn list_events_for_run(
    pool: &SqlitePool,
    run_id: &str,
    progress: ProgressReplay,
) -> Result<Vec<RunEventHistoryEntry>> {
    let excluded_kind = EventKind::AgentProgress.as_str();
    let rows = if progress.includes_progress() {
        sqlx::query_as!(
            EventRow,
            r#"
            SELECT id as "id!", run_id, event_type, payload_json, created_at
            FROM events
            WHERE run_id = ?
            ORDER BY created_at ASC, rowid ASC
            "#,
            run_id,
        )
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query_as!(
            EventRow,
            r#"
            SELECT id as "id!", run_id, event_type, payload_json, created_at
            FROM events
            WHERE run_id = ? AND event_type <> ?
            ORDER BY created_at ASC, rowid ASC
            "#,
            run_id,
            excluded_kind,
        )
        .fetch_all(pool)
        .await?
    };

    Ok(rows.into_iter().map(row_to_history_entry).collect())
}

/// One `evidence` row together with the `cycle_number` it belongs to.
pub struct EvidenceWithCycle {
    pub cycle_number: u32,
    pub evidence: Evidence,
}

pub async fn list_evidence_for_run(
    pool: &SqlitePool,
    run_id: &str,
) -> Result<Vec<EvidenceWithCycle>> {
    let rows = sqlx::query!(
        r#"
        SELECT evidence.id as "id!", evidence.cycle_id as "cycle_id!", evidence.finding_id,
               evidence.type as "evidence_type!", evidence.file_path as "file_path!",
               evidence.description as "description!", evidence.captured_at as "captured_at!",
               cycles.cycle_number as "cycle_number!"
        FROM evidence
        JOIN cycles ON cycles.id = evidence.cycle_id
        WHERE cycles.run_id = ?
        ORDER BY cycles.cycle_number ASC, evidence.captured_at ASC
        "#,
        run_id,
    )
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|r| {
            Ok(EvidenceWithCycle {
                cycle_number: checked_u32(r.cycle_number, "cycles.cycle_number")?,
                evidence: Evidence {
                    id: r.id,
                    cycle_id: r.cycle_id,
                    finding_id: r.finding_id,
                    evidence_type: EvidenceType::parse(&r.evidence_type)?,
                    file_path: r.file_path,
                    description: r.description,
                    captured_at: r.captured_at,
                },
            })
        })
        .collect()
}
