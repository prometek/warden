use super::*;

/// Persists one [`RunEvent`] (Phase 8, ADR-0008) as an `events` row. `id`
/// and `created_at` are supplied by the caller rather than generated here
/// (unlike most other `insert_*` functions in this module): the orchestrator
/// needs the *exact same* id/timestamp to also appear on the live Event Bus
/// broadcast (see `event_bus::EventBus::publish`), so a `warden-tui` that
/// subscribes to the bus before querying history can deduplicate an event it
/// already saw live against the same event showing up in a later history
/// query, by id.
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

/// A `evidence` row, with `type` already validated into
/// [`EvidenceType`] (ADR-0009, issue #7).
#[derive(Debug, Clone)]
pub struct Evidence {
    pub id: String,
    pub cycle_id: String,
    pub finding_id: Option<String>,
    pub evidence_type: EvidenceType,
    /// The eventual repo-relative destination path
    /// (`.warden/evidence/<cycle_number>/<filename>`) an artifact is stored
    /// under once committed (see `crate::evidence`) -- this column is
    /// written at capture time, before the commit itself happens, since it's
    /// deterministic and never changes (only the underlying bytes move from
    /// local scratch storage into the repo, at convergence).
    pub file_path: String,
    pub description: String,
    pub captured_at: String,
}

/// Records one artifact an evidence capture adapter produced for `cycle_id`
/// (ADR-0009). `finding_id` is `None` for the nominal case -- evidence
/// documenting that a cycle's behaviour works, not the resolution of one
/// specific finding.
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

/// Raw shape of an `events` row as decoded by sqlx, before `event_type` and
/// `payload_json` have been validated into a [`RunEvent`]. Kept private:
/// [`RunEventHistoryEntry`] is the only form that ever leaves this module.
struct EventRow {
    id: String,
    run_id: String,
    event_type: String,
    payload_json: String,
    created_at: String,
}

/// Validates one row's `event_type`/`payload_json` into a
/// [`RunEventHistoryEntry`] -- infallible (issue #58): a row that fails to
/// decode/validate becomes an explicit `Undecodable` marker rather than an
/// `Err`, so one bad row (e.g. an event-payload reshape that shipped without
/// a migration rewriting existing rows) never fails
/// [`list_events_for_run`]'s whole query and takes out the rest of the run's
/// history with it (code-standards.md: "no silent fallback, no
/// symptom-masking guards" -- the row is never silently dropped either).
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

/// Every event recorded for `run_id`, oldest first -- the full history a
/// late-attaching `warden-tui` replays before switching to the live socket
/// stream (Architecture.md §5.4). Ordered by `created_at` then `id` so two
/// events sharing the same (second-resolution) timestamp still come back in
/// a stable, deterministic order rather than SQLite's unspecified row order.
///
/// Issue #58: a row that can't be decoded/validated is returned as a typed
/// [`RunEventHistoryEntry::Undecodable`] entry, never dropped and never a
/// reason for the whole query to fail -- only a genuine query/connection
/// error (`?` below) still does that.
pub async fn list_events_for_run(
    pool: &SqlitePool,
    run_id: &str,
) -> Result<Vec<RunEventHistoryEntry>> {
    let rows = sqlx::query_as!(
        EventRow,
        r#"
        SELECT id as "id!", run_id, event_type, payload_json, created_at
        FROM events
        WHERE run_id = ?
        ORDER BY created_at ASC, id ASC
        "#,
        run_id,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(row_to_history_entry).collect())
}

/// One `evidence` row together with the `cycle_number` it belongs to -- the
/// bare `evidence` table only carries `cycle_id`, but `pr_summary`'s
/// Evidence section formatting (issue #7) groups/orders by cycle number.
pub struct EvidenceWithCycle {
    pub cycle_number: u32,
    pub evidence: Evidence,
}

/// Every evidence row captured across `run_id`'s cycles, ordered by cycle
/// then capture time -- used to build the Evidence section of the finalized
/// PR body (ADR-0009) and to find the artifacts still on local scratch
/// storage that need committing into the repo at convergence
/// (`evidence::commit_evidence_into_repo`).
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
