//! Run events published on the Event Bus and persisted to `EVENTS` for replay
//! (Architecture.md §6, ADR-0008). Pure, in-memory types only -- the actual
//! socket transport and SQLite persistence are I/O and live in the `warden`
//! (publisher) and `warden-tui` (subscriber/replay reader) crates; this
//! module is only the shared wire/row shape both sides agree on, so a
//! payload written by one is never silently misread by the other.

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};

/// Discriminant for a [`RunEvent`], stored separately as `EVENTS.event_type`
/// so it can be filtered/indexed in SQL without deserializing every
/// `payload_json` row. Mirrors [`RunEvent`]'s own serde tag one-for-one --
/// [`RunEvent::kind`] and this module's tests keep the two in sync, the same
/// `as_str`/`parse` pattern [`crate::RunState`] and [`crate::AgentRole`]
/// already use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    RunStarted,
    CycleStarted,
    AgentStarted,
    /// Issue #33 / ADR-0008 amendment: a live-only, declarative progress
    /// signal, translated by the run's `warden::tool_adapter::ToolAdapter`
    /// from one line of an agent's streamed output. Unlike every other
    /// variant here, a [`RunEvent::AgentProgress`] is **never** persisted as
    /// an `events` row (`warden::orchestrator::Orchestrator` broadcasts it
    /// straight on the Event Bus, bypassing `db::insert_event` entirely) --
    /// this discriminant still exists so [`RunEvent::kind`] stays a total
    /// function, not because anything ever writes `"agent_progress"` to a
    /// `event_type` column.
    AgentProgress,
    AgentFinished,
    FindingRaised,
    /// Modeled now for forward compatibility with Phase 7 (Evidence Capture
    /// Adapter, issue #7) even though nothing in this codebase can produce
    /// one yet -- the `EVIDENCE` table Phase 7 introduces doesn't exist on
    /// this branch, so there is no data source to raise it from. Kept here
    /// rather than added later so the wire/row protocol doesn't need a
    /// breaking change once Phase 7 lands.
    EvidenceCaptured,
    RunFinished,
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::RunStarted => "run_started",
            EventKind::CycleStarted => "cycle_started",
            EventKind::AgentStarted => "agent_started",
            EventKind::AgentProgress => "agent_progress",
            EventKind::AgentFinished => "agent_finished",
            EventKind::FindingRaised => "finding_raised",
            EventKind::EvidenceCaptured => "evidence_captured",
            EventKind::RunFinished => "run_finished",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "run_started" => Ok(EventKind::RunStarted),
            "cycle_started" => Ok(EventKind::CycleStarted),
            "agent_started" => Ok(EventKind::AgentStarted),
            "agent_progress" => Ok(EventKind::AgentProgress),
            "agent_finished" => Ok(EventKind::AgentFinished),
            "finding_raised" => Ok(EventKind::FindingRaised),
            "evidence_captured" => Ok(EventKind::EvidenceCaptured),
            "run_finished" => Ok(EventKind::RunFinished),
            other => Err(CoreError::UnknownEventKind(other.to_string())),
        }
    }
}

/// A single structured event describing one significant transition of a run
/// (Architecture.md §5.4/§6). This is everything a `warden-tui` view needs
/// to render a live or replayed run -- the TUI never re-derives meaning from
/// raw agent output or ad-hoc SQL joins of its own (code-standards.md, "TUI
/// (ratatui)": "aucune logique métier dans le code de rendu").
///
/// `role`/`source`/`severity`/`final_state` are carried as their already-
/// validated `as_str()` string form (not the `warden_core` enums
/// themselves): those enums don't derive `Serialize`/`Deserialize`, and
/// round-tripping through their stable string form here is the same
/// boundary convention `warden::db` already uses for SQLite columns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunEvent {
    RunStarted {
        intent: String,
        branch: String,
        max_cycles: u32,
    },
    CycleStarted {
        cycle_number: u32,
    },
    AgentStarted {
        role: String,
    },
    /// A single declarative progress signal reported by an agent while it is
    /// still running (issue #33): what the agent's own tool CLI says it is
    /// doing right now (a streamed assistant message, or a `tool_use`
    /// block), translated by that tool's own
    /// `warden::tool_adapter::ToolAdapter` impl -- this type carries no
    /// knowledge of any one CLI's wire format (e.g. `stream-json` never
    /// leaks past the adapter that produces `detail`).
    ///
    /// **Declarative, not verified**: this is what the agent *reports*
    /// itself doing, not a checked execution trace -- ADR-0009's evidence
    /// keeps that role, and this event must never be presented as one.
    ///
    /// **Live-only** (ADR-0008 amendment, issue #33): unlike every other
    /// variant in this enum, a value of this variant is *never* persisted to
    /// the `events` table -- see [`EventKind::AgentProgress`]'s own docs. A
    /// `warden-tui` that attaches after the fact never replays it; it is
    /// only ever seen by a subscriber watching the run live at the moment it
    /// was published, exactly like the bus already tolerates losing events
    /// for a slow subscriber (`warden::event_bus`).
    AgentProgress {
        role: String,
        detail: String,
    },
    AgentFinished {
        role: String,
        exit_code: i32,
    },
    FindingRaised {
        cycle_number: u32,
        source: String,
        severity: String,
        file: Option<String>,
        description: String,
        action: Option<String>,
    },
    /// See [`EventKind::EvidenceCaptured`]: modeled, not yet produced by
    /// anything in this codebase (Phase 7, issue #7).
    EvidenceCaptured {
        cycle_number: u32,
        evidence_type: String,
        file_path: String,
        description: Option<String>,
    },
    RunFinished {
        final_state: String,
    },
}

impl RunEvent {
    /// The [`EventKind`] this event's own variant corresponds to. Kept as an
    /// explicit method (rather than relying solely on the serde tag) so
    /// callers that need the discriminant without going through JSON --
    /// e.g. `warden::db::insert_event` picking `EVENTS.event_type` -- have a
    /// single, testable source of truth for the mapping.
    pub fn kind(&self) -> EventKind {
        match self {
            RunEvent::RunStarted { .. } => EventKind::RunStarted,
            RunEvent::CycleStarted { .. } => EventKind::CycleStarted,
            RunEvent::AgentStarted { .. } => EventKind::AgentStarted,
            RunEvent::AgentProgress { .. } => EventKind::AgentProgress,
            RunEvent::AgentFinished { .. } => EventKind::AgentFinished,
            RunEvent::FindingRaised { .. } => EventKind::FindingRaised,
            RunEvent::EvidenceCaptured { .. } => EventKind::EvidenceCaptured,
            RunEvent::RunFinished { .. } => EventKind::RunFinished,
        }
    }
}

/// One persisted/published event, together with the identity/ordering
/// metadata needed to store it as an `EVENTS` row *and* to deduplicate it on
/// the wire: `warden-tui` subscribes to the live socket **before** querying
/// SQLite for history (to avoid the gap a subscribe-after-query order would
/// risk), so it needs `id` to recognize a live event that's also about to
/// show up in the history query and skip replaying it twice (see
/// Architecture.md §5.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunEventRecord {
    pub id: String,
    pub run_id: String,
    pub event: RunEvent,
    /// RFC3339 timestamp, same convention as every other `warden::db`
    /// timestamp column.
    pub created_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_kind() -> Vec<EventKind> {
        vec![
            EventKind::RunStarted,
            EventKind::CycleStarted,
            EventKind::AgentStarted,
            EventKind::AgentProgress,
            EventKind::AgentFinished,
            EventKind::FindingRaised,
            EventKind::EvidenceCaptured,
            EventKind::RunFinished,
        ]
    }

    #[test]
    fn event_kind_round_trips_through_its_string_form() {
        for kind in every_kind() {
            assert_eq!(EventKind::parse(kind.as_str()).unwrap(), kind);
        }
    }

    #[test]
    fn unknown_event_kind_string_is_a_typed_error_not_a_panic() {
        assert_eq!(
            EventKind::parse("bogus"),
            Err(CoreError::UnknownEventKind("bogus".to_string()))
        );
    }

    fn sample(kind: EventKind) -> RunEvent {
        match kind {
            EventKind::RunStarted => RunEvent::RunStarted {
                intent: "do the thing".to_string(),
                branch: "main".to_string(),
                max_cycles: 5,
            },
            EventKind::CycleStarted => RunEvent::CycleStarted { cycle_number: 1 },
            EventKind::AgentStarted => RunEvent::AgentStarted {
                role: "coder".to_string(),
            },
            EventKind::AgentProgress => RunEvent::AgentProgress {
                role: "coder".to_string(),
                detail: "running `cargo test`".to_string(),
            },
            EventKind::AgentFinished => RunEvent::AgentFinished {
                role: "coder".to_string(),
                exit_code: 0,
            },
            EventKind::FindingRaised => RunEvent::FindingRaised {
                cycle_number: 1,
                source: "reviewer".to_string(),
                severity: "blocking".to_string(),
                file: Some("src/lib.rs".to_string()),
                description: "missing test".to_string(),
                action: Some("add one".to_string()),
            },
            EventKind::EvidenceCaptured => RunEvent::EvidenceCaptured {
                cycle_number: 1,
                evidence_type: "image".to_string(),
                file_path: ".warden/evidence/1/screenshot.png".to_string(),
                description: Some("login screen".to_string()),
            },
            EventKind::RunFinished => RunEvent::RunFinished {
                final_state: "converged".to_string(),
            },
        }
    }

    #[test]
    fn every_variant_reports_its_own_kind() {
        for kind in every_kind() {
            assert_eq!(sample(kind).kind(), kind);
        }
    }

    #[test]
    fn run_event_round_trips_through_json() {
        for kind in every_kind() {
            let event = sample(kind);
            let json = serde_json::to_string(&event).unwrap();
            let decoded: RunEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, event);
        }
    }

    #[test]
    fn run_event_record_round_trips_through_json() {
        let record = RunEventRecord {
            id: "event-1".to_string(),
            run_id: "run-1".to_string(),
            event: RunEvent::RunFinished {
                final_state: "converged".to_string(),
            },
            created_at: "2026-07-12T00:00:00+00:00".to_string(),
        };
        let json = serde_json::to_string(&record).unwrap();
        let decoded: RunEventRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, record);
    }
}
