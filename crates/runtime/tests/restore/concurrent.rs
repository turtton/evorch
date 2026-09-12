use super::*;
use event_bus::{AgentMessageKind, EventKind, LifecycleEvent};
use std::sync::Barrier;

struct ObservedModel {
    inner: Arc<ScriptedModel>,
    calls: tokio::sync::mpsc::Sender<Vec<Message>>,
    released: tokio::sync::watch::Receiver<bool>,
}

#[async_trait::async_trait]
impl runtime::AgentModel for ObservedModel {
    async fn complete(
        &self,
        invocation: &runtime::AgentInvocationContext,
        role: Role,
        messages: &[Message],
        tools: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        self.calls.send(messages.to_vec()).await.unwrap();
        self.released
            .clone()
            .wait_for(|released| *released)
            .await
            .unwrap();
        self.inner.complete(invocation, role, messages, tools).await
    }

    fn selected_model(&self, role: Role) -> String {
        runtime::AgentModel::selected_model(self.inner.as_ref(), role)
    }
}

fn gated_runtime(
    model: Arc<ScriptedModel>,
) -> (
    AgentRuntime,
    Arc<EventBus>,
    tokio::sync::mpsc::Receiver<Vec<Message>>,
    tokio::sync::watch::Sender<bool>,
) {
    let (calls, observed) = tokio::sync::mpsc::channel(8);
    let (release, released) = tokio::sync::watch::channel(true);
    let bus = Arc::new(EventBus::new(128));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    let runtime = AgentRuntime::new(
        Arc::clone(&bus),
        executor,
        Arc::new(ObservedModel {
            inner: model,
            calls,
            released,
        }),
    );
    (runtime, bus, observed, release)
}

#[tokio::test]
async fn second_send_after_restored_model_starts_delivers_both_turns() {
    // Given: the first restored completion is gated after its input was captured.
    let (_dir, config, storage, _) = storage_fixture();
    let model = Arc::new(ScriptedModel::new(
        (0..4).map(|_| Ok(text_response("answer", FinishReason::Stop))),
    ));
    let (runtime, _, mut observed, release) = gated_runtime(model);
    let runtime = runtime.with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let parent =
        runtime.delegate_background(Role::Orchestrator, "parent".into(), RunConfig::default());
    terminal(&runtime, parent).await;
    observed.recv().await.unwrap();
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, "child", RunConfig::default())
        .unwrap();
    terminal(&runtime, child).await;
    observed.recv().await.unwrap();
    release.send_replace(false);
    runtime
        .send_agent_message(parent, child, AgentMessageKind::Send, "turn-0", None)
        .unwrap();
    timeout(Duration::from_secs(5), observed.recv())
        .await
        .unwrap()
        .unwrap();

    // When: the second send arrives after the first model input is fixed.
    runtime
        .send_agent_message(parent, child, AgentMessageKind::Send, "turn-1", None)
        .unwrap();
    release.send_replace(true);

    // Then: the queued input reaches a second completion and the run terminates.
    let second = timeout(Duration::from_secs(5), observed.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.last().unwrap().role, providers::Role::User);
    for (id, turn) in [("msg-1", "turn-0"), ("msg-2", "turn-1")] {
        assert_eq!(
            second
                .iter()
                .filter(|message| message.content
                    == vec![ContentBlock::Text {
                        text: format!("[agent-message id={id} from={parent} kind=send]\n{turn}"),
                    }])
                .count(),
            1
        );
    }
    assert_eq!(terminal(&runtime, child).await, AgentRunPhase::Done);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_sends_restore_once_and_deliver_both_turns() {
    // Given: a completed child whose restored model call is gated.
    let (_dir, config, storage, _) = storage_fixture();
    let model = Arc::new(ScriptedModel::new(
        (0..8).map(|_| Ok(text_response("answer", FinishReason::Stop))),
    ));
    let (runtime, bus, mut observed, release) = gated_runtime(model);
    let runtime = runtime.with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let parent =
        runtime.delegate_background(Role::Orchestrator, "parent".into(), RunConfig::default());
    terminal(&runtime, parent).await;
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, "child", RunConfig::default())
        .unwrap();
    terminal(&runtime, child).await;
    observed.recv().await.unwrap();
    observed.recv().await.unwrap();
    release.send_replace(false);
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
    // Opening the latch also releases later completions if the second send missed the first input.
    release.send_replace(true);
    let phase = terminal(&runtime, child).await;
    assert_eq!(phase, AgentRunPhase::Done);
    let mut inputs = Vec::new();
    while let Ok(input) = observed.try_recv() {
        inputs.push(input);
    }
    let last = inputs.last().unwrap();
    for turn in ["turn-0", "turn-1"] {
        let suffix = format!("\n{turn}");
        assert_eq!(
            last.iter()
                .flat_map(|message| &message.content)
                .filter(|block| {
                    matches!(block, ContentBlock::Text { text } if text.ends_with(&suffix))
                })
                .count(),
            1
        );
    }
}
