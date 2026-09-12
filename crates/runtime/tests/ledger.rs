mod support;

use std::sync::Arc;

use config::{CompactionConfig, SummarizerKind};
use event_bus::{AgentMessageKind, AgentRunPhase, EventBus, EventKind, LedgerEvent};
use providers::{ContentBlock, FinishReason, Message, ToolResultContent};
use runtime::{AgentRuntime, Role, RunConfig, RunId, RunStore};
use serde_json::{Value, json};
use storage::{Database, Storage, StorageConfig};
use support::{ScriptedModel, text_response, tool_response};
use tokio::time::{Duration, timeout};
use tools::ToolExecutor;

fn runtime_with(model: Arc<ScriptedModel>) -> (AgentRuntime, Arc<EventBus>) {
    let bus = Arc::new(EventBus::new(128));
    let executor = Arc::new(ToolExecutor::new(Arc::clone(&bus)));
    (AgentRuntime::new(Arc::clone(&bus), executor, model), bus)
}

fn storage_fixture() -> (tempfile::TempDir, StorageConfig, Storage, Database) {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("ledger.sqlite3"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let database = Database::open(&config).unwrap();
    (dir, config, storage, database)
}

async fn terminal(runtime: &AgentRuntime, run: RunId) {
    assert_eq!(
        timeout(Duration::from_secs(5), runtime.wait(run))
            .await
            .unwrap()
            .unwrap(),
        AgentRunPhase::Done
    );
}

fn result(messages: &[Message], id: &str) -> (bool, Value) {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|block| match block {
            ContentBlock::ToolResult {
                tool_call_id,
                content,
                is_error,
            } if tool_call_id == id => {
                let ToolResultContent::Text { text } = &content[0];
                Some((
                    *is_error,
                    serde_json::from_str(text).expect("JSON tool result"),
                ))
            }
            _ => None,
        })
        .expect("tool result")
}

#[tokio::test]
async fn ledger_append_persists_and_read_returns_entries() {
    // Given: a real store and an agent that reads before and after compaction.
    let (_dir, config, storage, database) = storage_fixture();
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response("empty", "ledger_read", json!({}))),
        Ok(tool_response(
            "append",
            "ledger_append",
            json!({"body":"durable decision"}),
        )),
        Ok(tool_response("before", "ledger_read", json!({}))),
        Ok(text_response("ready", FinishReason::Stop)),
        Ok(tool_response("after", "ledger_read", json!({}))),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let (runtime, bus) = runtime_with(Arc::clone(&model));
    let runtime = runtime
        .with_run_store(RunStore::open(&config, storage.handle()).unwrap())
        .with_compaction(CompactionConfig {
            context_window_tokens: 1_000_000,
            keep_recent_tokens: 1,
            max_summary_bytes: 64,
            summarizer: SummarizerKind::Structural,
            ..CompactionConfig::default()
        });
    // When: the scripted run appends, compacts and terminates.
    let mut events = bus.subscribe();
    let run = runtime.delegate_background(
        Role::Orchestrator,
        "request ".repeat(100),
        RunConfig {
            interactive: true,
            ..RunConfig::default()
        },
    );
    timeout(Duration::from_secs(5), async {
        loop {
            if matches!(
                events.recv().await.unwrap().kind,
                EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStateChanged {
                    to: AgentRunPhase::Waiting,
                    ..
                })
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
    runtime.compact(run).unwrap();
    runtime.send_message(run, "continue".into()).unwrap();
    terminal(&runtime, run).await;
    // Then: the same immutable entry is read from both the meta-op and SQLite.
    let observed = model.observed().await;
    assert_eq!(result(&observed[1], "empty"), (false, json!([])));
    let (is_error, appended) = result(&observed[2], "append");
    assert!(!is_error);
    let entries = database.run_ledger(&run.to_string()).unwrap();
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry.body, "durable decision");
    assert_eq!(appended["seq"], entry.seq);
    let expected = (
        false,
        json!([{"seq":entry.seq,"body":entry.body,"created_at_ns":entry.created_at_ns}]),
    );
    assert_eq!(result(&observed[3], "before"), expected);
    assert_eq!(result(observed.last().unwrap(), "after"), expected);
    let record = database.run_context(&run.to_string()).unwrap().unwrap();
    let checkpoints: Vec<runtime::CompactionCheckpoint> =
        serde_json::from_str(&record.checkpoints_json).unwrap();
    assert_eq!(checkpoints.len(), 1);
}

#[tokio::test]
async fn ledger_append_emits_event() {
    // Given: a subscribed bus and a configured store.
    let (_dir, config, storage, _) = storage_fixture();
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response(
            "append",
            "ledger_append",
            json!({"body":"event body"}),
        )),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let (runtime, bus) = runtime_with(Arc::clone(&model));
    let runtime = runtime.with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let mut events = bus.subscribe();
    // When: the agent appends an entry.
    let run = runtime.delegate_background(Role::Worker, "request".into(), RunConfig::default());
    terminal(&runtime, run).await;
    // Then: the emitted sequence and caller match the successful tool response.
    let observed = model.observed().await;
    let (is_error, appended) = result(&observed[1], "append");
    assert!(!is_error);
    let event = timeout(Duration::from_secs(5), async {
        loop {
            if let EventKind::Ledger(event) = events.recv().await.unwrap().kind {
                break event;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        event,
        LedgerEvent::RunLedgerAppended {
            run_id: run.to_string(),
            seq: appended["seq"].as_u64().unwrap(),
            body: "event body".into()
        }
    );
}

#[tokio::test]
async fn ledger_append_without_run_store_returns_tool_error() {
    // Given: a runtime without persistence.
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response(
            "append",
            "ledger_append",
            json!({"body":"decision"}),
        )),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let (runtime, _) = runtime_with(Arc::clone(&model));
    // When: the agent attempts an append.
    let run = runtime.delegate_background(Role::Worker, "request".into(), RunConfig::default());
    terminal(&runtime, run).await;
    // Then: the run continues with a typed missing-store tool error.
    let observed = model.observed().await;
    let (is_error, error) = result(&observed[1], "append");
    assert!(is_error);
    assert_eq!(error["code"], "run_store_unavailable");
}

#[tokio::test]
async fn restored_run_sees_ledger_entries_before_trigger_message() {
    // Given: two ledger entries appended by a subsequently terminated run.
    let (_dir, config, storage, _) = storage_fixture();
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response(
            "first",
            "ledger_append",
            json!({"body":"first decision"}),
        )),
        Ok(tool_response(
            "second",
            "ledger_append",
            json!({"body":"second decision"}),
        )),
        Ok(text_response("done", FinishReason::Stop)),
        Ok(text_response("sender", FinishReason::Stop)),
        Ok(text_response("restored", FinishReason::Stop)),
    ]));
    let (runtime, _) = runtime_with(Arc::clone(&model));
    let runtime = runtime.with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let run = runtime.delegate_background(Role::Worker, "original".into(), RunConfig::default());
    terminal(&runtime, run).await;
    let sender = runtime
        .delegate_background_as_child(run, Role::Worker, "sender", RunConfig::default())
        .unwrap();
    terminal(&runtime, sender).await;
    // When: a send restores the terminated run.
    runtime
        .send_agent_message(sender, run, AgentMessageKind::Send, "trigger", None)
        .unwrap();
    terminal(&runtime, run).await;
    // Then: one ledger user message precedes the trigger in the first restored request.
    let observed = model.observed().await;
    let restored = observed.last().unwrap();
    let ledger: Vec<_> = restored
        .iter()
        .enumerate()
        .filter(|(_, message)| {
            message.role == providers::Role::User
                && matches!(&message.content[..],
            [ContentBlock::Text { text }] if text.starts_with("[run-ledger]\n"))
        })
        .collect();
    assert_eq!(ledger.len(), 1);
    let (index, message) = ledger[0];
    assert_eq!(index, restored.len() - 2);
    let first = result(&observed[1], "first").1["seq"].as_u64().unwrap();
    let second = result(&observed[2], "second").1["seq"].as_u64().unwrap();
    assert_eq!(
        message.content,
        vec![ContentBlock::Text {
            text: format!(
                "[run-ledger]\n- seq {first}: first decision\n- seq {second}: second decision"
            )
        }]
    );
    assert_eq!(restored[index + 1].role, providers::Role::User);
}
