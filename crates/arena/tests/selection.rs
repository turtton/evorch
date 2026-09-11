use arena::{Attribution, EvalTrace, FailureAttribution};
use proptest::prelude::*;

fn trace(id: usize, tokens: u64, latency: u64, passes: bool) -> EvalTrace {
    EvalTrace {
        id: format!("run/{id}"),
        arena_id: "run".into(),
        project: "p".into(),
        task_id: "t".into(),
        task_spec: "spec".into(),
        config_id: id.to_string(),
        profile: "local".into(),
        model: "mock".into(),
        attribution: Attribution::Worker,
        execution: None,
        output: String::new(),
        input_tokens: tokens,
        output_tokens: 0,
        elapsed_ms: latency,
        failure: if passes {
            None
        } else {
            Some(FailureAttribution::OutputMismatch)
        },
    }
}

proptest! {
    #[test]
    fn selection_is_permutation_invariant_and_gate_safe(
        metrics in prop::collection::vec((0u64..10000, 0u64..10000, any::<bool>()), 0..30)
    ) {
        // Given: arbitrary successful and failed configurations.
        let mut traces: Vec<_> = metrics.into_iter().enumerate().map(|(i,(t,l,p))| trace(i,t,l,p)).collect();
        // When: the same evidence is ranked in opposite input orders.
        let first = arena::select(&traces);
        traces.reverse();
        let second = arena::select(&traces);
        // Then: ordering is stable and every survivor is successful and non-dominated.
        prop_assert_eq!(&first, &second);
        for id in first {
            let winner = traces.iter().find(|t| t.config_id == id).expect("winner");
            prop_assert!(winner.failure.is_none());
            prop_assert!(!traces.iter().filter(|t| t.failure.is_none()).any(|t| arena::dominates(t, winner)));
        }
    }

    #[test]
    fn pairwise_order_is_antisymmetric_and_transitive(a in any::<u64>(), b in any::<u64>(), c in any::<u64>()) {
        // Given: metrics including values that would overflow a u64 sum.
        let mut rows = [trace(0,a,b,true), trace(1,b,c,true), trace(2,c,a,true)];
        rows[0].output_tokens = c;
        // When: pairwise comparisons order them.
        rows.sort_by(arena::pairwise_tiebreak);
        // Then: the comparator is total and transitive.
        prop_assert!(arena::pairwise_tiebreak(&rows[0], &rows[2]).is_le());
        prop_assert_eq!(arena::pairwise_tiebreak(&rows[0], &rows[1]), arena::pairwise_tiebreak(&rows[1], &rows[0]).reverse());
    }
}

#[test]
fn gate_precedes_pareto_and_tiebreak() {
    // Given: a cheap failure, dominated pass, and two Pareto survivors.
    let rows = [
        trace(0, 0, 0, false),
        trace(1, 10, 10, true),
        trace(2, 5, 15, true),
        trace(3, 20, 20, true),
    ];
    // When / Then: token count breaks the Pareto tradeoff; failure never wins.
    assert_eq!(arena::select(&rows), vec!["2", "1"]);
}
