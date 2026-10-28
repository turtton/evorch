//! A child call is recorded and closed exactly once, wherever the `run_js` call that
//! made it is cancelled (timeout, the program ending early, the run being stopped):
//! budgets, escalation and repeat detection read the record, and every started call
//! owes exactly one terminal event.

use std::task::Poll;

use event_bus::{Event, EventBus, EventKind, ToolEvent};
use sandbox::DirectSandbox;
use serde_json::json;

use super::*;

fn host() -> (RuntimeRunJsHost, event_bus::EventReceiver) {
    let bus = Arc::new(EventBus::new(256));
    let receiver = bus.subscribe();
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        bus,
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    let host = RuntimeRunJsHost {
        executor,
        base: ToolExecutionContext {
            run_id: "run-test".into(),
            ..Default::default()
        },
        policy: ExecutionPolicy::for_role(crate::Role::Worker),
        settings: config::RunJsConfig::default(),
        tools: vec!["read".into(), "shell".into()],
        records: Mutex::new(HashMap::new()),
        unconfirmed: Arc::new(Mutex::new(None)),
    };
    (host, receiver)
}

fn call(name: &str, args: Value) -> ChildCall {
    ChildCall {
        parent_call_id: "parent".into(),
        seq: 1,
        name: name.into(),
        args,
        remaining: Duration::from_secs(60),
    }
}

/// Started and terminal events of the child, in order.
fn lifecycle(events: &[Event]) -> (usize, usize) {
    let mut started = 0;
    let mut completed = 0;
    for event in events {
        match &event.kind {
            EventKind::Tool(ToolEvent::ToolStarted { call_id, .. }) if call_id == "parent/1" => {
                started += 1;
            }
            EventKind::Tool(ToolEvent::ToolCompleted { call_id, .. }) if call_id == "parent/1" => {
                completed += 1;
            }
            _ => {}
        }
    }
    (started, completed)
}

/// Polls the call `polls` times, yielding in between, then drops it unfinished (or
/// keeps its result when it finished first). `polls` counts the cancellation points
/// the call passes: before its tool starts, while it runs, as it completes.
async fn run_until_cancelled(
    host: &RuntimeRunJsHost,
    name: &str,
    args: Value,
    polls: usize,
) -> Option<ChildOutcome> {
    let mut call = Box::pin(host.call(call(name, args)));
    for _ in 0..polls {
        if let Poll::Ready(outcome) = futures_util::poll!(call.as_mut()) {
            return Some(outcome);
        }
        tokio::task::yield_now().await;
    }
    None
}

#[tokio::test]
async fn a_call_cancelled_at_any_point_is_recorded_and_closed_exactly_once() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("a.txt");
    std::fs::write(&file, "text\n").unwrap();
    let file = file.to_string_lossy().into_owned();
    let scenarios = [
        ("read", json!({"path": file})),
        ("shell", json!({"command": "sleep 30"})),
        ("shell", json!({"command": "echo done"})),
    ];
    for (name, args) in scenarios {
        for polls in 1..=12 {
            let (host, mut receiver) = host();
            let finished = run_until_cancelled(&host, name, args.clone(), polls)
                .await
                .is_some();
            host.drain_children("parent").await.unwrap();

            let records = host.take_records("parent");
            assert_eq!(
                records.len(),
                1,
                "{name} cancelled after {polls} polls (finished: {finished}): one record"
            );
            assert_eq!((records[0].seq, records[0].name.as_str()), (1, name));
            let events = receiver.drain_pending_snapshot();
            assert_eq!(
                lifecycle(&events),
                (1, 1),
                "{name} cancelled after {polls} polls (finished: {finished}): one start, one terminal event"
            );
        }
    }
}

#[tokio::test]
async fn a_finished_call_is_recorded_once_with_its_outcome_and_the_program_gets_the_result() {
    let (host, mut receiver) = host();
    let outcome = host
        .call(call("shell", json!({"command": "echo done; exit 3"})))
        .await;

    assert!(outcome.is_error);
    let records = host.take_records("parent");
    assert_eq!(records.len(), 1);
    assert!(records[0].is_error);
    assert_eq!(lifecycle(&receiver.drain_pending_snapshot()), (1, 1));
    let reply = outcome.result.unwrap();
    assert_eq!(reply["ok"], false);
    assert_eq!(reply["detail"]["exit_code"], 3);
    assert_eq!(reply["text"], "done\n");
}
