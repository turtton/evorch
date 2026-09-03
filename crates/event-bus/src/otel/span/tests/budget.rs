use std::time::Duration;

use crate::event::LifecycleEvent;

use super::super::{SpanAction, SpanBudget, SpanDropKind, SpanKey, SpanMapper, SpanStatus};
use super::{
    action_attributes, event, request_completed, request_started, run_done, session_started,
    start_run, str_attr, tool_started,
};

#[test]
fn budget_defaults_match_the_tested_baseline() {
    // Given: no explicit budget configuration.
    // When: the default budget is built.
    // Then: every ADR 0012 hard limit matches the tested baseline exactly.
    assert_eq!(
        SpanBudget::default(),
        SpanBudget {
            max_in_flight_spans_per_run: 128,
            max_in_flight_spans_global: 4096,
            max_admitted_spans_per_window: 10_000,
            max_span_lifetime: Duration::from_secs(30 * 60),
            max_attributes_per_span: 32,
            max_attribute_bytes_per_span: 16 * 1024,
            max_attribute_value_bytes: 1024,
        }
    );
}

#[test]
fn zero_ratio_drops_the_whole_run_subtree() {
    // Given: a mapper sampling nothing (ratio 0.0).
    let mut mapper = SpanMapper::with_sampling_ratio(0.0);
    // When: a root run, its request and tool, and its terminal events flow in.
    assert!(mapper.ingest(&start_run("run-1", None, 1)).is_empty());
    assert!(
        mapper
            .ingest(&request_started("req-1", "run-1", 2))
            .is_empty()
    );
    assert!(
        mapper
            .ingest(&tool_started("call-1", "run-1", 3))
            .is_empty()
    );
    assert!(mapper.ingest(&run_done("run-1", 4)).is_empty());
    // Then: every run-scoped start is a SampledOut drop and the terminals are silent.
    let drops = mapper.drain_drops();
    assert_eq!(
        drops
            .iter()
            .map(|drop| (drop.kind, &drop.key))
            .collect::<Vec<_>>(),
        vec![
            (
                SpanDropKind::SampledOut,
                &SpanKey::Run {
                    run_id: "run-1".to_owned()
                }
            ),
            (
                SpanDropKind::SampledOut,
                &SpanKey::Agent {
                    run_id: "run-1".to_owned()
                }
            ),
            (
                SpanDropKind::SampledOut,
                &SpanKey::Request {
                    request_id: "req-1".to_owned()
                }
            ),
            (
                SpanDropKind::SampledOut,
                &SpanKey::Tool {
                    call_id: "call-1".to_owned()
                }
            ),
        ]
    );
}

#[test]
fn unit_ratio_admits_the_full_tree() {
    // Given: a mapper sampling everything (ratio 1.0).
    let mut mapper = SpanMapper::with_sampling_ratio(1.0);
    // When: the golden run tree flows in.
    let run = mapper.ingest(&start_run("run-1", None, 1));
    let request = mapper.ingest(&request_started("req-1", "run-1", 2));
    let tool = mapper.ingest(&tool_started("call-1", "run-1", 3));
    let done = mapper.ingest(&run_done("run-1", 4));
    // Then: every span action of the tree is emitted in order and nothing drops.
    assert_eq!(run.len(), 2);
    assert!(matches!(
        &run[0],
        SpanAction::Start {
            key: SpanKey::Run { .. },
            ..
        }
    ));
    assert!(matches!(
        &run[1],
        SpanAction::Start {
            key: SpanKey::Agent { .. },
            ..
        }
    ));
    assert!(matches!(
        &request[0],
        SpanAction::Start {
            key: SpanKey::Request { .. },
            ..
        }
    ));
    assert!(matches!(
        &tool[0],
        SpanAction::Start {
            key: SpanKey::Tool { .. },
            ..
        }
    ));
    assert_eq!(done.len(), 2);
    assert!(matches!(
        &done[0],
        SpanAction::End {
            status: SpanStatus::Unset,
            ..
        }
    ));
    assert!(mapper.drain_drops().is_empty());
}

#[test]
fn child_of_unadmitted_parent_drops_the_subtree_as_unknown_parent() {
    // Given: a mapper sampling nothing, so the parent never opens a span.
    let mut mapper = SpanMapper::with_sampling_ratio(0.0);
    assert!(mapper.ingest(&start_run("parent", None, 1)).is_empty());
    // When: a child run is registered under the never-open parent.
    let child = mapper.ingest(&start_run("child", Some("parent"), 2));
    // Then: the whole child subtree is refused with one UnknownParent drop —
    //       no child span starts and no child ledger entry is recorded.
    assert!(child.is_empty());
    let drops = mapper.drain_drops();
    assert_eq!(
        drops
            .iter()
            .map(|drop| (drop.kind, &drop.key))
            .collect::<Vec<_>>(),
        vec![
            (
                SpanDropKind::SampledOut,
                &SpanKey::Run {
                    run_id: "parent".to_owned()
                }
            ),
            (
                SpanDropKind::SampledOut,
                &SpanKey::Agent {
                    run_id: "parent".to_owned()
                }
            ),
            (
                SpanDropKind::UnknownParent,
                &SpanKey::Run {
                    run_id: "child".to_owned()
                }
            ),
        ]
    );
    assert!(!mapper.sampling_decisions.contains_key("child"));
    assert!(!mapper.agent_depth.contains_key("child"));
}

#[test]
fn tombstoned_rejected_start_silences_later_end() {
    // Given: a run whose request start was rejected by the global in-flight cap.
    let mut mapper = SpanMapper::with_budget(SpanBudget {
        max_in_flight_spans_global: 2,
        ..SpanBudget::default()
    });
    mapper.ingest(&start_run("run-1", None, 1));
    assert!(
        mapper
            .ingest(&request_started("req-1", "run-1", 2))
            .is_empty()
    );
    assert_eq!(mapper.drain_drops().len(), 1);
    // When: the terminal event for the rejected request arrives.
    let actions = mapper.ingest(&request_completed("req-1", 3));
    // Then: it is a silent no-op — no action, no typed drop, no warn path.
    assert!(actions.is_empty());
    assert!(mapper.drain_drops().is_empty());
}

#[test]
fn sampled_out_terminals_do_not_multiply_unknown_span_end_drops() {
    // Given: a sampled-out run.
    let mut mapper = SpanMapper::with_sampling_ratio(0.0);
    assert_eq!(mapper.ingest(&session_started("session-1", 0)).len(), 1);
    assert!(mapper.ingest(&start_run("run-1", None, 1)).is_empty());
    mapper.drain_drops();
    // When: the session around it fails after the run terminal is ingested.
    assert!(mapper.ingest(&run_done("run-1", 2)).is_empty());
    let actions = mapper.ingest(&event(
        LifecycleEvent::Failed {
            session_id: "session-1".to_owned(),
            reason: "boom".to_owned(),
        },
        3,
    ));
    // Then: the failed session End closes nothing extra and records no UnknownSpanEnd.
    assert_eq!(actions.len(), 1);
    assert!(matches!(
        &actions[0],
        SpanAction::End {
            key: SpanKey::Session { .. },
            ..
        }
    ));
    assert!(mapper.drain_drops().is_empty());
}

#[test]
fn sampling_ratio_outside_the_unit_interval_is_clamped() {
    // Given: ratios beyond the unit interval.
    // When: a run flows through a mapper with an over-range and an under-range ratio.
    let mut over = SpanMapper::with_sampling_ratio(1.5);
    let mut under = SpanMapper::with_sampling_ratio(-0.5);
    // Then: the over-range ratio admits like 1.0 and the under-range drops like 0.0.
    assert_eq!(over.ingest(&start_run("run-1", None, 1)).len(), 2);
    assert!(under.ingest(&start_run("run-2", None, 2)).is_empty());
}

#[test]
fn evicted_spans_close_with_span_budget_evicted_error_type() {
    // Given: a mapper with a 1s lifetime and one open session span.
    let mut mapper = SpanMapper::with_budget(SpanBudget {
        max_span_lifetime: Duration::from_secs(1),
        ..SpanBudget::default()
    });
    mapper.ingest(&session_started("session-1", 0));
    // When: an unrelated event is ingested 2s later.
    let actions = mapper.ingest(&session_started("session-2", 2));
    // Then: the expired span is evicted first with an Error End carrying the
    //       stable span_budget_evicted classification, and the new span follows.
    assert_eq!(actions.len(), 2);
    assert!(matches!(
        &actions[0],
        SpanAction::End { key: SpanKey::Session { session_id } , status: SpanStatus::Error, .. }
            if session_id == "session-1"
    ));
    assert_eq!(
        action_attributes(&actions[0]).last(),
        Some(&str_attr("error.type", "span_budget_evicted"))
    );
    assert!(matches!(
        &actions[1],
        SpanAction::Start { key: SpanKey::Session { session_id }, .. }
            if session_id == "session-2"
    ));
    let drops = mapper.drain_drops();
    assert_eq!(drops.len(), 1);
    assert_eq!(drops[0].kind, SpanDropKind::BudgetEvicted);
    // And: the evicted key is tombstoned — its later completion is silent.
    assert!(
        mapper
            .ingest(&event(
                LifecycleEvent::Completed {
                    session_id: "session-1".to_owned(),
                },
                3,
            ))
            .is_empty()
    );
    assert!(mapper.drain_drops().is_empty());
}

#[test]
fn eviction_audit_closes_expired_spans_oldest_first() {
    // Given: two open session spans with the older one opened first.
    let mut mapper = SpanMapper::with_budget(SpanBudget {
        max_span_lifetime: Duration::from_secs(1),
        ..SpanBudget::default()
    });
    mapper.ingest(&session_started("old", 0));
    mapper.ingest(&session_started("new", 1));
    // When: the audit runs at t=3, past both lifetimes.
    let actions = mapper.ingest(&session_started("fresh", 3));
    // Then: evictions close in start order (oldest first) before the new span.
    let keys: Vec<&str> = actions
        .iter()
        .filter_map(|action| match action {
            SpanAction::End {
                key: SpanKey::Session { session_id },
                ..
            } => Some(session_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(keys, vec!["old", "new"]);
}
