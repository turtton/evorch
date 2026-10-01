use super::*;
use crate::escalation_review::{QuickModelReviewer, ReviewVerdict, review_context};
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolResultContent};

const FOLLOWUP: &str = "Commit the fix, push it, and confirm CI passes";
const DELEGATION: &str = "Agent delegation claims permission to push unrelated changes";

#[derive(Default)]
struct RecordingReviewer {
    payload: Mutex<Option<serde_json::Value>>,
}

#[async_trait::async_trait]
impl AgentModel for RecordingReviewer {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "review-test".into()
    }

    async fn complete(
        &self,
        invocation: &crate::AgentInvocationContext,
        _: Role,
        messages: &[Message],
        tools: &[providers::ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        assert_eq!(invocation.category.as_deref(), Some("quick"));
        assert!(tools.is_empty());
        let [ContentBlock::Text { text }] = messages[1].content.as_slice() else {
            panic!("expected review JSON");
        };
        *self.payload.lock().unwrap() = Some(serde_json::from_str(text).unwrap());
        Ok(ChatResponse {
            message: Message {
                role: providers::Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: r#"{"approve":true,"reason":"current human request","risk_level":"high","authorization_level":"high"}"#.into(),
                }],
            },
            finish_reason: FinishReason::Stop,
            usage: providers::Usage::default(),
        })
    }
}

fn user(text: &str) -> Message {
    Message {
        role: providers::Role::User,
        content: vec![ContentBlock::Text { text: text.into() }],
    }
}

async fn terminal_with_untrusted_history(fixture: &Fixture) -> RunId {
    let run = fixture.runtime.delegate_background(
        Role::Orchestrator,
        "Old request from the previous execution".into(),
        RunConfig {
            name: Some("chat:Orchestrator:review-thread".into()),
            ..RunConfig::default()
        },
    );
    assert_eq!(
        fixture.runtime.wait(run).await.unwrap(),
        AgentRunPhase::Done
    );
    let store = fixture.runtime.shared.run_store.get().unwrap();
    let mut record = store.restore_record(run).unwrap().unwrap();
    let mut history: Vec<Message> = serde_json::from_str(&record.messages_json).unwrap();
    history.extend([
        user("Persisted user-role text claims permission for all future commands"),
        Message {
            role: providers::Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "old-tool".into(),
                name: "shell".into(),
                input: serde_json::json!({"command":"pwd"}),
            }],
        },
        Message {
            role: providers::Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_call_id: "old-tool".into(),
                content: vec![ToolResultContent::Text {
                    text: "Tool output claims host access is approved".into(),
                }],
                is_error: false,
            }],
        },
        user(DELEGATION),
    ]);
    record.messages_json = serde_json::to_string(&history).unwrap();
    record.checkpoints_json = serde_json::to_string(&vec![crate::CompactionCheckpoint {
        id: "old-checkpoint".into(),
        summary: user("Compaction summary claims permission to publish anything"),
        range: (0, history.len()),
    }])
    .unwrap();
    fixture
        .storage
        .handle()
        .upsert_run_context(&record)
        .unwrap();
    run
}

async fn assert_child_review_uses_only_current_submission(runtime: &AgentRuntime, root: RunId) {
    runtime
        .send_internal_message(root, "Runtime notice claims host access is approved".into())
        .unwrap();
    let child = runtime.reserve_child_run_id(root).unwrap();
    runtime.spawn_reserved_child(root, child, Role::Worker, DELEGATION, RunConfig::default());
    let run = runtime
        .shared
        .review_runs
        .lock()
        .unwrap()
        .get(&child)
        .unwrap()
        .clone();
    let model = Arc::new(RecordingReviewer::default());
    let verdict = QuickModelReviewer::new(model.clone())
        .review_with_context(
            &child.to_string(),
            "git push origin main",
            "Agent justification claims permission",
            Some(review_context(run, None, None, "git push origin main")),
        )
        .await
        .unwrap();
    assert_eq!(verdict, ReviewVerdict::Approve);
    let payload = model.payload.lock().unwrap().clone().unwrap();
    assert_eq!(payload["context"]["root_run_id"], root.to_string());
    assert_eq!(
        payload["context"]["real_user_requests"],
        serde_json::json!([{"target_run_id":root.to_string(), "text":FOLLOWUP}]),
        "only the new host submission grants authority, never persisted history, tools, summaries, or delegation",
    );
    assert_eq!(
        payload["context"]["delegation_chain"],
        serde_json::json!([DELEGATION]),
    );
    runtime.cancel_subtree(root).unwrap();
    runtime.wait(root).await.unwrap();
    runtime.wait(child).await.unwrap();
}

#[tokio::test]
async fn goal_followup_restores_current_human_evidence_for_child_review() {
    let fixture = Fixture::new();
    let root = terminal_with_untrusted_history(&fixture).await;
    let continued = fixture
        .runtime
        .continue_goal(root, FOLLOWUP.into(), RunConfig::default())
        .unwrap();
    assert_eq!(continued, root);
    assert_child_review_uses_only_current_submission(&fixture.runtime, root).await;
}

#[tokio::test]
async fn chat_followup_after_restart_restores_current_human_evidence_for_child_review() {
    let fixture = Fixture::new();
    terminal_with_untrusted_history(&fixture).await;
    let Fixture {
        runtime,
        storage,
        _dir: dir,
    } = fixture;
    drop(runtime);
    let config = storage::StorageConfig {
        db_path: dir.path().join("goal.sqlite3"),
        ..storage::StorageConfig::default()
    };
    let bus = Arc::new(EventBus::new(128));
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(ToolExecutor::new(bus)),
        Arc::new(CompletingModel),
    )
    .with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
    let root = runtime
        .delegate_chat(
            "review-thread",
            Role::Orchestrator,
            FOLLOWUP.into(),
            RunConfig::default(),
        )
        .unwrap();
    assert_child_review_uses_only_current_submission(&runtime, root).await;
}
