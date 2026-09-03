use super::super::{SpanBudget, SpanDrop, SpanDropKind, SpanKey, SpanMapper};
use super::{
    action_attributes, i64_attr, request_completed, request_started, request_started_custom,
    session_started, start_run, str_attr,
};

#[test]
fn second_request_start_drops_when_per_run_in_flight_is_exhausted() {
    // Given: a per-run operational span cap that admits one request.
    let mut mapper = SpanMapper::with_budget(SpanBudget {
        max_in_flight_spans_per_run: 1,
        ..SpanBudget::default()
    });
    mapper.ingest(&start_run("run-1", None, 1));
    assert_eq!(
        mapper.ingest(&request_started("req-1", "run-1", 2)).len(),
        1
    );
    // When: a second request of the same run starts.
    let second = mapper.ingest(&request_started("req-2", "run-1", 3));
    // Then: it is dropped as BudgetInFlightPerRun with the request key.
    assert!(second.is_empty());
    assert_eq!(
        mapper.drain_drops(),
        vec![SpanDrop {
            kind: SpanDropKind::BudgetInFlightPerRun,
            key: SpanKey::Request {
                request_id: "req-2".to_owned()
            },
        }]
    );
    // And: closing the first request frees the slot for a later start.
    mapper.ingest(&request_completed("req-1", 4));
    assert_eq!(
        mapper.ingest(&request_started("req-3", "run-1", 5)).len(),
        1
    );
}

#[test]
fn third_span_start_drops_when_global_in_flight_is_exhausted() {
    // Given: a global in-flight cap of 2, filled by one run (run + agent).
    let mut mapper = SpanMapper::with_budget(SpanBudget {
        max_in_flight_spans_global: 2,
        ..SpanBudget::default()
    });
    assert_eq!(mapper.ingest(&start_run("run-1", None, 1)).len(), 2);
    // When: a third span tries to open — first as a request, then as a new run.
    let request = mapper.ingest(&request_started("req-1", "run-1", 2));
    let other_run = mapper.ingest(&start_run("run-2", None, 3));
    // Then: both third starts are dropped as BudgetInFlightGlobal.
    assert!(request.is_empty());
    assert!(other_run.is_empty());
    let drops = mapper.drain_drops();
    assert_eq!(drops.len(), 2);
    assert!(
        drops
            .iter()
            .all(|drop| drop.kind == SpanDropKind::BudgetInFlightGlobal)
    );
    assert_eq!(
        drops[1].key,
        SpanKey::Run {
            run_id: "run-2".to_owned()
        }
    );
}

#[test]
fn fourth_admission_within_window_drops_and_resets_after_the_window() {
    // Given: a window admission cap of 3 per 60s.
    let mut mapper = SpanMapper::with_budget(SpanBudget {
        max_admitted_spans_per_window: 3,
        ..SpanBudget::default()
    });
    // When: three sessions open, then a fourth within the window.
    for index in 1..=3_u64 {
        assert_eq!(
            mapper
                .ingest(&session_started(&format!("session-{index}"), index))
                .len(),
            1
        );
    }
    assert!(mapper.ingest(&session_started("session-4", 4)).is_empty());
    assert!(mapper.ingest(&session_started("session-5", 5)).is_empty());
    // Then: the over-window starts are dropped as BudgetWindow.
    assert!(
        mapper
            .drain_drops()
            .iter()
            .all(|drop| drop.kind == SpanDropKind::BudgetWindow)
    );
    // And: 61s after the window opened, admission resumes.
    assert_eq!(mapper.ingest(&session_started("session-6", 62)).len(), 1);
}

#[test]
fn attribute_overflow_beyond_the_count_cap_is_dropped() {
    // Given: an attribute count cap of 3.
    let mut mapper = SpanMapper::with_budget(SpanBudget {
        max_attributes_per_span: 3,
        ..SpanBudget::default()
    });
    // When: a root run opens (run span 4 attrs, agent span 6 attrs).
    let actions = mapper.ingest(&start_run("run-1", None, 1));
    // Then: each span keeps its first 3 attributes and the overflow drops.
    assert_eq!(action_attributes(&actions[0]).len(), 3);
    assert_eq!(action_attributes(&actions[1]).len(), 3);
    let drops = mapper.drain_drops();
    assert_eq!(drops.len(), 4);
    assert!(
        drops
            .iter()
            .all(|drop| drop.kind == SpanDropKind::BudgetAttributes)
    );
    let run_drops = drops
        .iter()
        .filter(|drop| {
            drop.key
                == SpanKey::Run {
                    run_id: "run-1".to_owned(),
                }
        })
        .count();
    let agent_drops = drops
        .iter()
        .filter(|drop| {
            drop.key
                == SpanKey::Agent {
                    run_id: "run-1".to_owned(),
                }
        })
        .count();
    assert_eq!(run_drops, 1);
    assert_eq!(agent_drops, 3);
}

#[test]
fn attributes_exceeding_the_span_byte_cap_are_dropped() {
    // Given: a 64-byte per-span attribute budget; the first two request attrs
    //        occupy 25 + 29 bytes and the third would exceed it.
    let mut mapper = SpanMapper::with_budget(SpanBudget {
        max_attribute_bytes_per_span: 64,
        ..SpanBudget::default()
    });
    mapper.ingest(&start_run("run-1", None, 1));
    mapper.drain_drops();
    // When: a request span starts with 5 attributes.
    let actions = mapper.ingest(&request_started("req-1", "run-1", 2));
    // Then: only the first two attributes are kept and the rest drop in order.
    assert_eq!(
        action_attributes(&actions[0]),
        vec![
            str_attr("gen_ai.operation.name", "chat"),
            str_attr("gen_ai.provider.name", "anthropic"),
        ]
    );
    let drops = mapper.drain_drops();
    assert_eq!(drops.len(), 3);
    assert!(
        drops
            .iter()
            .all(|drop| drop.kind == SpanDropKind::BudgetAttributes
                && drop.key
                    == SpanKey::Request {
                        request_id: "req-1".to_owned()
                    })
    );
}

#[test]
fn oversized_attribute_values_are_dropped_without_truncation() {
    // Given: an 8-byte per-value cap and a request whose model id exceeds it.
    let mut mapper = SpanMapper::with_budget(SpanBudget {
        max_attribute_value_bytes: 8,
        ..SpanBudget::default()
    });
    mapper.ingest(&start_run("r", None, 1));
    mapper.drain_drops();
    // When: a request span starts with model "gpt-test-long" (13 bytes).
    let actions = mapper.ingest(&request_started_custom(
        "req-1",
        "r",
        "openai",
        "gpt-test-long",
        2,
    ));
    // Then: the model attribute is dropped whole — never truncated — while the
    //       other attributes are kept.
    let attrs = action_attributes(&actions[0]);
    assert!(!attrs.iter().any(|attr| attr.key == "gen_ai.request.model"));
    assert_eq!(attrs.len(), 4);
    assert_eq!(mapper.drain_drops().len(), 1);
    // And: the kept set is stable through End (state mirrors the emitted attrs).
    let end = mapper.ingest(&request_completed("req-1", 3));
    let final_attrs = action_attributes(&end[0]);
    assert!(
        !final_attrs
            .iter()
            .any(|attr| attr.key == "gen_ai.request.model")
    );
    assert_eq!(final_attrs.len(), 7);
    assert!(final_attrs.contains(&i64_attr("gen_ai.usage.input_tokens", 1)));
    assert!(final_attrs.contains(&str_attr("evorch.request.id", "req-1")));
}
