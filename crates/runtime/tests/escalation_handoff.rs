mod support;

use std::sync::Arc;

use agents::Role;
use event_bus::{AgentRunPhase, Event, EventBus, EventKind, LifecycleEvent};
use providers::{ContentBlock, FinishReason, Message, Role as MessageRole};
use runtime::workspace::{Project, WorktreeManager};
use runtime::{AgentRuntime, MergeMode, RunConfig, RunId, WorkspaceInspection, WorkspaceMode};
use sandbox::DirectSandbox;
use serde_json::json;
use tokio::sync::Notify;
use tokio::time::{Duration, timeout};
use tools::ToolExecutor;

use support::{
    ScriptedModel, drain_events, git, init_git_repo, recording_factory, text_response,
    tool_response,
};

fn escalation_response() -> providers::ChatResponse {
    tool_response(
        "escalate-1",
        "escalate",
        json!({
            "original_request": "依存関係の更新を完了する",
            "escalation_reason": "編集失敗が連続した",
            "findings": ["API 境界を特定した", "既存テストを確認した"],
            "files_touched": ["crates/runtime/src/runtime.rs"],
            "blockers": ["単独 run では調整できない", "追加担当が必要"],
            "workspace_state": "M crates/runtime/src/runtime.rs",
            "suggested_next": "担当を分割して検証する"
        }),
    )
}

fn runtime_with(model: Arc<ScriptedModel>) -> (AgentRuntime, Arc<EventBus>) {
    let bus = Arc::new(EventBus::new(128));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    (AgentRuntime::new(Arc::clone(&bus), executor, model), bus)
}

async fn events_through_escalation(
    receiver: &mut event_bus::EventReceiver,
    source_run_id: RunId,
) -> (RunId, Vec<Event>) {
    let mut events = Vec::new();
    let new_run_id = timeout(Duration::from_secs(5), async {
        loop {
            let event = receiver.recv().await.expect("event bus remains open");
            let escalated = match &event.kind {
                EventKind::Lifecycle(LifecycleEvent::EscalationRequested {
                    source_run_id: source,
                    new_run_id,
                    ..
                }) if source == &source_run_id.to_string() => Some(new_run_id.clone()),
                _ => None,
            };
            events.push(event);
            if let Some(new_run_id) = escalated {
                let number = new_run_id
                    .strip_prefix("run-")
                    .expect("run id prefix")
                    .parse::<u64>()
                    .expect("numeric run id");
                return RunId::new(number);
            }
        }
    })
    .await
    .expect("escalation event timeout");
    (new_run_id, events)
}

fn user_text(messages: &[Message]) -> Option<&str> {
    messages.iter().find_map(|message| {
        (message.role == MessageRole::User).then(|| {
            message.content.iter().find_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                ContentBlock::Reasoning { .. }
                | ContentBlock::ToolUse { .. }
                | ContentBlock::ToolResult { .. } => None,
            })
        })?
    })
}

#[tokio::test]
async fn escalate_spawns_orchestrator_root_run_with_memo_prompt() {
    // Given: 有効な escalate の後、新 Orchestrator が自然停止する共有モデル
    let model = Arc::new(ScriptedModel::new([
        Ok(escalation_response()),
        Ok(text_response("引継ぎ完了", FinishReason::Stop)),
    ]));
    let (runtime, bus) = runtime_with(Arc::clone(&model));
    let mut receiver = bus.subscribe();

    // When: Worker root run がエスカレーションし、新 run も終端する
    let source = runtime.delegate_background(
        Role::Worker,
        "SHARED SOURCE".to_string(),
        RunConfig::default(),
    );
    let (new_run, mut events) = events_through_escalation(&mut receiver, source).await;
    assert_eq!(
        timeout(Duration::from_secs(5), runtime.wait(new_run)).await,
        Ok(Ok(AgentRunPhase::Done))
    );
    events.extend(drain_events(&mut receiver).await);

    // Then: 新 run は root Orchestrator で、旧 run の Done 後にメモ全文脈から開始する
    assert_eq!(runtime.escalation_source(new_run), Ok(Some(source)));
    let started = events
        .iter()
        .position(|event| {
            matches!(
                &event.kind,
                EventKind::Lifecycle(LifecycleEvent::AgentRunStarted {
                    run_id,
                    parent_run_id: None,
                    role,
                    ..
                }) if run_id == &new_run.to_string() && role == "orchestrator"
            )
        })
        .expect("new root orchestrator start event");
    let source_done = events
        .iter()
        .position(|event| {
            matches!(
                &event.kind,
                EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                    run_id,
                    to: AgentRunPhase::Done,
                    ..
                }) if run_id == &source.to_string()
            )
        })
        .expect("source Done event");
    assert!(source_done < started);

    let observed = model.observed().await;
    let prompt = observed
        .iter()
        .find_map(|messages| {
            user_text(messages).filter(|text| text.starts_with("[evorch escalation"))
        })
        .expect("new run initial escalation prompt");
    for value in [
        "依存関係の更新を完了する",
        "API 境界を特定した",
        "既存テストを確認した",
        "crates/runtime/src/runtime.rs",
        "単独 run では調整できない",
        "追加担当が必要",
        "担当を分割して検証する",
        &source.to_string(),
    ] {
        assert!(prompt.contains(value), "memo value missing: {value}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn isolated_escalation_adopts_workspace_exclusively_until_new_run_finishes() {
    // Given: isolated Worker の昇格先だけを gate する共有モデルと recording factory
    let (_temp, repo) = init_git_repo();
    let gate = Arc::new(Notify::new());
    let model = Arc::new(ScriptedModel::new([]));
    model
        .add_keyed("ISOLATED SOURCE", [Ok(escalation_response())])
        .await;
    model
        .add_keyed(
            "[evorch escalation",
            [Ok(text_response("引継ぎ完了", FinishReason::Stop))],
        )
        .await;
    model
        .gate_key("[evorch escalation", Arc::clone(&gate))
        .await;
    let bus = Arc::new(EventBus::new(128));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    let manager = WorktreeManager::new(Project::new(repo.clone()).expect("git repo is valid"));
    let (factory, mounts) = recording_factory();
    let runtime =
        AgentRuntime::with_workspace_context(Arc::clone(&bus), executor, model, manager, factory);
    let mut receiver = bus.subscribe();

    // When: source が worktree を新 root run へ移譲し、新 run のモデル呼び出しで停止する
    let source = runtime.delegate_background(
        Role::Worker,
        "ISOLATED SOURCE".to_string(),
        RunConfig {
            workspace_mode: WorkspaceMode::Isolated,
            ..RunConfig::default()
        },
    );
    let source_path = repo.join(".evorch/worktrees").join(source.to_string());
    let source_branch = format!("evorch/task/{source}");
    let (new_run, _events) = events_through_escalation(&mut receiver, source).await;
    timeout(Duration::from_secs(5), async {
        loop {
            if mounts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len()
                == 2
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("adopted executor build timeout");

    // Then: 所有中は同じ path/branch が新 run だけに紐付き、source cleanup は走らない
    assert_eq!(
        runtime
            .inspect_agent(new_run)
            .expect("new run inspection")
            .workspace,
        Some(WorkspaceInspection {
            mode: WorkspaceMode::Isolated,
            branch: Some(source_branch.clone()),
            worktree_path: Some(source_path.clone()),
            merge_mode: MergeMode::Branch,
        })
    );
    assert_eq!(
        runtime
            .inspect_agent(source)
            .expect("source inspection")
            .workspace,
        Some(WorkspaceInspection {
            mode: WorkspaceMode::Isolated,
            branch: Some(source_branch.clone()),
            worktree_path: None,
            merge_mode: MergeMode::Branch,
        })
    );
    assert!({
        let captured = mounts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        captured
            .iter()
            .all(|mount| mount.workspace_root == source_path)
    });
    assert!(source_path.exists());

    gate.notify_one();
    assert_eq!(
        timeout(Duration::from_secs(5), runtime.wait(new_run)).await,
        Ok(Ok(AgentRunPhase::Done))
    );
    timeout(Duration::from_secs(5), async {
        while source_path.exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("adopted worktree cleanup timeout");
    let branches = git(&repo, &["branch", "--list", &source_branch]);
    assert!(branches.status.success());
    assert!(String::from_utf8_lossy(&branches.stdout).contains(&source_branch));
}
