//! Registration-only Q2 leaf target, not a durable authority reconstruction path.
#[test]
fn per_thread_progress_has_four_distinct_current_format_states() {
    use storage::recovery::graph_progress::GraphDisposalStep::*;
    let values =
        [NotStarted, Applied, NotApplicable, Unknown].map(|s| serde_json::to_string(&s).unwrap());
    assert_eq!(
        values
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        4
    );
}
