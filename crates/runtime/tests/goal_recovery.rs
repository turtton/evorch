mod support;

use std::sync::Arc;

use event_bus::{
    AgentMessage, AgentMessageKind, EventBus, EventKind, GateRejection, OrchestratorEvent,
};
use providers::FinishReason;
use runtime::orchestration::delivery::FixtureDeliveryAdapter;
use runtime::orchestration::ledger::{GoalLedger, OrchestrationSettings};
use runtime::orchestration::supervisor::GoalSupervisor;
use runtime::{AgentModel, AgentRuntime};
use sandbox::DirectSandbox;
use tokio::time::{Duration, timeout};
use tools::ToolExecutor;

use support::{ScriptedModel, text_response};

#[tokio::test]
async fn recover_starts_new_run_with_snapshot_and_transcript_context() {
    // Given: crash 前 snapshot と、最後の会話行を含む永続 transcript
    let old_run = runtime::RunId::new(41);
    let created = OrchestratorEvent::GoalCreated {
        goal_id: "goal-recover".into(),
        session_id: "session-recover".into(),
        project_id: "evorch".into(),
        thread_id: "thread-recover".into(),
        goal: "restore the delivery loop".into(),
        references: vec![],
        constraints: vec![],
        repo: "turtton/evorch".into(),
        base_ref: "main".into(),
        root_run_id: old_run.to_string(),
    };
    let mut ledger = GoalLedger::new(&created);
    ledger
        .apply(&OrchestratorEvent::FinishRejected {
            goal_id: "goal-recover".into(),
            run_id: old_run.to_string(),
            rejections: vec![GateRejection::NoDeliverableBranch],
        })
        .expect("rejection applies");
    for event in [
        OrchestratorEvent::TaskProgressed {
            task_id: "task-recover".into(),
            run_id: old_run.to_string(),
            progress: serde_json::json!({"step": 7}),
            reason: "saved".into(),
        },
        OrchestratorEvent::TaskCheckpoint {
            task_id: "task-recover".into(),
            run_id: old_run.to_string(),
            tool_call_count: 50,
            cumulative_input_tokens: 1000,
            cumulative_output_tokens: 200,
            elapsed_ms: 500,
        },
        OrchestratorEvent::TaskRetryScheduled {
            goal_id: "goal-recover".into(),
            task_id: "task-recover".into(),
            attempt: 1,
            reason: "retry".into(),
            new_run_id: "run-old-retry".into(),
        },
        OrchestratorEvent::TaskStaleMarked {
            task_id: "task-recover".into(),
            run_id: "run-old-retry".into(),
            last_heartbeat_ns: 900,
        },
    ] {
        ledger.apply(&event).expect("durable state applies");
    }
    let snapshot = ledger.snapshot().clone();
    let expected = snapshot.clone();
    let transcript = vec![AgentMessage {
        message_id: "msg-last".into(),
        sender_run_id: old_run.to_string(),
        recipient_run_id: "run-42".into(),
        kind: AgentMessageKind::Send,
        content: "LAST-TRANSCRIPT-LINE".into(),
        reply_to: None,
    }];
    let model = Arc::new(ScriptedModel::new([Ok(text_response(
        "recovered",
        FinishReason::Stop,
    ))]));
    let bus = Arc::new(EventBus::new(256));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    let runtime_model: Arc<dyn AgentModel> = model.clone();
    let runtime = AgentRuntime::new(Arc::clone(&bus), executor, runtime_model);
    let handle = GoalSupervisor::spawn(
        runtime.clone(),
        bus,
        Arc::new(FixtureDeliveryAdapter::default()),
        OrchestrationSettings::default(),
    );

    // When: snapshot から recovery run を開始する
    let mut events = handle.subscribe();
    let new_run = handle
        .recover(snapshot, transcript)
        .expect("recovery command accepted");
    timeout(Duration::from_secs(2), async {
        loop {
            let event = events.recv().await.expect("supervisor event");
            if matches!(
                event.kind,
                EventKind::Orchestrator(OrchestratorEvent::ContinuationDispatched { .. })
            ) {
                break;
            }
        }
    })
    .await
    .expect("recovery attached");
    timeout(Duration::from_secs(2), runtime.wait(new_run))
        .await
        .expect("recovery run timeout")
        .expect("recovery run exists");

    // Then: 新 RunId の初期 prompt に goal/transcript/unmet が入り、旧 tool snapshot は復元しない
    assert_ne!(new_run, old_run);
    let observed = model.observed().await;
    let prompt = observed[0][0]
        .content
        .iter()
        .find_map(|block| match block {
            providers::ContentBlock::Text { text } => Some(text.as_str()),
            providers::ContentBlock::Image { .. }
            | providers::ContentBlock::Reasoning { .. }
            | providers::ContentBlock::ToolUse { .. }
            | providers::ContentBlock::ToolResult { .. } => None,
        })
        .expect("recovery prompt");
    assert!(prompt.contains("restore the delivery loop"));
    assert!(prompt.contains("LAST-TRANSCRIPT-LINE"));
    assert!(prompt.contains("NoDeliverableBranch"));
    assert!(!prompt.contains("tool snapshot"));
    assert_eq!(runtime.list_agents().len(), 1);
    let recovered = handle.snapshot("goal-recover").expect("recovered snapshot");
    assert_eq!(recovered.task_progress, expected.task_progress);
    assert_eq!(recovered.task_checkpoints, expected.task_checkpoints);
    assert_eq!(recovered.task_attempts, expected.task_attempts);
    assert_eq!(recovered.task_retries, expected.task_retries);
    assert_eq!(recovered.stale_marks, expected.stale_marks);
    assert_eq!(recovered.task_runs, expected.task_runs);
}
