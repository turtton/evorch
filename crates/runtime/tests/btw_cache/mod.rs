//! A `/btw` side question rides the run's cache affinity without touching the
//! run: the main turns stay append-only, the side request is the frozen prefix plus
//! one question, and the question never reaches a later main request.
use super::*;

const QUESTION: &str = "side question: which file did you read?";
const ANSWER: &str = "side answer: none yet";

fn wire_requests(harness: &Harness) -> Vec<Value> {
    harness
        .mock
        .recorded_requests()
        .into_iter()
        .filter(|request| request.path == harness.wire.path())
        .map(|request| request.body)
        .filter(|body| !is_compaction_trigger_request(body))
        .collect()
}

fn items(request: &Value) -> Vec<Value> {
    request
        .get("messages")
        .or_else(|| request.get("input"))
        .and_then(Value::as_array)
        .cloned()
        .expect("conversation items")
}

fn started(events: &[Event]) -> Vec<(String, Option<RequestPurpose>)> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Provider(ProviderEvent::RequestStarted {
                request_id,
                purpose,
                ..
            }) => Some((request_id.clone(), *purpose)),
            _ => None,
        })
        .collect()
}

/// Main A, B, then a side question, then main C and D.
async fn scenario(wire: Wire) -> (Harness, RunId, Vec<Event>, storage::Storage) {
    let mut harness = harness_with_wire(
        wire,
        vec![
            text_response("reply A"),
            text_response("reply B"),
            text_response(ANSWER),
            text_response("reply C"),
            text_response("reply D"),
        ],
        1_000_000,
        Arc::new(BulkRead),
        Vec::new(),
        None,
        0.5,
        |_| {},
    );
    let config = storage::StorageConfig {
        db_path: harness._directory.path().join("history.db"),
        ..Default::default()
    };
    let db = storage::Storage::open(config.clone()).unwrap();
    harness.runtime = harness
        .runtime
        .with_run_store(runtime::RunStore::open(&config, db.handle()).unwrap());
    let run = harness
        .runtime
        .delegate_chat(
            "btw",
            Role::Worker,
            "turn A".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
        .unwrap();
    let mut events = through_phase(&mut harness.receiver, run, AgentRunPhase::Waiting).await;
    harness.runtime.send_message(run, "turn B".into()).unwrap();
    events.extend(through_phase(&mut harness.receiver, run, AgentRunPhase::Waiting).await);

    let snapshot = harness.runtime.btw_snapshot(run).unwrap();
    assert!(snapshot.source().settled);
    let answer = snapshot.ask(QUESTION).await.unwrap();
    assert_eq!(answer.text, ANSWER);

    harness.runtime.send_message(run, "turn C".into()).unwrap();
    events.extend(through_phase(&mut harness.receiver, run, AgentRunPhase::Waiting).await);
    harness.runtime.send_message(run, "turn D".into()).unwrap();
    events.extend(through_phase(&mut harness.receiver, run, AgentRunPhase::Waiting).await);
    harness.runtime.cancel(run).unwrap();
    harness.runtime.wait(run).await.unwrap();
    (harness, run, events, db)
}

#[tokio::test]
async fn side_question_leaves_the_main_wire_prefix_and_cache_baseline_alone() {
    for wire in [Wire::ChatCompletions, Wire::Responses, Wire::Codex] {
        let (harness, run, events, _db) = scenario(wire).await;
        let requests = wire_requests(&harness);
        assert_eq!(requests.len(), 5, "A, B, side, C, D");
        assert_eq!(harness.mock.remaining_scripts(), 0);
        let (a, b, side, c, d) = (
            &requests[0],
            &requests[1],
            &requests[2],
            &requests[3],
            &requests[4],
        );

        // Main turns only append to each other, with the side question skipped.
        for (previous, next) in [(a, b), (b, c), (c, d)] {
            assert_append_only(wire.protocol(), previous, next).unwrap();
        }

        // The side request is B's input, B's reply and exactly one new question.
        let (main_b, side_items) = (items(b), items(side));
        assert_eq!(side_items[..main_b.len()], main_b[..]);
        assert_eq!(side_items.len(), main_b.len() + 2);
        let tail = side_items[main_b.len()].to_string();
        assert!(tail.contains("reply B"), "{tail}");
        assert!(side_items.last().unwrap().to_string().contains(QUESTION));

        // Neither the question nor its answer ever reaches a main request.
        for request in [a, b, c, d] {
            let text = request.to_string();
            assert!(!text.contains(QUESTION), "{text}");
            assert!(!text.contains(ANSWER), "{text}");
        }

        // Instructions, tools, model and cache affinity are the main run's; only
        // the side request forbids tool calls.
        for key in ["model", "tools", "instructions", "prompt_cache_key"] {
            assert_eq!(side.get(key), b.get(key), "{key}");
            assert_eq!(c.get(key), b.get(key), "{key}");
        }
        let choice = |request: &Value| request.get("tool_choice").cloned();
        assert_eq!(choice(side), Some(json!("none")));
        for request in [a, b, c, d] {
            assert_ne!(choice(request), Some(json!("none")));
        }

        // Purpose and the cache baseline: C is compared with B, never with the
        // side request, and the side request has no main baseline to regress against.
        let ids = started(&events);
        assert_eq!(
            ids.iter().map(|(_, purpose)| *purpose).collect::<Vec<_>>(),
            [
                Some(RequestPurpose::Agent),
                Some(RequestPurpose::Agent),
                Some(RequestPurpose::Btw),
                Some(RequestPurpose::Agent),
                Some(RequestPurpose::Agent),
            ]
        );
        let comparison = |index: usize| {
            events.iter().find_map(|event| match &event.kind {
                EventKind::Provider(ProviderEvent::CacheReuseObserved {
                    request_id,
                    comparison,
                    ..
                }) if request_id == &ids[index].0 => Some(comparison.clone()),
                _ => None,
            })
        };
        match comparison(3) {
            Some(event_bus::CacheComparison::Compared {
                previous_request_id,
                ..
            }) => assert_eq!(previous_request_id, ids[1].0),
            other => panic!("C must be compared with B: {other:?}"),
        }
        assert!(!matches!(
            comparison(2),
            Some(event_bus::CacheComparison::Compared { .. })
        ));
        assert!(events.iter().all(|event| !matches!(&event.kind,
            EventKind::Diagnostic(diagnostic) if diagnostic.code == "CacheRegression")));

        // The mock cache is derived from the input it received: C reuses B's whole
        // input although the side request came in between.
        let usage: Vec<(u64, u64)> = events
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::Provider(ProviderEvent::RequestCompleted {
                    input_tokens,
                    cache_read_tokens,
                    ..
                }) => Some((*input_tokens, *cache_read_tokens)),
                _ => None,
            })
            .collect();
        assert!(usage[3].1 >= usage[1].0, "{usage:?}");

        // Nothing of the side question is stored with the run or announced on the bus.
        let saved = harness
            .runtime
            .run_context_view(run)
            .unwrap()
            .expect("saved context");
        let saved = format!("{:?}", saved.messages);
        assert!(saved.contains("turn D"), "the run's own context is saved");
        for leaked in [QUESTION, ANSWER] {
            assert!(!saved.contains(leaked), "{leaked} saved with the run");
            assert!(
                !format!("{events:?}").contains(leaked),
                "{leaked} on the bus"
            );
        }
    }
}

fn chat_config() -> RunConfig {
    RunConfig {
        interactive: true,
        keep_alive: true,
        ..Default::default()
    }
}

fn attach_store(harness: &mut Harness) -> storage::Storage {
    let config = storage::StorageConfig {
        db_path: harness._directory.path().join("history.db"),
        ..Default::default()
    };
    let db = storage::Storage::open(config.clone()).unwrap();
    harness.runtime = harness
        .runtime
        .clone()
        .with_run_store(runtime::RunStore::open(&config, db.handle()).unwrap());
    db
}

struct ReleaseOnDrop(mock_openai::ResponseGate);
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}

#[tokio::test]
async fn side_question_and_a_running_main_request_never_wait_for_each_other() {
    for wire in [Wire::ChatCompletions, Wire::Codex] {
        // The main request is held at the provider while the side question is asked
        // and answered; the side answer is held while the next main turn completes.
        let (main_gate, main_arrived) = mock_openai::ResponseGate::new();
        let (side_gate, side_arrived) = mock_openai::ResponseGate::new();
        let _release_main = ReleaseOnDrop(main_gate.clone());
        let _release_side = ReleaseOnDrop(side_gate.clone());
        let mut harness = harness_with_wire(
            wire,
            vec![
                text_response("reply A").with_gate(main_gate.clone()),
                text_response("early side answer"),
                text_response("late side answer").with_gate(side_gate.clone()),
                text_response("reply B"),
            ],
            1_000_000,
            Arc::new(BulkRead),
            Vec::new(),
            None,
            0.5,
            |_| {},
        );
        let _db = attach_store(&mut harness);
        let run = harness
            .runtime
            .delegate_chat("btw", Role::Worker, "turn A".into(), chat_config())
            .unwrap();
        tokio::task::spawn_blocking(move || main_arrived.recv().unwrap())
            .await
            .unwrap();

        // First request still in flight: the snapshot is the request just sent.
        let early = harness.runtime.btw_snapshot(run).unwrap();
        assert!(!early.source().settled);
        let answer = early.ask("zq-early").await.unwrap();
        assert_eq!(answer.text, "early side answer");
        main_gate.release();
        let mut events = through_phase(&mut harness.receiver, run, AgentRunPhase::Waiting).await;

        // The settled snapshot now includes the final answer of turn A.
        let settled = harness.runtime.btw_snapshot(run).unwrap();
        assert!(settled.source().settled);
        let side = tokio::spawn(async move { settled.ask("zq-late").await });
        tokio::task::spawn_blocking(move || side_arrived.recv().unwrap())
            .await
            .unwrap();
        harness.runtime.send_message(run, "turn B".into()).unwrap();
        events.extend(through_phase(&mut harness.receiver, run, AgentRunPhase::Waiting).await);
        side_gate.release();
        assert_eq!(side.await.unwrap().unwrap().text, "late side answer");
        harness.runtime.cancel(run).unwrap();
        harness.runtime.wait(run).await.unwrap();

        let requests = wire_requests(&harness);
        assert_eq!(requests.len(), 4);
        let main: Vec<_> = [0, 3].into_iter().map(|index| &requests[index]).collect();
        assert_append_only(wire.protocol(), main[0], main[1]).unwrap();
        for request in &main {
            let text = request.to_string();
            for leaked in ["zq-", "side answer"] {
                assert!(!text.contains(leaked), "{leaked} leaked: {text}");
            }
        }
        // The early question saw only turn A; the late one also saw its reply.
        assert!(!requests[1].to_string().contains("reply A"));
        assert!(requests[2].to_string().contains("reply A"));
        assert_eq!(
            started(&events)
                .iter()
                .filter(|(_, purpose)| *purpose == Some(RequestPurpose::Btw))
                .count(),
            2,
            "both side questions are marked as such"
        );
    }
}
