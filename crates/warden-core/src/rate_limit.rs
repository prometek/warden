//! Rate-limit/quota status (issue #84): a pure, tool-agnostic value type
//! carrying the quota signal one agent invocation's underlying CLI reported.
//! Mirrors `crate::TokenUsage`'s own "pure core type, I/O-owning crate
//! produces it" split (see that module's own docs) -- lives in
//! `warden-core` (not `warden::tool_adapter`, which actually produces it) so
//! it can ride on [`crate::RunEvent::RateLimitStatusUpdated`] without
//! `warden-core` gaining a dependency on any one tool's wire format.
//!
//! # Shaped strictly from the real payload, not #83's own proposal
//!
//! #83 (the parent issue) proposed `RateLimitStatus { used, limit, remaining,
//! resets_at, scope }`. That shape does not match anything a real CLI emits.
//! Captured directly against a live `claude` CLI (version `2.1.220 (Claude
//! Code)`, `claude -p ... --output-format stream-json --verbose`), the exact
//! wire payload is:
//!
//! ```json
//! {"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","resetsAt":1785686400,"rateLimitType":"seven_day","utilization":0.93,"isUsingOverage":false,"surpassedThreshold":0.75},"uuid":"21c05092-e021-402f-bee8-df86ed81af44","session_id":"cc97c92a-3093-421b-a6f1-ecb2b3546855"}
//! ```
//!
//! There are no `used`/`limit`/`remaining` absolute counts at all -- only a
//! `utilization` fraction, a `surpassed_threshold` fraction, a
//! `status`/`rate_limit_type` pair of strings, an `is_using_overage` bool,
//! and a `resets_at` unix-seconds timestamp. Per this issue's own guidance
//! ("ne pas inventer des champs qu'aucun CLI n'émet"), this type carries
//! exactly those fields and nothing else.
//!
//! This same capture also settles one of #83's open questions: **a reset
//! time *is* provided directly by the CLI** (`resetsAt`) -- there is no need
//! to derive one from a known window length plus the first call's own
//! timestamp, as #83 speculated might be necessary.
//!
//! **Optional by construction**: not every `--tool` adapter's CLI reports a
//! rate-limit signal at all (see
//! `warden::tool_adapter::ToolAdapter::extract_rate_limit`'s own docs) -- a
//! caller that has no [`RateLimitStatus`] for an invocation must render that
//! as "n/a", never invent one.
use serde::{Deserialize, Serialize};

/// `rate_limit_info.status` as reported by a CLI (observed value:
/// `"allowed_warning"`). Kept tolerant of any value this crate hasn't
/// observed yet -- [`RateLimitState::Other`] is the catch-all, so a future
/// CLI value (or a status this adapter's author simply never triggered) is
/// preserved verbatim rather than failing the whole parse (the same
/// tolerance convention `warden::tool_adapter::ClaudeContentBlock::Other`
/// already uses for an unrecognized wire variant).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RateLimitState {
    /// Observed against the real CLI: quota consumption crossed a warning
    /// band but requests are still allowed.
    AllowedWarning,
    /// Any value this crate hasn't specifically modeled, preserved verbatim.
    Other(String),
}

impl RateLimitState {
    pub fn as_str(&self) -> &str {
        match self {
            RateLimitState::AllowedWarning => "allowed_warning",
            RateLimitState::Other(raw) => raw,
        }
    }
}

impl From<String> for RateLimitState {
    fn from(raw: String) -> Self {
        match raw.as_str() {
            "allowed_warning" => RateLimitState::AllowedWarning,
            _ => RateLimitState::Other(raw),
        }
    }
}

impl From<RateLimitState> for String {
    fn from(state: RateLimitState) -> Self {
        state.as_str().to_string()
    }
}

impl Serialize for RateLimitState {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for RateLimitState {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(RateLimitState::from(String::deserialize(deserializer)?))
    }
}

/// `rate_limit_info.rateLimitType` as reported by a CLI (observed value:
/// `"seven_day"`) -- which quota window this report describes. Same
/// unknown-value tolerance as [`RateLimitState`], for the same reason: a
/// value this crate hasn't observed yet must still parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RateLimitWindow {
    /// Observed against the real CLI: a rolling seven-day quota window.
    SevenDay,
    /// Any value this crate hasn't specifically modeled, preserved verbatim.
    Other(String),
}

impl RateLimitWindow {
    pub fn as_str(&self) -> &str {
        match self {
            RateLimitWindow::SevenDay => "seven_day",
            RateLimitWindow::Other(raw) => raw,
        }
    }
}

impl From<String> for RateLimitWindow {
    fn from(raw: String) -> Self {
        match raw.as_str() {
            "seven_day" => RateLimitWindow::SevenDay,
            _ => RateLimitWindow::Other(raw),
        }
    }
}

impl From<RateLimitWindow> for String {
    fn from(window: RateLimitWindow) -> Self {
        window.as_str().to_string()
    }
}

impl Serialize for RateLimitWindow {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for RateLimitWindow {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(RateLimitWindow::from(String::deserialize(deserializer)?))
    }
}

/// One CLI's self-reported rate-limit/quota status for the account it's
/// running as, as of one invocation (issue #84) -- see this module's own
/// docs for the verbatim payload this is modeled from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateLimitStatus {
    pub status: RateLimitState,
    pub rate_limit_type: RateLimitWindow,
    /// Fraction of quota consumed, in `0.0..=1.0` -- **not** a percentage
    /// (`0.93` means 93%, never 93). This is the figure issue #85's
    /// anticipation threshold is meant to compare against; this issue itself
    /// implements no such comparison.
    pub utilization: f64,
    pub is_using_overage: bool,
    /// The warning-band fraction (same `0.0..=1.0` unit as `utilization`)
    /// that was crossed to produce this report.
    pub surpassed_threshold: f64,
    /// Unix epoch seconds (UTC) at which this window resets -- provided
    /// directly by the CLI, not derived from a window length plus a
    /// first-call timestamp (see this module's own docs: this settles #83's
    /// open question on the point).
    pub resets_at: i64,
}

impl RateLimitStatus {
    pub fn new(
        status: RateLimitState,
        rate_limit_type: RateLimitWindow,
        utilization: f64,
        is_using_overage: bool,
        surpassed_threshold: f64,
        resets_at: i64,
    ) -> Self {
        Self {
            status,
            rate_limit_type,
            utilization,
            is_using_overage,
            surpassed_threshold,
            resets_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> RateLimitStatus {
        RateLimitStatus::new(
            RateLimitState::AllowedWarning,
            RateLimitWindow::SevenDay,
            0.93,
            false,
            0.75,
            1785686400,
        )
    }

    #[test]
    fn rate_limit_status_round_trips_through_json() {
        let status = sample();
        let json = serde_json::to_string(&status).unwrap();
        let decoded: RateLimitStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, status);
    }

    /// The verbatim payload captured against the real CLI (issue #84) --
    /// pins this type's shape against the real wire format, not a guess.
    #[test]
    fn rate_limit_info_decodes_from_the_real_captured_payload() {
        let raw = r#"{"status":"allowed_warning","resetsAt":1785686400,"rateLimitType":"seven_day","utilization":0.93,"isUsingOverage":false,"surpassedThreshold":0.75}"#;
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct RawInfo {
            status: RateLimitState,
            resets_at: i64,
            rate_limit_type: RateLimitWindow,
            utilization: f64,
            is_using_overage: bool,
            surpassed_threshold: f64,
        }
        let decoded: RawInfo = serde_json::from_str(raw).unwrap();
        assert_eq!(decoded.status, RateLimitState::AllowedWarning);
        assert_eq!(decoded.resets_at, 1785686400);
        assert_eq!(decoded.rate_limit_type, RateLimitWindow::SevenDay);
        assert_eq!(decoded.utilization, 0.93);
        assert!(!decoded.is_using_overage);
        assert_eq!(decoded.surpassed_threshold, 0.75);
    }

    /// A future/unobserved `status`/`rateLimitType` value must still parse
    /// (tolerant enum, never a hard deserialize failure) -- otherwise a CLI
    /// upgrade emitting a status this crate never anticipated would break
    /// rate-limit extraction outright.
    #[test]
    fn unknown_status_and_rate_limit_type_values_are_preserved_verbatim() {
        assert_eq!(
            RateLimitState::from("blocked".to_string()),
            RateLimitState::Other("blocked".to_string())
        );
        assert_eq!(
            RateLimitWindow::from("five_hour".to_string()),
            RateLimitWindow::Other("five_hour".to_string())
        );
    }

    #[test]
    fn rate_limit_state_as_str_round_trips() {
        assert_eq!(RateLimitState::AllowedWarning.as_str(), "allowed_warning");
        assert_eq!(
            RateLimitState::Other("blocked".to_string()).as_str(),
            "blocked"
        );
    }

    #[test]
    fn rate_limit_window_as_str_round_trips() {
        assert_eq!(RateLimitWindow::SevenDay.as_str(), "seven_day");
        assert_eq!(
            RateLimitWindow::Other("five_hour".to_string()).as_str(),
            "five_hour"
        );
    }
}
