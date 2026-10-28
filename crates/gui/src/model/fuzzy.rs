//! Case-insensitive fuzzy matching shared by search boxes.

/// How well a query matches: lower is better.
pub const PREFIX: u8 = 0;
pub const CONTAINS: u8 = 1;
pub const SUBSEQUENCE: u8 = 2;

/// Whether the characters of `needle` appear in `haystack` in order.
pub fn is_subsequence(haystack: &str, needle: &str) -> bool {
    let mut chars = haystack.chars();
    needle.chars().all(|wanted| chars.any(|c| c == wanted))
}

/// Match tier of `query` against `haystack`: prefix, then contains, then
/// subsequence. An empty query matches everything as a prefix.
pub fn tier(haystack: &str, query: &str) -> Option<u8> {
    let haystack = haystack.to_lowercase();
    let query = query.to_lowercase();
    if haystack.starts_with(&query) {
        Some(PREFIX)
    } else if haystack.contains(&query) {
        Some(CONTAINS)
    } else if is_subsequence(&haystack, &query) {
        Some(SUBSEQUENCE)
    } else {
        None
    }
}

/// Score of a whitespace-separated `query`: every word must match at least one of
/// `fields`, and the sum of each word's best tier ranks the result (lower first).
/// An empty query scores 0 for any fields.
pub fn score<'a>(fields: impl IntoIterator<Item = &'a str> + Clone, query: &str) -> Option<u32> {
    query
        .split_whitespace()
        .map(|word| {
            fields
                .clone()
                .into_iter()
                .filter_map(|field| tier(field, word))
                .min()
                .map(u32::from)
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_orders_prefix_before_contains_before_subsequence() {
        for (haystack, query, expected) in [
            ("claude-opus", "claude", Some(PREFIX)),
            ("claude-opus", "OPUS", Some(CONTAINS)),
            ("claude-opus", "cop", Some(SUBSEQUENCE)),
            ("claude-opus", "opx", None),
            ("claude-opus", "", Some(PREFIX)),
        ] {
            assert_eq!(tier(haystack, query), expected, "{haystack} / {query}");
        }
    }

    #[test]
    fn score_requires_every_word_and_sums_best_tiers() {
        let fields = ["anthropic", "claude-opus-5-5", "anthropic-subscription"];
        // Both words match a field: a prefix of the provider and a substring of the model.
        assert_eq!(score(fields, "anthropic opus"), Some(1));
        assert_eq!(score(fields, "anthropic"), Some(0));
        assert_eq!(score(fields, "anthropic zzz"), None);
        assert_eq!(score(fields, "  "), Some(0));
        assert!(score(fields, "claude") < score(fields, "opus"));
    }
}
