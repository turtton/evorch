mod support;

use std::sync::Arc;

use agents::Role;
use event_bus::{AgentRunPhase, Event, EventBus, EventKind, LifecycleEvent, ToolEvent};
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
    tool_response, tool_responses,
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

async fn complete_escalation(
    runtime: &AgentRuntime,
    receiver: &mut event_bus::EventReceiver,
    source: RunId,
) -> (RunId, Vec<Event>) {
    let (new_run, mut events) = events_through_escalation(receiver, source).await;
    assert_eq!(
        timeout(Duration::from_secs(5), runtime.wait(new_run)).await,
        Ok(Ok(AgentRunPhase::Done))
    );
    events.extend(drain_events(receiver).await);
    (new_run, events)
}

fn spawned_event_index(events: &[Event], new_run: RunId) -> usize {
    events
        .iter()
        .position(|event| {
            matches!(
                &event.kind,
                EventKind::Lifecycle(LifecycleEvent::AgentRunStarted { run_id, .. })
                    if run_id == &new_run.to_string()
            )
        })
        .expect("escalated run start event")
}

fn source_done_event_index(events: &[Event], source: RunId) -> usize {
    events
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
        .expect("source Done event")
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
    let (new_run, events) = complete_escalation(&runtime, &mut receiver, source).await;

    // Then: 新 run は root Orchestrator で、旧 run の Done 後にメモ全文脈から開始する
    assert_eq!(runtime.escalation_source(new_run), Ok(Some(source)));
    let started = spawned_event_index(&events, new_run);
    let source_done = source_done_event_index(&events, source);
    assert!(matches!(
        &events[started].kind,
        EventKind::Lifecycle(LifecycleEvent::AgentRunStarted {
            parent_run_id: None,
            role,
            ..
        }) if role == "orchestrator"
    ));
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

#[tokio::test]
async fn batch_edit_then_escalate_skips_remaining_tools_and_orders_terminal_before_spawn() {
    // Given: 最初の edit のみ実ファイルへ書き込み、その後の escalate を含む複数ツール応答
    let temp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let first_path = temp.path().join("first.txt");
    let skipped_path = temp.path().join("skipped.txt");
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_responses([
            (
                "e1",
                "edit",
                json!({ "path": first_path, "new_string": "first edit" }),
            ),
            (
                "esc",
                "escalate",
                json!({
                    "original_request": "複数ツールの実行を引き継ぐ",
                    "escalation_reason": "追加の調整が必要",
                    "findings": ["最初の編集を完了した"],
                    "files_touched": ["first.txt"],
                    "blockers": ["単独 run では完結しない"],
                    "workspace_state": "M first.txt",
                    "suggested_next": "Orchestrator が残作業を分担する"
                }),
            ),
            (
                "e2",
                "edit",
                json!({ "path": skipped_path, "new_string": "must not run" }),
            ),
        ])),
        Ok(text_response("引継ぎ完了", FinishReason::Stop)),
    ]));
    let (runtime, bus) = runtime_with(model);
    let mut receiver = bus.subscribe();

    // When: Worker root run が edit → escalate → edit の batch を実行する
    let source = runtime.delegate_background(
        Role::Worker,
        "BATCH ESCALATION SOURCE".to_string(),
        RunConfig::default(),
    );
    let (new_run, events) = complete_escalation(&runtime, &mut receiver, source).await;

    // Then: 最初の edit は完了し、source 終端後に新 root が開始し、残りの edit は開始されない
    assert!(first_path.exists());
    let first_complete = events
        .iter()
        .position(|event| {
            matches!(
                &event.kind,
                EventKind::Tool(ToolEvent::ToolCompleted { call_id, .. }) if call_id == "e1"
            )
        })
        .expect("first edit completion event");
    let source_done = source_done_event_index(&events, source);
    let new_started = spawned_event_index(&events, new_run);
    assert!(first_complete < source_done);
    assert!(source_done < new_started);
    assert!(!events.iter().any(|event| {
        matches!(
            &event.kind,
            EventKind::Tool(ToolEvent::ToolStarted { call_id, .. }) if call_id == "e2"
        )
    }));
}

#[tokio::test]
async fn escalation_memo_summary_matches_recorded_memo() {
    // Given: 全メモ項目を持つ escalate と、新 root を終了する共有モデル
    let model = Arc::new(ScriptedModel::new([
        Ok(escalation_response()),
        Ok(text_response("引継ぎ完了", FinishReason::Stop)),
    ]));
    let (runtime, bus) = runtime_with(Arc::clone(&model));
    let mut receiver = bus.subscribe();

    // When: Worker root run がエスカレーションする
    let source = runtime.delegate_background(
        Role::Worker,
        "MEMO SUMMARY SOURCE".to_string(),
        RunConfig::default(),
    );
    let (_new_run, events) = complete_escalation(&runtime, &mut receiver, source).await;

    // Then: イベント要約と記録済みメモが全フィールドで一致し、新 prompt は移譲元 run ID を含む
    let summary = events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::Lifecycle(LifecycleEvent::EscalationRequested {
                source_run_id,
                summary,
                ..
            }) if source_run_id == &source.to_string() => Some(summary),
            _ => None,
        })
        .expect("escalation requested event");
    let memo = runtime
        .escalation_memo(source)
        .expect("source escalation memo is recorded");
    assert_eq!(summary.original_request, memo.original_request);
    assert_eq!(summary.escalation_reason, memo.escalation_reason);
    assert_eq!(
        summary.files_touched,
        memo.files_touched
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
    );
    assert_eq!(summary.blockers, memo.blockers);
    assert_eq!(summary.suggested_next, memo.suggested_next);
    let observed = model.observed().await;
    let prompt = observed
        .iter()
        .find_map(|messages| {
            user_text(messages).filter(|text| text.starts_with("[evorch escalation"))
        })
        .expect("new run initial escalation prompt");
    assert!(prompt.contains(&source.to_string()));
}

#[tokio::test]
async fn escalated_run_has_no_run_result() {
    // Given: エスカレーション後に新 root が自然停止する共有モデル
    let model = Arc::new(ScriptedModel::new([
        Ok(escalation_response()),
        Ok(text_response("引継ぎ完了", FinishReason::Stop)),
    ]));
    let (runtime, bus) = runtime_with(model);
    let mut receiver = bus.subscribe();

    // When: Worker root run がエスカレーションする
    let source = runtime.delegate_background(
        Role::Worker,
        "RESULT CONTRACT SOURCE".to_string(),
        RunConfig::default(),
    );
    let (_new_run, _) = complete_escalation(&runtime, &mut receiver, source).await;

    // Then: source run は完了テキストを公開せず、結果は新 root の責務となる
    assert_eq!(runtime.run_result(source), Ok(None));
}
