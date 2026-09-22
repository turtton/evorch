use config::BudgetConfig;
use runtime::budget_tracker::BudgetSettings;

#[test]
fn maps_all_configured_budgets_without_changing_other_limits() {
    // Given: every exposed setting differs from its fallback.
    let config = BudgetConfig {
        max_tool_calls: 31,
        max_tokens: 123_456,
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
            max_tool_calls: 31,
            max_no_progress_rounds: 7,
            max_file_rereads: 9,
            max_identical_tool_call_repeats: 2,
            max_tokens: 123_456,
            max_elapsed: std::time::Duration::from_secs(600),
        }
    );
}
