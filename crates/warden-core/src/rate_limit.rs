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
//!
//! **Numeric range validation is not this module's job.** `utilization`/
//! `surpassed_threshold`/`resets_at` are plain, unvalidated `f64`/`i64`
//! here -- the boundary that must reject an implausible value (a unit
//! regression, a non-finite float, a non-positive timestamp) is
//! `warden::tool_adapter::ClaudeAdapter::extract_rate_limit`, the one place
//! that actually reads untrusted agent stdout (code-standards.md: "valider
//! toute entrée externe ... à la frontière"). This type only carries an
//! already-validated value.
use serde::{Deserialize, Serialize};

/// Upper bound on an unrecognized [`RateLimitState`]/[`RateLimitWindow`]
/// value's length (issue #84 review, finding 2): `status`/`rate_limit_type`
/// are untrusted agent-CLI output that reaches the database, the `events`
/// payload, and `warden-tui`'s rendering verbatim -- an unbounded
/// `Other(String)` would let a buggy or compromised agent smuggle an
/// arbitrarily large (or control-character-laden) string all the way to a
/// terminal. Mirrors the truncation convention
/// `warden::tool_adapter::summarize_progress_text` already uses for
/// agent-reported text (issue #33) -- collapse to one line, cap the length
/// -- applied here, at construction, so every caller downstream of
/// `From<String>` gets it for free rather than each having to remember to.
/// `64` is generous for an enum-like identifier (every value observed so far
/// -- `"allowed_warning"`, `"seven_day"` -- is under 20 chars) while still
/// bounding the damage a hostile/buggy report could do.
const MAX_OTHER_VALUE_CHARS: usize = 64;

/// Bounds an unrecognized wire value before it becomes a [`RateLimitState::Other`]/
/// [`RateLimitWindow::Other`] payload -- see [`MAX_OTHER_VALUE_CHARS`]'s own
/// docs. Replaces control characters (a raw terminal escape sequence, a
/// stray newline) with a space and collapses whitespace, the same
/// "one-line log entry, not a rendered blob" contract
/// `summarize_progress_text` enforces, then truncates by character count
/// (never by byte count, which could split a multi-byte UTF-8 sequence).
fn sanitize_other_value(raw: String) -> String {
    let control_free: String = raw
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let collapsed = control_free
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if collapsed.chars().count() <= MAX_OTHER_VALUE_CHARS {
        collapsed
    } else {
        let truncated: String = collapsed.chars().take(MAX_OTHER_VALUE_CHARS).collect();
        format!("{truncated}…")
    }
}

/// `rate_limit_info.status` as reported by a CLI (observed value:
/// `"allowed_warning"`). Kept tolerant of any value this crate hasn't
/// observed yet -- [`RateLimitState::Other`] is the catch-all, so a future
/// CLI value (or a status this adapter's author simply never triggered) is
/// preserved (bounded, see [`sanitize_other_value`]) rather than failing the
/// whole parse (the same tolerance convention
/// `warden::tool_adapter::ClaudeContentBlock::Other` already uses for an
/// unrecognized wire variant).
///
/// `#[serde(from = "String", into = "String")]`: serializes/deserializes as
/// its plain string form (`as_str`/`From<String>`), the same wire shape a
/// hand-written `Serialize`/`Deserialize` impl would produce -- no behaviour
/// change, just less code to keep in sync.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum RateLimitState {
    /// Observed against the real CLI: quota consumption crossed a warning
    /// band but requests are still allowed.
    AllowedWarning,
    /// Any value this crate hasn't specifically modeled, preserved
    /// (bounded -- see [`sanitize_other_value`]).
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
            _ => RateLimitState::Other(sanitize_other_value(raw)),
        }
    }
}

impl From<RateLimitState> for String {
    fn from(state: RateLimitState) -> Self {
        state.as_str().to_string()
    }
}

/// `rate_limit_info.rateLimitType` as reported by a CLI (observed value:
/// `"seven_day"`) -- which quota window this report describes. Same
/// unknown-value tolerance (and the same bounded `Other`, see
/// [`sanitize_other_value`]) as [`RateLimitState`], for the same reason: a
/// value this crate hasn't observed yet must still parse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum RateLimitWindow {
    /// Observed against the real CLI: a rolling seven-day quota window.
    SevenDay,
    /// Any value this crate hasn't specifically modeled, preserved
    /// (bounded -- see [`sanitize_other_value`]).
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
            _ => RateLimitWindow::Other(sanitize_other_value(raw)),
        }
    }
}

impl From<RateLimitWindow> for String {
    fn from(window: RateLimitWindow) -> Self {
        window.as_str().to_string()
    }
}

/// One CLI's self-reported rate-limit/quota status for the account it's
/// running as, as of one invocation (issue #84) -- see this module's own
/// docs for the verbatim payload this is modeled from, and for why the
/// numeric fields below are assumed already-validated (the boundary check
/// lives in `warden::tool_adapter::ClaudeAdapter::extract_rate_limit`, the
/// one place that actually reads untrusted agent stdout).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateLimitStatus {
    pub status: RateLimitState,
    pub rate_limit_type: RateLimitWindow,
    /// Fraction of quota consumed, in `0.0..=1.0` -- **not** a percentage
    /// (`0.93` means 93%, never 93). This is the figure issue #85's
    /// anticipation threshold is meant to compare against; this issue itself
    /// implements no such comparison. Can legitimately exceed `1.0` while
    /// `is_using_overage` is `true` -- see `extract_rate_limit`'s own docs
    /// for the validation bound this implies.
    pub utilization: f64,
    pub is_using_overage: bool,
    /// The warning-band fraction (same `0.0..=1.0`-ish unit as
    /// `utilization`, see that field's own docs) that was crossed to produce
    /// this report.
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

    /// **Not** the production wire-format test -- this crate has no
    /// dependency on `warden::tool_adapter::ClaudeRateLimitInfo` (the actual
    /// camelCase struct the real CLI's `rate_limit_info` object decodes
    /// into; that pinning test lives in `tool_adapter.rs`, against the
    /// verbatim captured payload). This only proves
    /// `RateLimitState`/`RateLimitWindow` behave correctly (tolerant enum,
    /// camelCase-friendly rename) when embedded in *some* `rename_all =
    /// "camelCase"` struct -- if production ever dropped its own
    /// `rename_all`, this test would keep passing, since it defines its own.
    #[test]
    fn rate_limit_state_and_window_deserialize_correctly_when_embedded_in_a_camel_case_struct() {
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

    /// Issue #84 review, finding 2: an unbounded unrecognized value must not
    /// reach the DB/event bus/TUI verbatim -- truncated to
    /// `MAX_OTHER_VALUE_CHARS`, with an ellipsis marking the cut.
    #[test]
    fn an_overly_long_unrecognized_status_is_truncated_not_stored_verbatim() {
        let huge = "x".repeat(10_000);
        let state = RateLimitState::from(huge.clone());
        let RateLimitState::Other(stored) = state else {
            panic!("expected Other for an unrecognized value");
        };
        assert!(
            stored.chars().count() <= MAX_OTHER_VALUE_CHARS + 1,
            "stored value must be bounded, got {} chars",
            stored.chars().count()
        );
        assert!(stored.ends_with('…'));
        assert_ne!(stored, huge);
    }

    /// Issue #84 review, finding 2: a control character (e.g. a raw
    /// terminal escape sequence) in an unrecognized value must never reach
    /// `warden-tui`'s rendering unescaped.
    #[test]
    fn control_characters_in_an_unrecognized_status_are_stripped() {
        let hostile = "blocked\u{1b}[31mFAKE\u{1b}[0m\nmore".to_string();
        let state = RateLimitState::from(hostile);
        let RateLimitState::Other(stored) = state else {
            panic!("expected Other for an unrecognized value");
        };
        assert!(!stored.chars().any(|c| c.is_control()), "{stored:?}");
    }
}
