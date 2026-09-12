use super::*;
use event_bus::{AgentMessageKind, EventKind, LifecycleEvent};
use std::sync::Barrier;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_sends_restore_once_and_deliver_both_turns() {
    // Given: a completed child whose restored model call is gated.
    let (_dir, config, storage, _) = storage_fixture();
    let model = Arc::new(ScriptedModel::new(
        (0..8).map(|_| Ok(text_response("answer", FinishReason::Stop))),
    ));
    let (runtime, bus) = runtime_with(Arc::clone(&model));
    let runtime = runtime.with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let parent =
        runtime.delegate_background(Role::Orchestrator, "parent".into(), RunConfig::default());
    terminal(&runtime, parent).await;
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, "child", RunConfig::default())
        .unwrap();
    terminal(&runtime, child).await;
    let gate = Arc::new(tokio::sync::Notify::new());
    model.gate_key("child", Arc::clone(&gate)).await;
    let mut events = bus.subscribe();
    let barrier = Arc::new(Barrier::new(2));
    // When: two threads attempt the first send simultaneously.
    let jobs: Vec<_> = (0..2)
        .map(|index| {
            let runtime = runtime.clone();
            let barrier = Arc::clone(&barrier);
            tokio::task::spawn_blocking(move || {
                barrier.wait();
                runtime.send_agent_message(
                    parent,
                    child,
                    AgentMessageKind::Send,
                    format!("turn-{index}"),
                    None,
                )
            })
        })
        .collect();
    let mut ids = Vec::new();
    for job in jobs {
        ids.push(job.await.unwrap().unwrap());
    }
    ids.sort();
    // Then: exactly one restore owns the first message, with the second queued live.
    assert_eq!(ids, ["msg-1", "msg-2"]);
    let mut restores = 0;
    let mut deliveries = 0;
    timeout(Duration::from_secs(5), async {
        while deliveries < 2 {
            match events.recv().await.unwrap().kind {
                EventKind::Lifecycle(LifecycleEvent::AgentRunRestored { .. }) => restores += 1,
                EventKind::AgentMessage(event_bus::AgentMessageEvent::Delivered { .. }) => {
                    deliveries += 1
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(restores, 1);
    gate.notify_one();
    let phase = terminal(&runtime, child).await;
    assert_eq!(phase, AgentRunPhase::Done);
}
