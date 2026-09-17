use super::*;

#[test]
fn successful_change_resets_consecutive_no_progress_rounds() {
    // Given: two rounds without a file change.
    let mut counters = BudgetCounters::default();
    counters.finish_round();
    counters.finish_round();
    assert_eq!(counters.no_progress_rounds, 2);
    // When: a round records a successful file change.
    counters.file_changed();
    counters.finish_round();
    // Then: the consecutive counter resets.
    assert_eq!(counters.no_progress_rounds, 0);
}

#[test]
fn usage_accumulation_saturates_without_wrapping() {
    // Given: counters close to their storage limits.
    let mut counters = BudgetCounters::default();
    counters.usage(Usage {
        input_tokens: u64::MAX,
        output_tokens: u64::MAX,
        ..Default::default()
    });
    // When: more usage arrives.
    counters.usage(Usage {
        input_tokens: 1,
        output_tokens: 2,
        ..Default::default()
    });
    // Then: both counters saturate instead of resetting the budget.
    assert_eq!(counters.cumulative_input_tokens, u64::MAX);
    assert_eq!(counters.cumulative_output_tokens, u64::MAX);
}
