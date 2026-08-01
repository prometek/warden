//! Rate-limit/quota status: a pure, tool-agnostic value type carrying the quota signal one agent
//! invocation's underlying CLI reported.
use serde::{Deserialize, Serialize};

const MAX_OTHER_VALUE_CHARS: usize = 64;

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

/// `rate_limit_info.status` as reported by a CLI (observed value: `"allowed_warning"`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum RateLimitState {
    /// Observed against the real CLI: quota consumption crossed a warning band but requests are
    /// still allowed.
    AllowedWarning,
    /// Unknown bounded value preserved for forward compatibility.
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

/// `rate_limit_info.rateLimitType` as reported by a CLI (observed value: `"seven_day"`) -- which
/// quota window this report describes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum RateLimitWindow {
    /// Observed against the real CLI: a rolling seven-day quota window.
    SevenDay,
    /// Unknown bounded value preserved for forward compatibility.
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateLimitStatus {
    pub status: RateLimitState,
    pub rate_limit_type: RateLimitWindow,
    /// Fraction of quota consumed, in `0.0..=1.0` -- **not** a percentage (`0.93` means 93%, never
    /// 93).
    pub utilization: f64,
    pub is_using_overage: bool,
    pub surpassed_threshold: f64,
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

    #[test]
    fn control_characters_in_an_unrecognized_status_are_stripped() {
        let hostile = "blocked\u{1b}[31mFAKE\u{1b}[0m\nmore".to_string();
        let state = RateLimitState::from(hostile);
        let RateLimitState::Other(stored) = state else {
            panic!("expected Other for an unrecognized value");
        };
        assert!(!stored.chars().any(|c| c.is_control()), "{stored:?}");
    }

    #[test]
    fn c1_control_characters_are_stripped_too_not_just_ascii_escapes() {
        let hostile = "blocked\u{9b}31mFAKE".to_string();
        let RateLimitState::Other(stored) = RateLimitState::from(hostile) else {
            panic!("expected Other for an unrecognized value");
        };
        assert!(!stored.chars().any(|c| c.is_control()), "{stored:?}");
        assert!(!stored.contains('\u{9b}'), "{stored:?}");
    }

    #[test]
    fn truncation_never_splits_a_multi_byte_character() {
        for raw in ["é".repeat(10_000), "🚀".repeat(200)] {
            let RateLimitState::Other(stored) = RateLimitState::from(raw) else {
                panic!("expected Other for an unrecognized value");
            };
            assert_eq!(
                stored.chars().count(),
                MAX_OTHER_VALUE_CHARS + 1,
                "expected {MAX_OTHER_VALUE_CHARS} chars plus the ellipsis, got {stored:?}"
            );
            assert!(stored.ends_with('…'));
        }
    }

    #[test]
    fn sanitizing_an_already_sanitized_value_is_stable() {
        for raw in [
            "x".repeat(10_000),
            "🚀".repeat(200),
            format!("{} tail", "y".repeat(63)),
            "  spaced   out  ".to_string(),
            "blocked\u{1b}[31m".to_string(),
            String::new(),
            "   ".to_string(),
        ] {
            let RateLimitState::Other(once) = RateLimitState::from(raw.clone()) else {
                panic!("expected Other for an unrecognized value");
            };
            let RateLimitState::Other(twice) = RateLimitState::from(once.clone()) else {
                panic!("expected Other for an unrecognized value");
            };
            assert_eq!(once, twice, "sanitizing {raw:?} is not idempotent");
        }
    }

    #[test]
    fn a_hostile_unrecognized_status_stays_bounded_across_a_json_round_trip() {
        let hostile = format!("blocked\u{1b}[31m{}", "x".repeat(10_000));
        let status = RateLimitStatus::new(
            RateLimitState::from(hostile),
            RateLimitWindow::SevenDay,
            0.93,
            false,
            0.75,
            1785686400,
        );
        let decoded: RateLimitStatus =
            serde_json::from_str(&serde_json::to_string(&status).unwrap()).unwrap();
        assert_eq!(decoded, status, "a round-trip must not drift the value");
        let RateLimitState::Other(stored) = decoded.status else {
            panic!("expected Other for an unrecognized value");
        };
        assert!(stored.chars().count() <= MAX_OTHER_VALUE_CHARS + 1);
        assert!(!stored.chars().any(|c| c.is_control()));
    }
}
