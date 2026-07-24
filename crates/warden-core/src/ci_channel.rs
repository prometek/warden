//! ADR-0011 (issue #15): the reverse-channel message `warden-gated` sends
//! back to `warden` once a run's post-`Converged` tail (push + PR
//! open/finalize + CI watch) reaches a terminal outcome. Pure/serializable
//! shape only -- the socket transport (bind/connect, `0600` hardening) is
//! I/O and lives in `warden`'s listener / `warden-gated`'s sender,
//! mirroring the `warden_core::RunEvent` / `warden::event_bus` split.
//!
//! **One message per run** (ADR-0011: "un seul message terminal par run,
//! pas un flux"), parsed at the receiving boundary with the same rigor as
//! `warden_gated::notification::parse_post_receive_line`: any malformed
//! shape is a typed error, never silently ignored.

use serde::{Deserialize, Serialize};

use crate::convergence::{CiOutcome, Finding, FindingSource, Severity};
use crate::error::{CoreError, Result};

/// Wire shape of one [`Finding`] riding across the reverse channel --
/// mirrors `RunEvent::FindingRaised`'s convention (the already-validated
/// `as_str()` string form for `source`/`severity`, re-validated back into
/// the real enums at receipt): `Finding` itself doesn't derive
/// `Serialize`/`Deserialize`, and round-tripping through this wire struct is
/// the same boundary-validation pattern `warden::db` already uses for
/// SQLite columns. Public (rather than private) only because it appears in
/// [`CiWatchOutcome::ChecksFailed`]'s public field -- callers should still
/// prefer [`CiWatchOutcome::checks_failed`]/[`CiWatchOutcome::findings`] over
/// constructing/matching this directly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiFindingWire {
    source: String,
    severity: String,
    file: Option<String>,
    description: String,
    action: Option<String>,
}

impl CiFindingWire {
    fn from_finding(finding: &Finding) -> Self {
        Self {
            source: finding.source.as_str().to_string(),
            severity: finding.severity.as_str().to_string(),
            file: finding.file.clone(),
            description: finding.description.clone(),
            action: finding.action.clone(),
        }
    }

    fn into_finding(self) -> Result<Finding> {
        Ok(Finding {
            source: FindingSource::parse(&self.source)?,
            severity: Severity::parse(&self.severity)?,
            file: self.file,
            description: self.description,
            action: self.action,
        })
    }
}

/// Mirrors `warden_gated::ci_watcher::WatchOutcome` one-for-one, plus
/// [`CiWatchOutcome::GateFailed`] for when the tail fails before a watch
/// could even start (the skeleton push, `OpenDraft`, or `Finalize` itself
/// failed) -- `warden-core` cannot depend on `warden-gated` (ADR-0006), so
/// this is the type that actually crosses the wire; `warden-gated`'s sender
/// maps its own `WatchOutcome` into this shape via the `checks_failed`/etc.
/// constructors below.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum CiWatchOutcome {
    Merged,
    Closed,
    ChecksPassed,
    ChecksFailed {
        findings: Vec<CiFindingWire>,
    },
    TimedOut,
    /// The tail failed before a watch could even start -- distinct from
    /// `ChecksFailed`, since no PR ever reached a checkable state at all.
    GateFailed {
        reason: String,
    },
}

impl CiWatchOutcome {
    pub fn merged() -> Self {
        CiWatchOutcome::Merged
    }

    pub fn closed() -> Self {
        CiWatchOutcome::Closed
    }

    pub fn checks_passed() -> Self {
        CiWatchOutcome::ChecksPassed
    }

    pub fn checks_failed(findings: &[Finding]) -> Self {
        CiWatchOutcome::ChecksFailed {
            findings: findings.iter().map(CiFindingWire::from_finding).collect(),
        }
    }

    pub fn timed_out() -> Self {
        CiWatchOutcome::TimedOut
    }

    pub fn gate_failed(reason: impl Into<String>) -> Self {
        CiWatchOutcome::GateFailed {
            reason: reason.into(),
        }
    }

    /// Maps to the pure [`CiOutcome`] `decide_next_state_after_ci` consumes,
    /// or `None` for `GateFailed` -- an infrastructure failure has no CI
    /// signal to interpret, and is always handled as an unconditional
    /// `Failed` by the caller rather than through the cycle-budget logic
    /// `decide_next_state_after_ci` applies to `ChecksFailed`.
    pub fn as_ci_outcome(&self) -> Option<CiOutcome> {
        match self {
            CiWatchOutcome::Merged => Some(CiOutcome::Merged),
            CiWatchOutcome::Closed => Some(CiOutcome::Closed),
            CiWatchOutcome::ChecksPassed => Some(CiOutcome::ChecksPassed),
            CiWatchOutcome::ChecksFailed { .. } => Some(CiOutcome::ChecksFailed),
            CiWatchOutcome::TimedOut => Some(CiOutcome::TimedOut),
            CiWatchOutcome::GateFailed { .. } => None,
        }
    }

    /// The blocking findings this outcome carries -- only `ChecksFailed` has
    /// any (`FindingSource::Ci`, per `ci_watcher::failed_checks_to_findings`).
    /// A malformed individual finding fails the whole call, never salvaged
    /// partially (code-standards.md: never trust untrusted input piecemeal).
    pub fn findings(&self) -> Result<Vec<Finding>> {
        match self {
            CiWatchOutcome::ChecksFailed { findings } => findings
                .iter()
                .cloned()
                .map(CiFindingWire::into_finding)
                .collect(),
            _ => Ok(Vec::new()),
        }
    }
}

/// The one terminal message `warden-gated` sends back to `warden` per run
/// (ADR-0011), keyed by `run_id` so `warden`'s per-run listener knows which
/// run it applies to even though nothing else about the transport carries
/// that identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiResultMessage {
    pub run_id: String,
    /// The PR `warden-gated` opened for this run, if it got that far --
    /// `None` only when `outcome` is `GateFailed` and the failure happened
    /// before `OpenDraft` itself returned a PR handle.
    pub pr_number: Option<u64>,
    pub outcome: CiWatchOutcome,
}

impl CiResultMessage {
    /// Serializes this message to the exact wire form [`parse_ci_result_message`]
    /// parses back -- one JSON object per connection (ADR-0011: one terminal
    /// message per run, not a line-delimited stream).
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(self)
            .map_err(|error| CoreError::MalformedCiResultMessage(error.to_string()))
    }
}

/// Parses one raw reverse-channel payload into a [`CiResultMessage`], with
/// the same rigor as `warden_gated::notification::parse_post_receive_line`:
/// malformed JSON, an unknown `outcome` tag, a blank `run_id`, or an
/// unparsable finding inside `ChecksFailed` are all typed errors -- never
/// silently ignored or partially trusted (code-standards.md: "valider toute
/// entrée externe ... à la frontière").
pub fn parse_ci_result_message(raw: &str) -> Result<CiResultMessage> {
    let message: CiResultMessage = serde_json::from_str(raw)
        .map_err(|error| CoreError::MalformedCiResultMessage(error.to_string()))?;

    if message.run_id.trim().is_empty() {
        return Err(CoreError::MalformedCiResultMessage(
            "run_id must not be blank".to_string(),
        ));
    }

    // Validate any embedded findings now, at the boundary, rather than
    // leaving a caller to discover a malformed one later inside the
    // convergence loop.
    message.outcome.findings()?;

    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ci_finding(description: &str) -> Finding {
        Finding {
            source: FindingSource::Ci,
            severity: Severity::Blocking,
            file: None,
            description: description.to_string(),
            action: None,
        }
    }

    #[test]
    fn every_outcome_round_trips_through_json() {
        let messages = vec![
            CiResultMessage {
                run_id: "run-1".to_string(),
                pr_number: Some(7),
                outcome: CiWatchOutcome::merged(),
            },
            CiResultMessage {
                run_id: "run-1".to_string(),
                pr_number: Some(7),
                outcome: CiWatchOutcome::closed(),
            },
            CiResultMessage {
                run_id: "run-1".to_string(),
                pr_number: Some(7),
                outcome: CiWatchOutcome::checks_passed(),
            },
            CiResultMessage {
                run_id: "run-1".to_string(),
                pr_number: Some(7),
                outcome: CiWatchOutcome::checks_failed(&[ci_finding("build failed")]),
            },
            CiResultMessage {
                run_id: "run-1".to_string(),
                pr_number: Some(7),
                outcome: CiWatchOutcome::timed_out(),
            },
            CiResultMessage {
                run_id: "run-1".to_string(),
                pr_number: None,
                outcome: CiWatchOutcome::gate_failed("skeleton push failed"),
            },
        ];

        for message in messages {
            let json = message.to_json().unwrap();
            let decoded = parse_ci_result_message(&json).unwrap();
            assert_eq!(decoded, message);
        }
    }

    #[test]
    fn checks_failed_findings_round_trip_with_source_ci() {
        let message = CiResultMessage {
            run_id: "run-1".to_string(),
            pr_number: Some(1),
            outcome: CiWatchOutcome::checks_failed(&[ci_finding("lint failed")]),
        };
        let decoded = parse_ci_result_message(&message.to_json().unwrap()).unwrap();
        let findings = decoded.outcome.findings().unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].source, FindingSource::Ci);
        assert_eq!(findings[0].severity, Severity::Blocking);
        assert_eq!(findings[0].description, "lint failed");
    }

    #[test]
    fn as_ci_outcome_maps_every_watch_outcome_except_gate_failed() {
        assert_eq!(
            CiWatchOutcome::merged().as_ci_outcome(),
            Some(CiOutcome::Merged)
        );
        assert_eq!(
            CiWatchOutcome::closed().as_ci_outcome(),
            Some(CiOutcome::Closed)
        );
        assert_eq!(
            CiWatchOutcome::checks_passed().as_ci_outcome(),
            Some(CiOutcome::ChecksPassed)
        );
        assert_eq!(
            CiWatchOutcome::checks_failed(&[ci_finding("x")]).as_ci_outcome(),
            Some(CiOutcome::ChecksFailed)
        );
        assert_eq!(
            CiWatchOutcome::timed_out().as_ci_outcome(),
            Some(CiOutcome::TimedOut)
        );
        assert_eq!(CiWatchOutcome::gate_failed("boom").as_ci_outcome(), None);
    }

    #[test]
    fn rejects_malformed_json() {
        assert!(matches!(
            parse_ci_result_message("not json"),
            Err(CoreError::MalformedCiResultMessage(_))
        ));
    }

    #[test]
    fn rejects_an_unknown_outcome_tag() {
        let json = r#"{"run_id":"run-1","pr_number":1,"outcome":"bogus"}"#;
        assert!(matches!(
            parse_ci_result_message(json),
            Err(CoreError::MalformedCiResultMessage(_))
        ));
    }

    #[test]
    fn rejects_a_blank_run_id() {
        let json = r#"{"run_id":"  ","pr_number":1,"outcome":{"outcome":"merged"}}"#;
        assert!(matches!(
            parse_ci_result_message(json),
            Err(CoreError::MalformedCiResultMessage(_))
        ));
    }

    #[test]
    fn rejects_an_unknown_finding_source_inside_checks_failed() {
        // Issue #73: roles are now open (workflow-defined) -- a non-blank
        // source like "ghost" no longer trips `FindingSource::parse` itself
        // (see its own docs). Only a *blank* source still does, propagated
        // as-is: still a typed `CoreError`, just a more specific variant
        // than the generic `MalformedCiResultMessage` catch-all used for
        // shape errors.
        let json = r#"{"run_id":"run-1","pr_number":1,"outcome":{"outcome":"checks_failed","findings":[{"source":"   ","severity":"blocking","file":null,"description":"x","action":null}]}}"#;
        assert!(matches!(
            parse_ci_result_message(json),
            Err(CoreError::UnknownFindingSource(_))
        ));
    }
}
