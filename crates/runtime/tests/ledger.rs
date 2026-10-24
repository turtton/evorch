mod support;

use std::sync::Arc;

use config::{CompactionConfig, SummarizerKind};
use event_bus::{AgentMessageKind, AgentRunPhase, EventBus, EventKind, LedgerEvent};
use providers::{ContentBlock, FinishReason, Message, ToolResultContent};
use runtime::{AgentRuntime, Role, RunConfig, RunId, RunStore};
use serde_json::{Value, json};
use storage::{Database, Storage, StorageConfig};
use support::{ScriptedModel, text_response, tool_response};
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
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Done);
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
                let ToolResultContent::Text { text } = &content[0] else {
                    panic!("expected text result");
                };
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
async fn working_note_write_persists_and_read_returns_latest() {
    // Given: a real store and an agent that reads before and after compaction.
    let (_dir, config, storage, database) = storage_fixture();
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response("empty", "working_note_read", json!({}))),
        Ok(tool_response(
            "append",
            "working_note_write",
            json!({"body":"durable decision"}),
        )),
        Ok(tool_response("before", "working_note_read", json!({}))),
        Ok(text_response("ready", FinishReason::Stop)),
        Ok(tool_response("after", "working_note_read", json!({}))),
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
    runtime.compact(run).unwrap();
    runtime.send_message(run, "continue".into()).unwrap();
    terminal(&runtime, run).await;
    // Then: the same immutable entry is read from both the meta-op and SQLite.
    let observed = model.observed().await;
    assert_eq!(result(&observed[1], "empty"), (false, Value::Null));
    let (is_error, appended) = result(&observed[2], "append");
    assert!(!is_error);
    let entries = database.run_ledger(&run.to_string()).unwrap();
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry.body, "durable decision");
    assert_eq!(appended["revision"], entry.seq);
    let expected = (
        false,
        json!({"note_id":run.to_string(),"revision":entry.seq,"writer_run_id":run.to_string(),"body":entry.body,"created_at_ns":entry.created_at_ns}),
    );
    assert_eq!(result(&observed[3], "before"), expected);
    assert_eq!(result(observed.last().unwrap(), "after"), expected);
    let record = database.run_context(&run.to_string()).unwrap().unwrap();
    let checkpoints: Vec<runtime::CompactionCheckpoint> =
        serde_json::from_str(&record.checkpoints_json).unwrap();
    assert_eq!(checkpoints.len(), 1);
}

#[tokio::test]
async fn working_note_write_emits_audit_event() {
    // Given: a subscribed bus and a configured store.
    let (_dir, config, storage, _) = storage_fixture();
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response(
            "append",
            "working_note_write",
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
    let event = loop {
        if let EventKind::Ledger(event) = events.recv().await.unwrap().kind {
            break event;
        }
    };
    assert_eq!(
        event,
        LedgerEvent::RunLedgerAppended {
            run_id: run.to_string(),
            seq: appended["revision"].as_u64().unwrap(),
            body: "event body".into()
        }
    );
}

#[tokio::test]
async fn working_note_write_without_run_store_returns_tool_error() {
    // Given: a runtime without persistence.
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response(
            "append",
            "working_note_write",
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
async fn full_history_restore_does_not_replay_working_notes() {
    // Given: two ledger entries appended by a subsequently terminated run.
    let (_dir, config, storage, _) = storage_fixture();
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response(
            "first",
            "working_note_write",
            json!({"body":"first decision"}),
        )),
        Ok(tool_response(
            "second",
            "working_note_write",
            json!({"body":"second decision"}),
        )),
        Ok(text_response("done", FinishReason::Stop)),
        Ok(text_response("sender", FinishReason::Stop)),
        Ok(text_response("restored", FinishReason::Stop)),
        // The sender's completion report, held while the run was finished.
        Ok(text_response("report handled", FinishReason::Stop)),
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
    // Restoring full history must preserve the already-sent prefix without replaying notes.
    let observed = model.observed().await;
    let restored = observed.last().unwrap();
    assert!(restored.starts_with(&observed[2]));
    assert!(!restored.iter().flat_map(|message| &message.content).any(
        |block| matches!(block, ContentBlock::Text { text } if text.starts_with("[working-note]"))
    ));
}

#[tokio::test]
async fn compaction_selects_only_the_latest_working_note() {
    let (_dir, config, storage, _) = storage_fixture();
    let model = Arc::new(ScriptedModel::new([
        Ok(text_response(
            &"old evidence ".repeat(100),
            FinishReason::Stop,
        )),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let (runtime, bus) = runtime_with(model.clone());
    let runtime = runtime
        .with_run_store(RunStore::open(&config, storage.handle()).unwrap())
        .with_compaction(CompactionConfig {
            context_window_tokens: 1_000_000,
            keep_recent_tokens: 1,
            max_summary_bytes: 64,
            summarizer: SummarizerKind::Structural,
            ..Default::default()
        });
    let mut events = bus.subscribe();
    let run = runtime.delegate_background(
        Role::Worker,
        "original request".into(),
        RunConfig {
            interactive: true,
            ..Default::default()
        },
    );
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
    storage
        .handle()
        .append_run_ledger(&run.to_string(), "stale decision")
        .unwrap();
    let latest = storage
        .handle()
        .append_run_ledger(&run.to_string(), "current decision")
        .unwrap();
    runtime.compact(run).unwrap();
    runtime.send_message(run, "continue".into()).unwrap();
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Done);
    let requests = model.observed().await;
    let notes: Vec<Value> = requests[1]
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::Text { text } => text
                .strip_prefix("[working-note]\n")
                .map(|json| serde_json::from_str(json).unwrap()),
            _ => None,
        })
        .collect();
    assert_eq!(
        notes.len(),
        1,
        "successful compaction must deliver the latest note"
    );
    assert_eq!(notes[0]["revision"], latest);
    assert_eq!(notes[0]["body"], "current decision");
}

#[tokio::test]
async fn original_assistant_explanation_survives_restart_for_host_reply() {
    let (_dir, config, storage, database) = storage_fixture();
    let model = Arc::new(ScriptedModel::new([Ok(text_response(
        "ORIGINAL approval explanation",
        FinishReason::Stop,
    ))]));
    let (runtime, _) = runtime_with(model);
    let runtime = runtime.with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let run = runtime
        .delegate_chat(
            "approval",
            Role::Worker,
            "original request".into(),
            RunConfig::default(),
        )
        .unwrap();
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Done);
    drop(runtime);
    let (runtime, _) = runtime_with(Arc::new(ScriptedModel::new([])));
    let runtime = runtime.with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    runtime
        .continue_goal(run, "yes".into(), RunConfig::default())
        .unwrap();
    let rows = database
        .review_evidence_for_lineage(&[run.to_string()])
        .unwrap();
    let row = rows.iter().find(|row| row.text == "yes").unwrap();
    let reference: Value = serde_json::from_str(
        row.reference_context
            .as_deref()
            .expect("the original assistant explanation must survive restart"),
    )
    .unwrap();
    assert_eq!(reference["text"], "ORIGINAL approval explanation");
    assert_eq!(reference["source_run_id"], run.to_string());
    assert!(reference["turn_end"].as_u64().unwrap() > 0);
    runtime.stop(run, runtime::StopScope::SelfOnly).unwrap();
    runtime.wait(run).await.unwrap();
}

#[tokio::test]
async fn escalation_without_a_note_still_exposes_original_source_history() {
    let (_dir, config, storage, _) = storage_fixture();
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response(
            "escalate",
            "escalate",
            json!({"original_request":"request", "escalation_reason":"coordination needed"}),
        )),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let (runtime, bus) = runtime_with(model.clone());
    let runtime = runtime.with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let mut events = bus.subscribe();
    let source = runtime.delegate_background(
        Role::Worker,
        "original source evidence".into(),
        RunConfig::default(),
    );
    let target = loop {
        if let EventKind::Lifecycle(event_bus::LifecycleEvent::EscalationRequested {
            new_run_id,
            ..
        }) = events.recv().await.unwrap().kind
        {
            break new_run_id.parse::<RunId>().unwrap();
        }
    };
    assert_eq!(runtime.wait(target).await.unwrap(), AgentRunPhase::Done);
    let requests = model.observed().await;
    let manifest = requests[1]
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|block| match block {
            ContentBlock::Text { text } => text
                .strip_prefix("[history-sources]\n")
                .map(|json| serde_json::from_str::<Value>(json).unwrap()),
            _ => None,
        })
        .expect("empty note must still carry original evidence access");
    assert_eq!(manifest[0]["source_run_id"], source.to_string());
    assert!(manifest[0]["message_count"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn writes_replace_clear_and_reject_invalid_body_without_losing_latest() {
    let (_dir, config, storage, database) = storage_fixture();
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response(
            "first",
            "working_note_write",
            json!({"body":"obsolete decision"}),
        )),
        Ok(tool_response(
            "replace",
            "working_note_write",
            json!({"body":"current decision"}),
        )),
        Ok(tool_response(
            "invalid",
            "working_note_write",
            json!({"body":"x".repeat(8193)}),
        )),
        Ok(tool_response("latest", "working_note_read", json!({}))),
        Ok(tool_response(
            "clear",
            "working_note_write",
            json!({"body":""}),
        )),
        Ok(tool_response("cleared", "working_note_read", json!({}))),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let (runtime, _) = runtime_with(model.clone());
    let runtime = runtime.with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let run = runtime.delegate_background(Role::Worker, "request".into(), RunConfig::default());
    terminal(&runtime, run).await;
    let requests = model.observed().await;
    assert!(result(&requests[3], "invalid").0);
    let latest = result(&requests[4], "latest").1;
    assert_eq!(latest["body"], "current decision");
    assert_eq!(latest, result(&requests[2], "replace").1);
    let cleared = result(&requests[6], "cleared").1;
    assert_eq!(cleared["body"], "");
    assert!(cleared["revision"].as_u64().unwrap() > latest["revision"].as_u64().unwrap());
    let revisions = database.run_ledger(&run.to_string()).unwrap();
    assert_eq!(revisions.len(), 3);
    assert!(revisions.last().unwrap().body.is_empty());
}
