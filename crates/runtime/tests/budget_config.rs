use config::BudgetConfig;
use runtime::budget_tracker::BudgetSettings;

#[test]
fn maps_all_configured_budgets_without_changing_other_limits() {
    // Given: every exposed setting differs from its fallback.
    let config = BudgetConfig {
        max_tool_calls: 31,
        max_no_progress_rounds: 7,
        max_file_rereads: 9,
        max_identical_tool_call_repeats: 2,
    };
    // When: config values become runtime settings.
    let settings = BudgetSettings::from(&config);
    // Then: all four values map and unexposed limits remain unchanged.
    assert_eq!(
        settings,
        BudgetSettings {
            max_tool_calls: 31,
            max_no_progress_rounds: 7,
            max_file_rereads: 9,
            max_identical_tool_call_repeats: 2,
            ..Default::default()
        }
    );
}
