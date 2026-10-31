//! Codex may omit `usage` from a terminal response. The omission must not disturb the wire
//! prefix, the input-derived mock cache, or the next request's baseline, and a missing report
//! must never be counted as a measured zero. Verified here with the real Codex client over
//! HTTP/SSE. Unlike `verify_trace`, a request may legitimately report no tokens, so the
//! non-zero checks apply only to requests whose usage was reported.
use event_bus::{
    AgentRunPhase, CacheComparison, CompactionEvent, CompactionReason, Event, EventKind,
    ProviderEvent, RequestPurpose, UsageEvent,
};
use mock_openai::ScriptedResponse;
use mock_openai::cache_contract::{
    CacheProtocol, assert_append_only, assert_compaction_transition,
};
use runtime::{Role, RunConfig};
use serde_json::Value;

use super::{
    BulkRead, Harness, MODEL, Wire, harness_with_wire, read_response, text_response, through_phase,
};

const BLOB: &str = "opaque-state-written-without-usage";

struct Completed {
    request_id: String,
    purpose: Option<RequestPurpose>,
    reported: bool,
    input: u64,
    cached: u64,
}

fn completed(events: &[Event]) -> Vec<Completed> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Provider(ProviderEvent::RequestCompleted {
                request_id,
                purpose,
                usage_reported,
                input_tokens,
                cache_read_tokens,
                ..
            }) => Some(Completed {
                request_id: request_id.clone(),
                purpose: *purpose,
                reported: *usage_reported,
                input: *input_tokens,
                cached: *cache_read_tokens,
            }),
            _ => None,
        })
        .collect()
}

fn count(events: &[Event], matches: impl Fn(&EventKind) -> bool) -> usize {
    events.iter().filter(|event| matches(&event.kind)).count()
}

fn missing_usage_diagnostics(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Diagnostic(diagnostic) if diagnostic.code == "ProviderUsageMissing" => {
                Some(diagnostic.call_id.clone().unwrap_or_default())
            }
            _ => None,
        })
        .collect()
}

fn assert_no_cache_regression(events: &[Event]) {
    assert_eq!(
        count(events, |kind| matches!(kind,
            EventKind::Diagnostic(diagnostic) if diagnostic.code == "CacheRegression")),
        0
    );
}

fn codex_requests(harness: &Harness) -> Vec<Value> {
    harness
        .mock
        .recorded_requests()
        .into_iter()
        .filter(|request| request.method == "POST" && request.path == harness.wire.path())
        .map(|request| request.body)
        .collect()
}

/// Four-request tool conversation; the response at `omit` (if any) carries no usage.
async fn tool_turns(omit: Option<usize>) -> (Vec<Event>, Vec<Value>) {
    let script = vec![
        read_response(0),
        read_response(1),
        read_response(2),
        text_response("done"),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, response)| {
        if Some(index) == omit {
            response.without_usage()
        } else {
            response
        }
    })
    .collect();
    let mut harness = harness_with_wire(
        Wire::Codex,
        script,
        1_000_000,
        std::sync::Arc::new(BulkRead),
        Vec::new(),
        None,
        0.5,
        |_| {},
    );
    let run = harness.runtime.delegate_background(
        Role::Worker,
        "Read three results".into(),
        RunConfig::default(),
    );
    let events = through_phase(&mut harness.receiver, run, AgentRunPhase::Done).await;
    assert_eq!(
        harness.runtime.wait(run).await.unwrap(),
        AgentRunPhase::Done
    );
    assert_eq!(harness.mock.remaining_scripts(), 0);
    (events, codex_requests(&harness))
}

// Given: a four-request Codex tool conversation where the second response omits usage
// When: it runs next to an otherwise identical conversation that reports every usage
// Then: every request is an append-only extension of the previous one; every reported request
// has the same input-derived tokens and cache reads as its control (equal sizes, so the omission
// changed nothing on the wire); the unreported one reports no tokens,
// no UsageEvent and one diagnostic; and the cache baseline resumes at the next reported request
#[tokio::test]
async fn usage_reported_then_missing_then_reported_keeps_the_wire_prefix_and_cache() {
    let (control_events, control_requests) = tool_turns(None).await;
    let (events, requests) = tool_turns(Some(1)).await;

    assert_eq!(requests.len(), control_requests.len());
    for pair in requests.windows(2) {
        assert_append_only(CacheProtocol::Codex, &pair[0], &pair[1]).unwrap();
    }
    let control = completed(&control_events);
    let observed = completed(&events);
    assert_eq!(observed.len(), 4);
    assert_eq!(
        observed.iter().map(|c| c.reported).collect::<Vec<_>>(),
        [true, false, true, true]
    );
    assert!(control.iter().all(|c| c.reported && c.input > 0));
    for (index, (got, expected)) in observed.iter().zip(&control).enumerate() {
        if got.reported {
            assert!(got.input > 0 && got.cached <= got.input, "request {index}");
            assert_eq!(
                (got.input, got.cached),
                (expected.input, expected.cached),
                "request {index}: mock cache must stay input-derived"
            );
        } else {
            assert_eq!((got.input, got.cached), (0, 0), "request {index}");
        }
    }
    // The request after the gap still reuses everything the earlier requests cached.
    assert!(observed[2].cached >= control[1].input);
    assert!(observed[3].cached >= observed[2].input);

    let usage_events = |events: &[Event]| {
        count(events, |kind| {
            matches!(kind, EventKind::Usage(UsageEvent::Usage { .. }))
        })
    };
    assert_eq!(usage_events(&control_events), 4);
    assert_eq!(usage_events(&events), 3, "no UsageEvent for the gap");
    assert_eq!(
        missing_usage_diagnostics(&events),
        [observed[1].request_id.clone()]
    );
    assert!(missing_usage_diagnostics(&control_events).is_empty());
    // The unreported request makes no cache observation and does not become the baseline.
    let reuse = |events: &[Event]| -> Vec<(String, CacheComparison)> {
        events
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::Provider(ProviderEvent::CacheReuseObserved {
                    request_id,
                    comparison,
                    ..
                }) => Some((request_id.clone(), comparison.clone())),
                _ => None,
            })
            .collect()
    };
    let reused = reuse(&events);
    assert_eq!(reused.len(), 3);
    assert!(reused.iter().all(|(id, _)| *id != observed[1].request_id));
    assert_eq!(
        reused[2].1,
        CacheComparison::Compared {
            previous_request_id: observed[2].request_id.clone(),
            previous_cache_tokens: observed[2].cached,
        }
    );
    assert_no_cache_regression(&events);
}

// Given: Codex official compaction whose terminal response omits usage
// When: a manual compaction succeeds and the conversation continues
// Then: the history is replaced exactly once with the returned state, the compaction request
// records no tokens and one diagnostic, and the new prefix is append-only and cached from then on
#[tokio::test]
async fn compaction_without_usage_still_replaces_history_then_reuses_the_new_prefix() {
    let review_storage = crate::support::ReviewStorage::new();
    let script = vec![
        read_response(0),
        text_response(&"old analysis that can be summarized ".repeat(100)),
        ScriptedResponse::compaction("compaction", MODEL, BLOB).without_usage(),
        read_response(1),
        text_response("done"),
    ];
    let mut harness = harness_with_wire(
        Wire::Codex,
        script,
        1_000_000,
        std::sync::Arc::new(BulkRead),
        Vec::new(),
        None,
        0.5,
        |_| {},
    );
    let run = harness.runtime.delegate_background(
        Role::Worker,
        "Preserve this original goal".into(),
        RunConfig {
            interactive: true,
            ..Default::default()
        },
    );
    let mut events = through_phase(&mut harness.receiver, run, AgentRunPhase::Waiting).await;
    harness.runtime.compact(run).unwrap();
    review_storage.attach(&harness.runtime);
    harness
        .runtime
        .send_message(run, "Continue after this boundary".into())
        .unwrap();
    events.extend(through_phase(&mut harness.receiver, run, AgentRunPhase::Done).await);
    assert_eq!(
        harness.runtime.wait(run).await.unwrap(),
        AgentRunPhase::Done
    );
    assert_eq!(harness.mock.remaining_scripts(), 0);

    let checkpoints: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Compaction(CompactionEvent::Compacted {
                reason,
                checkpoint_id,
                ..
            }) => Some((*reason, checkpoint_id.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(checkpoints.len(), 1);
    assert_eq!(checkpoints[0].0, CompactionReason::Manual);

    // Requests: 0 read, 1 old reply, 2 compaction trigger, 3 first after, 4 last.
    let requests = codex_requests(&harness);
    assert_eq!(requests.len(), 5);
    let trigger = requests[2]["input"].as_array().unwrap().last().unwrap();
    assert_eq!(trigger["type"], "compaction_trigger");
    assert_append_only(CacheProtocol::Codex, &requests[0], &requests[1]).unwrap();
    assert_compaction_transition(
        CacheProtocol::Codex,
        &requests[1],
        &requests[3],
        &checkpoints[0].1,
    )
    .unwrap();
    assert!(
        requests[3]["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "compaction" && item["encrypted_content"] == BLOB)
    );
    assert_append_only(CacheProtocol::Codex, &requests[3], &requests[4]).unwrap();

    let all = completed(&events);
    assert_eq!(all.len(), 5);
    let compaction = &all[2];
    assert_eq!(compaction.purpose, Some(RequestPurpose::Compaction));
    assert!(!compaction.reported);
    assert_eq!((compaction.input, compaction.cached), (0, 0));
    assert_eq!(
        missing_usage_diagnostics(&events),
        vec![compaction.request_id.clone()]
    );
    let generation: Vec<_> = all.iter().filter(|c| c.reported).collect();
    assert_eq!(generation.len(), 4);
    assert!(
        generation
            .iter()
            .all(|c| c.input > 0 && c.cached <= c.input)
    );
    assert_eq!(generation[0].cached, 0, "first request must be cold");
    assert!(generation[1].cached >= generation[0].input);
    assert!(
        generation[2].cached < generation[1].input,
        "compaction must really invalidate part of the old prefix"
    );
    assert!(
        generation[3].cached >= generation[2].input,
        "the new prefix is reused after a compaction that reported no usage"
    );
    assert_eq!(
        count(&events, |kind| matches!(
            kind,
            EventKind::Usage(UsageEvent::Usage { .. })
        )),
        4,
        "no UsageEvent for the compaction request"
    );
    assert_no_cache_regression(&events);
}
