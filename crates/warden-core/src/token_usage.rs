//! Token usage: a pure, tool-agnostic value type carrying what one agent invocation's underlying
//! CLI reported spending.
use serde::{Deserialize, Serialize};

/// Tokens spent by a single agent invocation, as reported by its underlying tool CLI.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: Option<u64>,
    #[serde(default)]
    pub cache_creation_tokens: Option<u64>,
}

impl TokenUsage {
    pub fn new(
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: Option<u64>,
        cache_creation_tokens: Option<u64>,
    ) -> Self {
        Self {
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_creation_tokens,
        }
    }

    pub fn total(&self) -> u64 {
        self.input_tokens
            + self.output_tokens
            + self.cache_read_tokens.unwrap_or(0)
            + self.cache_creation_tokens.unwrap_or(0)
    }

    /// Combines `self` with `other`, field by field -- the aggregation primitive every "per
    /// cycle"/"run total" rollup is built from, agent-invocation by agent-invocation.
    pub fn merge(&self, other: &TokenUsage) -> TokenUsage {
        TokenUsage {
            input_tokens: self.input_tokens + other.input_tokens,
            output_tokens: self.output_tokens + other.output_tokens,
            cache_read_tokens: merge_optional(self.cache_read_tokens, other.cache_read_tokens),
            cache_creation_tokens: merge_optional(
                self.cache_creation_tokens,
                other.cache_creation_tokens,
            ),
        }
    }

    pub fn sum<'a>(usages: impl IntoIterator<Item = &'a TokenUsage>) -> Option<TokenUsage> {
        usages.into_iter().fold(None, |acc, usage| match acc {
            None => Some(*usage),
            Some(acc) => Some(acc.merge(usage)),
        })
    }
}

fn merge_optional(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (None, None) => None,
        (left, right) => Some(left.unwrap_or(0) + right.unwrap_or(0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn total_sums_every_dimension_when_all_are_present() {
        let usage = TokenUsage::new(100, 50, Some(10), Some(5));
        assert_eq!(usage.total(), 165);
    }

    #[test]
    fn total_treats_absent_cache_fields_as_zero() {
        let usage = TokenUsage::new(100, 50, None, None);
        assert_eq!(usage.total(), 150);
    }

    #[test]
    fn merge_adds_input_and_output_tokens_unconditionally() {
        let a = TokenUsage::new(100, 50, None, None);
        let b = TokenUsage::new(20, 10, None, None);
        let merged = a.merge(&b);
        assert_eq!(merged.input_tokens, 120);
        assert_eq!(merged.output_tokens, 60);
    }

    #[test]
    fn merge_keeps_a_cache_field_none_when_neither_side_ever_reported_it() {
        let a = TokenUsage::new(100, 50, None, None);
        let b = TokenUsage::new(20, 10, None, None);
        let merged = a.merge(&b);
        assert_eq!(merged.cache_read_tokens, None);
        assert_eq!(merged.cache_creation_tokens, None);
    }

    #[test]
    fn merge_preserves_a_cache_total_even_when_the_other_side_never_reports_it() {
        let a = TokenUsage::new(100, 50, Some(30), None);
        let b = TokenUsage::new(20, 10, None, None);
        let merged = a.merge(&b);
        assert_eq!(merged.cache_read_tokens, Some(30));
    }

    #[test]
    fn merge_adds_cache_fields_when_both_sides_report_them() {
        let a = TokenUsage::new(100, 50, Some(30), Some(5));
        let b = TokenUsage::new(20, 10, Some(15), Some(2));
        let merged = a.merge(&b);
        assert_eq!(merged.cache_read_tokens, Some(45));
        assert_eq!(merged.cache_creation_tokens, Some(7));
    }

    #[test]
    fn sum_of_no_usages_is_none_not_zero() {
        assert_eq!(TokenUsage::sum(std::iter::empty()), None);
    }

    #[test]
    fn sum_folds_every_usage_into_one_running_total() {
        let usages = vec![
            TokenUsage::new(100, 50, Some(10), None),
            TokenUsage::new(20, 10, None, Some(5)),
        ];
        let total = TokenUsage::sum(&usages).unwrap();
        assert_eq!(total.input_tokens, 120);
        assert_eq!(total.output_tokens, 60);
        assert_eq!(total.cache_read_tokens, Some(10));
        assert_eq!(total.cache_creation_tokens, Some(5));
    }

    #[test]
    fn token_usage_round_trips_through_json() {
        let usage = TokenUsage::new(100, 50, Some(10), Some(5));
        let json = serde_json::to_string(&usage).unwrap();
        let decoded: TokenUsage = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, usage);
    }

    #[test]
    fn token_usage_decodes_from_json_missing_the_optional_cache_fields() {
        let json = r#"{"input_tokens":100,"output_tokens":50}"#;
        let decoded: TokenUsage = serde_json::from_str(json).unwrap();
        assert_eq!(decoded, TokenUsage::new(100, 50, None, None));
    }
}
