use config::BudgetConfig;
use runtime::budget_tracker::{BudgetSettings, ResourceLimit};

#[test]
fn maps_all_configured_budgets_without_changing_other_limits() {
    // Given: every exposed setting differs from its fallback.
    let config = BudgetConfig {
        max_tool_calls: 31,
        max_tokens: Some(123_456),
        max_elapsed_secs: 600,
        max_no_progress_rounds: 7,
        max_file_rereads: 9,
        max_identical_tool_call_repeats: 2,
    };
    // When: config values become runtime settings.
    let settings = BudgetSettings::from(&config);
    // Then: all configured limits reach runtime accounting.
    assert_eq!(
        settings,
        BudgetSettings {
            max_tool_calls: ResourceLimit::Bounded(31),
            max_no_progress_rounds: 7,
            max_file_rereads: 9,
            max_identical_tool_call_repeats: 2,
            max_tokens: ResourceLimit::Bounded(123_456),
            max_elapsed: ResourceLimit::Bounded(std::time::Duration::from_secs(600)),
        }
    );
}

#[test]
fn default_config_maps_to_the_runtime_defaults() {
    // Given/When: the untouched config schema defaults become runtime settings.
    let settings = BudgetSettings::from(&BudgetConfig::default());
    // Then: 400 calls, 2 hours and no token cap are preserved, none of them lifted.
    assert_eq!(settings, BudgetSettings::default());
    assert_eq!(settings.max_tool_calls, ResourceLimit::Bounded(400));
    assert_eq!(
        settings.max_elapsed,
        ResourceLimit::Bounded(std::time::Duration::from_secs(7_200))
    );
    assert_eq!(settings.max_tokens, ResourceLimit::Unlimited);
}

#[test]
fn config_values_are_never_mapped_to_unlimited_caps() {
    // Given: a config with the largest representable caps.
    let config = BudgetConfig {
        max_tool_calls: u32::MAX,
        max_elapsed_secs: u64::MAX,
        max_tokens: Some(u64::MAX),
        ..Default::default()
    };
    // When/Then: they stay explicit bounds; only the runtime lane API lifts caps.
    let settings = BudgetSettings::from(&config);
    assert_eq!(settings.max_tool_calls, ResourceLimit::Bounded(u32::MAX));
    assert_eq!(settings.max_tokens, ResourceLimit::Bounded(u64::MAX));
    assert!(matches!(settings.max_elapsed, ResourceLimit::Bounded(_)));
}
