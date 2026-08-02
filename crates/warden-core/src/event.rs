//! Run events published on the Event Bus and persisted to `EVENTS` for replay.

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};

/// Discriminant for a [`RunEvent`], stored separately as `EVENTS.event_type` so it can be
/// filtered/indexed in SQL without deserializing every `payload_json` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    RunStarted,
    CycleStarted,
    AgentStarted,
    /// / amendment: a live-only, declarative progress signal, translated by the run's
    /// `warden::tool_adapter::ToolAdapter` from one line of an agent's streamed output.
    AgentProgress,
    AgentFinished,
    FindingRaised,
    /// Modeled now for forward compatibility with even though nothing in this codebase can produce
    /// one yet.
    EvidenceCaptured,
    UntrustedAgentDefinitionUsed,
    RateLimitStatusUpdated,
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
            EventKind::UntrustedAgentDefinitionUsed => "untrusted_agent_definition_used",
            EventKind::RateLimitStatusUpdated => "rate_limit_status_updated",
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
            "untrusted_agent_definition_used" => Ok(EventKind::UntrustedAgentDefinitionUsed),
            "rate_limit_status_updated" => Ok(EventKind::RateLimitStatusUpdated),
            "run_finished" => Ok(EventKind::RunFinished),
            other => Err(CoreError::UnknownEventKind(other.to_string())),
        }
    }
}

/// One structured run transition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunEvent {
    RunStarted {
        intent: String,
        branch: String,
        #[serde(alias = "max_review_cycles")]
        max_cycles: u32,
    },
    CycleStarted {
        cycle_number: u32,
    },
    AgentStarted {
        role: String,
    },
    AgentProgress {
        role: String,
        detail: String,
    },
    AgentFinished {
        role: String,
        exit_code: i32,
        #[serde(default)]
        usage: Option<crate::TokenUsage>,
    },
    FindingRaised {
        cycle_number: u32,
        source: String,
        severity: String,
        file: Option<String>,
        description: String,
        action: Option<String>,
    },
    /// See [`EventKind::EvidenceCaptured`]: modeled, not yet produced by anything in this codebase.
    EvidenceCaptured {
        cycle_number: u32,
        evidence_type: String,
        file_path: String,
        description: Option<String>,
    },
    UntrustedAgentDefinitionUsed {
        role: String,
        path: String,
        canonical_path: String,
    },
    RateLimitStatusUpdated {
        role: String,
        status: crate::RateLimitStatus,
    },
    RunFinished {
        final_state: String,
    },
}

impl RunEvent {
    /// The [`EventKind`] this event's own variant corresponds to.
    pub fn kind(&self) -> EventKind {
        match self {
            RunEvent::RunStarted { .. } => EventKind::RunStarted,
            RunEvent::CycleStarted { .. } => EventKind::CycleStarted,
            RunEvent::AgentStarted { .. } => EventKind::AgentStarted,
            RunEvent::AgentProgress { .. } => EventKind::AgentProgress,
            RunEvent::AgentFinished { .. } => EventKind::AgentFinished,
            RunEvent::FindingRaised { .. } => EventKind::FindingRaised,
            RunEvent::EvidenceCaptured { .. } => EventKind::EvidenceCaptured,
            RunEvent::UntrustedAgentDefinitionUsed { .. } => {
                EventKind::UntrustedAgentDefinitionUsed
            }
            RunEvent::RateLimitStatusUpdated { .. } => EventKind::RateLimitStatusUpdated,
            RunEvent::RunFinished { .. } => EventKind::RunFinished,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunEventRecord {
    pub id: String,
    pub run_id: String,
    pub event: RunEvent,
    /// RFC3339 timestamp, same convention as every other `warden::db` timestamp column.
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UndecodableReason {
    UnknownEventType,
    PayloadDeserialize,
    /// `payload_json` decoded to a valid [`RunEvent`], but its own kind ([`RunEvent::kind`])
    /// disagrees with the row's declared `event_type` column.
    KindMismatch {
        payload_kind: String,
    },
}

impl std::fmt::Display for UndecodableReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UndecodableReason::UnknownEventType => write!(f, "unknown event_type"),
            UndecodableReason::PayloadDeserialize => {
                write!(f, "payload_json failed to deserialize")
            }
            UndecodableReason::KindMismatch { payload_kind } => {
                write!(f, "payload's own kind is {payload_kind:?}")
            }
        }
    }
}

/// One `events` row whose `payload_json`/`event_type` could not be turned into a [`RunEvent`] --
/// see [`UndecodableReason`] for why this happens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndecodableEvent {
    pub id: String,
    pub run_id: String,
    pub event_type: String,
    pub reason: UndecodableReason,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RunEventHistoryEntry {
    /// A row that decoded and validated cleanly.
    Decoded(RunEventRecord),
    Undecodable(UndecodableEvent),
}

impl RunEventHistoryEntry {
    /// The row's own `id`, whichever variant this is -- used to key deduplication/ordering the same
    /// way a [`RunEventRecord`]'s `id` already does.
    pub fn id(&self) -> &str {
        match self {
            RunEventHistoryEntry::Decoded(record) => &record.id,
            RunEventHistoryEntry::Undecodable(event) => &event.id,
        }
    }

    /// The row's own `created_at`, whichever variant this is.
    pub fn created_at(&self) -> &str {
        match self {
            RunEventHistoryEntry::Decoded(record) => &record.created_at,
            RunEventHistoryEntry::Undecodable(event) => &event.created_at,
        }
    }

    /// The decoded [`RunEventRecord`], or `None` for an [`RunEventHistoryEntry::Undecodable`] row.
    pub fn decoded(&self) -> Option<&RunEventRecord> {
        match self {
            RunEventHistoryEntry::Decoded(record) => Some(record),
            RunEventHistoryEntry::Undecodable(_) => None,
        }
    }

    /// The decoded [`RunEvent`] itself, or `None` for an [`RunEventHistoryEntry::Undecodable`] row.
    pub fn event(&self) -> Option<&RunEvent> {
        self.decoded().map(|record| &record.event)
    }
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
            EventKind::UntrustedAgentDefinitionUsed,
            EventKind::RateLimitStatusUpdated,
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
                usage: Some(crate::TokenUsage::new(100, 50, Some(10), None)),
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
            EventKind::UntrustedAgentDefinitionUsed => RunEvent::UntrustedAgentDefinitionUsed {
                role: "reviewer".to_string(),
                path: "/repo/.warden/agents/reviewer.md".to_string(),
                canonical_path: "/repo/.warden/agents/reviewer.md".to_string(),
            },
            EventKind::RateLimitStatusUpdated => RunEvent::RateLimitStatusUpdated {
                role: "coder".to_string(),
                status: crate::RateLimitStatus::new(
                    crate::RateLimitState::AllowedWarning,
                    crate::RateLimitWindow::SevenDay,
                    0.93,
                    false,
                    0.75,
                    1785686400,
                ),
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

    #[test]
    fn decoded_history_entry_exposes_its_record_and_event() {
        let record = RunEventRecord {
            id: "event-1".to_string(),
            run_id: "run-1".to_string(),
            event: RunEvent::CycleStarted { cycle_number: 1 },
            created_at: "2026-07-12T00:00:00+00:00".to_string(),
        };
        let entry = RunEventHistoryEntry::Decoded(record.clone());

        assert_eq!(entry.id(), "event-1");
        assert_eq!(entry.created_at(), "2026-07-12T00:00:00+00:00");
        assert_eq!(entry.decoded(), Some(&record));
        assert_eq!(entry.event(), Some(&record.event));
    }

    #[test]
    fn undecodable_history_entry_has_no_decoded_event() {
        let entry = RunEventHistoryEntry::Undecodable(UndecodableEvent {
            id: "event-2".to_string(),
            run_id: "run-1".to_string(),
            event_type: "run_finished".to_string(),
            reason: UndecodableReason::KindMismatch {
                payload_kind: "cycle_started".to_string(),
            },
            created_at: "2026-07-12T00:00:01+00:00".to_string(),
        });

        assert_eq!(entry.id(), "event-2");
        assert_eq!(entry.created_at(), "2026-07-12T00:00:01+00:00");
        assert_eq!(entry.decoded(), None);
        assert_eq!(entry.event(), None);
    }

    #[test]
    fn undecodable_reason_display_is_distinct_per_variant() {
        let unknown = UndecodableReason::UnknownEventType.to_string();
        let deserialize = UndecodableReason::PayloadDeserialize.to_string();
        let mismatch = UndecodableReason::KindMismatch {
            payload_kind: "cycle_started".to_string(),
        }
        .to_string();

        assert_ne!(unknown, deserialize);
        assert_ne!(deserialize, mismatch);
        assert_ne!(unknown, mismatch);
        assert!(mismatch.contains("cycle_started"), "{mismatch}");
    }

    #[test]
    fn undecodable_event_round_trips_through_json() {
        let event = UndecodableEvent {
            id: "event-2".to_string(),
            run_id: "run-1".to_string(),
            event_type: "run_finished".to_string(),
            reason: UndecodableReason::KindMismatch {
                payload_kind: "cycle_started".to_string(),
            },
            created_at: "2026-07-12T00:00:01+00:00".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let decoded: UndecodableEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, event);
    }

    #[test]
    fn agent_finished_round_trips_with_no_usage_reported() {
        let event = RunEvent::AgentFinished {
            role: "coder".to_string(),
            exit_code: 0,
            usage: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        let decoded: RunEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, event);
    }

    #[test]
    fn agent_finished_decodes_from_a_pre_issue_53_payload_missing_the_usage_field() {
        let json = r#"{"kind":"agent_finished","role":"coder","exit_code":0}"#;
        let decoded: RunEvent = serde_json::from_str(json).unwrap();
        assert_eq!(
            decoded,
            RunEvent::AgentFinished {
                role: "coder".to_string(),
                exit_code: 0,
                usage: None,
            }
        );
    }
}
