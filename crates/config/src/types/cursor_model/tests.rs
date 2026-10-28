use super::*;

#[test]
fn split_cursor_model_id_strips_every_suffix_kind() {
    // (id, base, effort, fast, thinking)
    let cases = [
        (
            "claude-opus-5-5-high",
            "claude-opus-5-5",
            Some("high"),
            false,
            false,
        ),
        ("claude-opus-5-5", "claude-opus-5-5", None, false, false),
        ("gpt-5-high-fast", "gpt-5", Some("high"), true, false),
        ("gpt-5-xhigh", "gpt-5", Some("xhigh"), false, false),
        ("gpt-5-none", "gpt-5", Some("none"), false, false),
        ("gpt-5-max", "gpt-5", Some("max"), false, false),
        (
            "claude-opus-5-5-thinking",
            "claude-opus-5-5",
            None,
            false,
            true,
        ),
        (
            "claude-opus-5-5-thinking-high",
            "claude-opus-5-5",
            Some("high"),
            false,
            true,
        ),
        (
            "claude-opus-5-5-high-thinking",
            "claude-opus-5-5",
            Some("high"),
            false,
            true,
        ),
        ("composer-2.5-fast", "composer-2.5", None, true, false),
        ("composer-2.5", "composer-2.5", None, false, false),
        (
            "gemini-3.1-pro-low",
            "gemini-3.1-pro",
            Some("low"),
            false,
            false,
        ),
        ("-fast", "-fast", None, false, false),
        ("-high", "-high", None, false, false),
        ("high", "high", None, false, false),
    ];
    for (id, base, effort, fast, thinking) in cases {
        assert_eq!(
            split_cursor_model_id(id),
            CursorModelParts {
                base,
                effort,
                fast,
                thinking
            },
            "{id}"
        );
    }
}

#[test]
fn lookup_ids_run_from_specific_to_base_and_keep_the_original() {
    assert_eq!(
        cursor_lookup_ids("claude-opus-5-5-thinking-high-fast"),
        [
            "claude-opus-5-5-thinking-high-fast",
            "claude-opus-5-5-thinking-high",
            "claude-opus-5-5-thinking",
            "claude-opus-5-5"
        ]
    );
    assert_eq!(cursor_lookup_ids("o3"), ["o3"]);
}

#[test]
fn api_providers_follow_the_model_family() {
    for (base, expected) in [
        ("claude-opus-5-5", &["anthropic"][..]),
        ("gpt-5", &["openai"]),
        ("o3", &["openai"]),
        ("o4-mini", &["openai"]),
        ("gemini-3.1-pro", &["google"]),
        ("grok-4", &["xai"]),
        ("composer-2.5", &[]),
        ("omega", &[]),
    ] {
        assert_eq!(cursor_api_providers(base), expected, "{base}");
    }
}

#[test]
fn docs_prices_cover_cursor_models_and_the_documented_fast_lane() {
    let input = |id: &str, tokens: u64| {
        cursor_docs_pricing(id).map(|schedule| {
            let rates = schedule.rates_for(tokens, crate::types::provider::ModelSpeed::Standard);
            (rates.input, rates.cache_read, rates.output)
        })
    };
    // Standard lane, then the Fast lane named by the `-fast` ID.
    assert_eq!(
        input("composer-2.5", 1),
        Some((Some(0.5), Some(0.2), Some(2.5)))
    );
    assert_eq!(
        input("composer-2.5-fast", 1),
        Some((Some(3.0), Some(0.5), Some(15.0)))
    );
    // An effort suffix does not change the lane, and Grok 4.7 tiers by input size.
    assert_eq!(
        input("grok-4.7-high", 256_000),
        Some((Some(2.0), Some(0.5), Some(6.0)))
    );
    assert_eq!(
        input("grok-4.7", 256_001),
        Some((Some(4.0), Some(1.0), Some(12.0)))
    );
    assert_eq!(
        input("grok-4.7-fast", 256_001),
        Some((Some(6.0), Some(1.5), Some(18.0)))
    );
    assert_eq!(
        input("grok-4.5-fast", 1),
        Some((Some(4.0), Some(1.0), Some(18.0)))
    );
    // Third-party models are priced by models.dev, and unknown models have no row.
    assert_eq!(input("claude-opus-5-5", 1), None);
    assert_eq!(input("unknown-model", 1), None);
}

#[test]
fn docs_context_windows_ignore_suffixes() {
    assert_eq!(
        cursor_docs_context_window("composer-2.5-fast"),
        Some(200_000)
    );
    assert_eq!(
        cursor_docs_context_window("claude-opus-5-5-high"),
        Some(300_000)
    );
    assert_eq!(
        cursor_docs_context_window("gpt-5.6-sol-xhigh"),
        Some(272_000)
    );
    assert_eq!(cursor_docs_context_window("unknown-model"), None);
}

#[test]
fn id_aliases_cover_version_separator_and_preview_spellings() {
    // (id, aliases in order)
    let cases: [(&str, &[&str]); 8] = [
        ("claude-opus-5-5", &["claude-opus-5.5"]),
        ("gpt-5.6-sol", &["gpt-5-6-sol"]),
        (
            "gemini-3.1-pro",
            &[
                "gemini-3-1-pro",
                "gemini-3.1-pro-preview",
                "gemini-3-1-pro-preview",
            ],
        ),
        ("gemini-3.1-pro-preview", &["gemini-3-1-pro-preview"]),
        ("claude-opus-5", &[]),
        ("composer-2.5", &["composer-2-5"]),
        ("o3-mini", &[]),
        ("claude-opus-4-5-20251101", &["claude-opus-4.5-20251101"]),
    ];
    for (id, expected) in cases {
        assert_eq!(cursor_id_aliases(id), expected, "{id}");
    }
}

#[test]
fn docs_rows_match_alternative_spellings() {
    assert_eq!(
        cursor_docs_context_window("claude-opus-5.5-high"),
        Some(300_000)
    );
    assert_eq!(cursor_docs_context_window("gpt-5-6-sol"), Some(272_000));
    assert!(cursor_docs_pricing("composer-2-5-fast").is_some());
}
