//! Output-only defaults for individually verified Claude model IDs.
//!
//! Sources checked 2026-11-16:
//! https://platform.claude.com/docs/en/models/overview
//! https://platform.claude.com/docs/en/models/{slug}/overview for each slug below.
//! The overview's embedded model data confirms 128000 for Fable 5.2 and
//! Opus/Sonnet/Haiku 5.5. Individual pages' embedded maxOutputTokens confirm
//! 128000 for Fable 5/5.1, Mythos 5/5.1/5.2, Opus 5/4.8/4.7/4.6 and
//! Sonnet 5/4.6; 64000 for Opus/Sonnet/Haiku 4.5.
//! Retired Opus 4/4.1 and Sonnet 4 individual pages expose no integer: use
//! conservative decimal 32000/64000 from the overview's 32K/64K (not *1024).
//! Unlisted IDs, including preview and future versions, retain 4096.

pub(super) const LEGACY_DEFAULT: u64 = 4096;

const KNOWN_LIMITS: &[(&str, u64)] = &[
    ("claude-fable-5-2", 128_000),
    ("claude-opus-5-5", 128_000),
    ("claude-sonnet-5-5", 128_000),
    ("claude-haiku-5-5", 128_000),
    ("claude-mythos-5-2", 128_000),
    ("claude-fable-5-1", 128_000),
    ("claude-mythos-5-1", 128_000),
    ("claude-fable-5", 128_000),
    ("claude-mythos-5", 128_000),
    ("claude-opus-5", 128_000),
    ("claude-opus-4-8", 128_000),
    ("claude-opus-4-7", 128_000),
    ("claude-opus-4-6", 128_000),
    ("claude-sonnet-5", 128_000),
    ("claude-sonnet-4-6", 128_000),
    ("claude-opus-4-5", 64_000),
    ("claude-sonnet-4-5", 64_000),
    ("claude-sonnet-4", 64_000),
    ("claude-haiku-4-5", 64_000),
    ("claude-opus-4-1", 32_000),
    ("claude-opus-4", 32_000),
];

pub(super) fn default_limit(model: &str) -> u64 {
    KNOWN_LIMITS
        .iter()
        .find_map(|(id, limit)| {
            // The same eight ASCII-digit date suffix allowed by model_mode.
            (model == *id
                || model.strip_prefix(id).is_some_and(|suffix| {
                    suffix.strip_prefix('-').is_some_and(|date| {
                        date.len() == 8 && date.bytes().all(|digit| digit.is_ascii_digit())
                    })
                }))
            .then_some(*limit)
        })
        .unwrap_or(LEGACY_DEFAULT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_exact_known_ids_and_eight_digit_dates_are_accepted() {
        for (id, limit) in KNOWN_LIMITS {
            assert_eq!(default_limit(id), *limit);
            assert_eq!(default_limit(&format!("{id}-20260101")), *limit);
            for suffix in [
                "-latest",
                "-preview",
                "-2026010",
                "-202601011",
                "-2026010x",
                "-unverified",
            ] {
                assert_eq!(default_limit(&format!("{id}{suffix}")), LEGACY_DEFAULT);
            }
        }
        for unknown in [
            "claude-opus-6",
            "claude-opus-4-9",
            "claude-sonnet-5-6",
            "claude-mythos-preview",
            "other",
        ] {
            assert_eq!(default_limit(unknown), LEGACY_DEFAULT);
        }
    }

    #[test]
    fn known_defaults_do_not_exceed_overlapping_catalog_fixture_limits() {
        let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        let fixture = std::fs::read_to_string(
            std::path::Path::new(&manifest).join("../catalog/src/fixture.json"),
        )
        .unwrap();
        let api: serde_json::Value = serde_json::from_str(&fixture).unwrap();
        // The current fixture has no Claude entries; check any future overlap
        // without inventing catalog values from our defaults.
        if let Some(models) = api["anthropic"]["models"].as_object() {
            for (id, model) in models {
                if let Some(limit) = model["limit"]["output"].as_u64()
                    && default_limit(id) != LEGACY_DEFAULT
                {
                    assert!(default_limit(id) <= limit, "{id}: default exceeds catalog");
                }
            }
        }
    }
}
