mod support;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use event_bus::{
    AgentMessageKind, AgentRunPhase, EventBus, EventKind, EventReceiver, LifecycleEvent,
};
use providers::{ChatResponse, ContentBlock, FinishReason, Message, RequestTurnContext, ToolSpec};
use runtime::{
    AgentInvocationContext, AgentModel, AgentRuntime, Role, RunConfig, RunId, RuntimeError,
};
use sandbox::DirectSandbox;
use tools::ToolExecutor;

type Observed = Vec<(AgentInvocationContext, Vec<Message>)>;
struct RecordingModel {
    responses: Mutex<VecDeque<ChatResponse>>,
    observed: Mutex<Observed>,
}

#[async_trait]
impl AgentModel for RecordingModel {
    async fn complete(
        &self,
        invocation: &AgentInvocationContext,
        _: Role,
        messages: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.observed
            .lock()
            .unwrap()
            .push((invocation.clone(), messages.to_vec()));
        Ok(self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("scripted response"))
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "provenance-model".into()
    }
}

fn runtime(
    responses: impl IntoIterator<Item = ChatResponse>,
) -> (AgentRuntime, Arc<RecordingModel>, EventReceiver) {
    let bus = Arc::new(EventBus::new(256));
    let events = bus.subscribe();
    let model = Arc::new(RecordingModel {
        responses: Mutex::new(responses.into_iter().collect()),
        observed: Mutex::new(Vec::new()),
    });
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        bus.clone(),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    (
        AgentRuntime::new(bus, executor, model.clone()),
        model,
        events,
    )
}

async fn waiting(events: &mut EventReceiver, run: RunId) {
    loop {
        let event = events.recv().await.unwrap();
        if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to, .. }) =
            event.kind
            && run_id == run.to_string()
        {
            match to {
                AgentRunPhase::Waiting => return,
                AgentRunPhase::Error | AgentRunPhase::Stopped | AgentRunPhase::Done => {
                    panic!("run ended before input waiting: {to:?}")
                }
                _ => {}
            }
        }
    }
}

fn interactive() -> RunConfig {
    RunConfig {
        interactive: true,
        keep_alive: true,
        ..Default::default()
    }
}

fn turn(observed: &Observed, index: usize) -> RequestTurnContext {
    observed[index].0.turn.clone().expect("runtime provenance")
}

fn assert_unknown(turn: &RequestTurnContext) {
    assert_eq!(turn.origin, None);
    assert_eq!((turn.prompt_index, turn.turn_index), (None, None));
}

#[tokio::test]
async fn human_boundaries_tool_rounds_and_agent_inputs_have_distinct_provenance() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("evidence");
    std::fs::write(&path, "evidence").unwrap();
    let stop = || support::text_response("done", FinishReason::Stop);
    let (runtime, model, mut events) = runtime([
        support::tool_response("read-1", "read", serde_json::json!({"path":path})),
        stop(),
        stop(),
        support::tool_response("read-child", "read", serde_json::json!({"path":path})),
        stop(),
        stop(),
        stop(),
        stop(),
        stop(),
        stop(),
    ]);
    let storage = support::ReviewStorage::new();
    storage.attach(&runtime);
    let root = runtime
        .delegate_chat(
            "provenance",
            Role::Orchestrator,
            "Read evidence".into(),
            interactive(),
        )
        .unwrap();
    waiting(&mut events, root).await;
    {
        let observed = model.observed.lock().unwrap();
        let first = turn(&observed, 0);
        assert_eq!(first, turn(&observed, 1));
        assert_eq!(
            (
                first.origin.as_deref(),
                first.prompt_index,
                first.turn_index
            ),
            (Some("human"), Some(1), Some(1))
        );
        assert!(!first.is_subagent);
        assert_eq!(first.parent_subagent_id, None);
        assert!(
            observed[1]
                .1
                .iter()
                .flat_map(|m| &m.content)
                .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
        );
        assert_eq!(&observed[1].1[..observed[0].1.len()], observed[0].1);
    }
    runtime
        .send_message(root, "next human input".into())
        .unwrap();
    waiting(&mut events, root).await;
    let second = turn(&model.observed.lock().unwrap(), 2);
    assert_eq!((second.prompt_index, second.turn_index), (Some(2), Some(2)));
    assert_ne!(
        second.prompt_id,
        turn(&model.observed.lock().unwrap(), 0).prompt_id
    );
    let child = runtime
        .delegate_background_as_child(root, Role::Worker, "agent task", interactive())
        .unwrap();
    waiting(&mut events, child).await;
    let delegated = turn(&model.observed.lock().unwrap(), 3);
    assert_unknown(&delegated);
    assert!(delegated.is_subagent);
    assert_eq!(delegated.parent_subagent_id, None);
    assert_eq!(delegated, turn(&model.observed.lock().unwrap(), 4));
    assert_eq!(
        model.observed.lock().unwrap()[4].0.run_id,
        child.to_string()
    );
    runtime
        .send_agent_message(
            child,
            root,
            AgentMessageKind::Send,
            "agent notification",
            None,
        )
        .unwrap();
    waiting(&mut events, root).await;
    let agent_input = turn(&model.observed.lock().unwrap(), 5);
    assert_unknown(&agent_input);
    assert!(!agent_input.is_subagent);
    assert_eq!(agent_input.parent_subagent_id, None);
    assert_ne!(agent_input.prompt_id, second.prompt_id);
    runtime
        .send_message(root, "human after unknown input".into())
        .unwrap();
    waiting(&mut events, root).await;
    let later = turn(&model.observed.lock().unwrap(), 6);
    assert_eq!(later.origin.as_deref(), Some("human"));
    assert_eq!((later.prompt_index, later.turn_index), (None, None));
    runtime
        .send_agent_message(
            root,
            child,
            AgentMessageKind::Send,
            "child internal input",
            None,
        )
        .unwrap();
    waiting(&mut events, child).await;
    let child_internal = turn(&model.observed.lock().unwrap(), 7);
    assert_unknown(&child_internal);
    assert_eq!(child_internal.parent_subagent_id, None);
    assert!(child_internal.is_subagent);
    runtime
        .send_message(child, "child human input".into())
        .unwrap();
    waiting(&mut events, child).await;
    let child_human = turn(&model.observed.lock().unwrap(), 8);
    assert_eq!(child_human.origin.as_deref(), Some("human"));
    assert_eq!(child_human.parent_subagent_id, None);
    assert!(child_human.is_subagent);
    let grandchild = runtime
        .delegate_background_as_child(child, Role::Worker, "nested agent task", interactive())
        .unwrap();
    waiting(&mut events, grandchild).await;
    let nested = turn(&model.observed.lock().unwrap(), 9);
    assert!(nested.is_subagent);
    assert_eq!(nested.parent_subagent_id, Some(child.to_string()));
    runtime.cancel(grandchild).unwrap();
    runtime.wait(grandchild).await.unwrap();
    runtime.cancel(child).unwrap();
    runtime.wait(child).await.unwrap();
    runtime.cancel(root).unwrap();
    runtime.wait(root).await.unwrap();
}

#[tokio::test]
async fn ambiguous_programmatic_input_is_not_declared_human() {
    let (runtime, model, _) = runtime([support::text_response("done", FinishReason::Stop)]);
    let run = runtime.delegate_background(
        Role::Worker,
        "programmatic task".into(),
        RunConfig::default(),
    );
    assert_eq!(runtime.wait(run).await, Ok(AgentRunPhase::Done));
    let input = turn(&model.observed.lock().unwrap(), 0);
    assert_unknown(&input);
    assert!(!input.is_subagent);
}

#[tokio::test]
async fn snapshots_preserve_known_counts_and_never_guess_missing_restore_positions() {
    let directory = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: directory.path().join("turns.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let database = storage::Database::open(&config).unwrap();
    let (before, model, _) = runtime([support::text_response("first", FinishReason::Stop)]);
    let before = before.with_run_store(runtime::RunStore::open(&config, storage.handle()).unwrap());
    let first = before
        .delegate_chat(
            "restored",
            Role::Worker,
            "first human".into(),
            RunConfig::default(),
        )
        .unwrap();
    assert_eq!(before.wait(first).await, Ok(AgentRunPhase::Done));
    let original = turn(&model.observed.lock().unwrap(), 0);
    let record = database.run_context(&first.to_string()).unwrap().unwrap();
    let descriptor: runtime::restore::RunRestoreDescriptor =
        serde_json::from_str(&record.config_json).unwrap();
    assert_eq!(descriptor.turn, Some(original));
    drop(before);
    let (after, model, _) = runtime([support::text_response("second", FinishReason::Stop)]);
    let after = after.with_run_store(runtime::RunStore::open(&config, storage.handle()).unwrap());
    let second = after
        .delegate_chat(
            "restored",
            Role::Worker,
            "second human".into(),
            RunConfig::default(),
        )
        .unwrap();
    assert_eq!(after.wait(second).await, Ok(AgentRunPhase::Done));
    let known = turn(&model.observed.lock().unwrap(), 0);
    assert_eq!((known.prompt_index, known.turn_index), (Some(2), Some(2)));
    let mut record = database.run_context(&second.to_string()).unwrap().unwrap();
    let mut descriptor: serde_json::Value = serde_json::from_str(&record.config_json).unwrap();
    descriptor.as_object_mut().unwrap().remove("turn");
    record.config_json = serde_json::to_string(&descriptor).unwrap();
    storage.handle().upsert_run_context(&record).unwrap();
    drop(after);
    let (legacy, model, _) = runtime([support::text_response("third", FinishReason::Stop)]);
    let legacy = legacy.with_run_store(runtime::RunStore::open(&config, storage.handle()).unwrap());
    let third = legacy
        .delegate_chat(
            "restored",
            Role::Worker,
            "third human".into(),
            RunConfig::default(),
        )
        .unwrap();
    assert_eq!(legacy.wait(third).await, Ok(AgentRunPhase::Done));
    let unknown = turn(&model.observed.lock().unwrap(), 0);
    assert_eq!(unknown.origin.as_deref(), Some("human"));
    assert_eq!((unknown.prompt_index, unknown.turn_index), (None, None));
}

#[tokio::test]
async fn resuming_an_unfinished_request_retains_its_saved_prompt_identity() {
    let directory = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: directory.path().join("resume.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let database = storage::Database::open(&config).unwrap();
    let (before, model, _) = runtime([support::text_response("first", FinishReason::Stop)]);
    let before = before.with_run_store(runtime::RunStore::open(&config, storage.handle()).unwrap());
    let run = before
        .delegate_chat(
            "unfinished",
            Role::Worker,
            "human request".into(),
            RunConfig::default(),
        )
        .unwrap();
    before.wait(run).await.unwrap();
    let (original, sent) = model.observed.lock().unwrap()[0].clone();
    let mut record = database.run_context(&run.to_string()).unwrap().unwrap();
    let mut descriptor: runtime::restore::RunRestoreDescriptor =
        serde_json::from_str(&record.config_json).unwrap();
    // An interrupted provider has the exact sent input but no retained assistant reply.
    descriptor.completed_turn_end = None;
    // Saved prompt provenance survives, but current task ownership must replace stale ancestry.
    descriptor.turn.as_mut().unwrap().parent_subagent_id = Some("stale-parent".into());
    descriptor.turn.as_mut().unwrap().is_subagent = true;
    record.config_json = serde_json::to_string(&descriptor).unwrap();
    record.messages_json = serde_json::to_string(&sent).unwrap();
    record.terminal_phase = "Checkpoint".into();
    storage.handle().upsert_run_context(&record).unwrap();
    drop(before);
    let (after, model, mut events) = runtime([
        support::text_response("retried", FinishReason::Stop),
        support::text_response("next", FinishReason::Stop),
    ]);
    let after = after.with_run_store(runtime::RunStore::open(&config, storage.handle()).unwrap());
    assert_eq!(after.resume_chat(run, RunConfig::default()).unwrap(), run);
    waiting(&mut events, run).await;
    assert_eq!(model.observed.lock().unwrap()[0].0.turn, original.turn);
    after
        .continue_goal(run, "new human input".into(), RunConfig::default())
        .unwrap();
    waiting(&mut events, run).await;
    let next = turn(&model.observed.lock().unwrap(), 1);
    assert_eq!((next.prompt_index, next.turn_index), (Some(2), Some(2)));
    assert_ne!(next.prompt_id, original.turn.unwrap().prompt_id);
    after.cancel(run).unwrap();
    after.wait(run).await.unwrap();
}
