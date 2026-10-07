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
    (
        AgentRuntime::new(Arc::clone(&bus), executor, model).with_sequential_run_ids(),
        bus,
    )
}

async fn events_through_escalation(
    receiver: &mut event_bus::EventReceiver,
    source_run_id: RunId,
) -> (RunId, Vec<Event>) {
    let mut events = Vec::new();
    let new_run_id = loop {
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
            break new_run_id.parse::<RunId>().expect("run id");
        }
    };
    (new_run_id, events)
}

async fn complete_escalation(
    runtime: &AgentRuntime,
    receiver: &mut event_bus::EventReceiver,
    source: RunId,
) -> (RunId, Vec<Event>) {
    let (new_run, mut events) = events_through_escalation(receiver, source).await;
    assert_eq!(runtime.wait(new_run).await, Ok(AgentRunPhase::Done));
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
                ContentBlock::Compaction { .. } => {
                    tracing::warn!("この処理では compaction block をスキップします");
                    None
                }
                ContentBlock::Image { .. }
                | ContentBlock::Reasoning { .. }
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
    let requested = events
        .iter()
        .position(|event| {
            matches!(
                &event.kind,
                EventKind::Lifecycle(LifecycleEvent::EscalationRequested {
                    source_run_id,
                    new_run_id,
                    ..
                }) if source_run_id == &source.to_string() && new_run_id == &new_run.to_string()
            )
        })
        .expect("escalation link event");
    assert!(source_done < requested && requested < started);

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

#[tokio::test]
async fn escalation_keeps_the_source_project_after_the_active_project_changes() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let source_gate = Arc::new(Notify::new());
    let target_gate = Arc::new(Notify::new());
    let first_model = Arc::new(ScriptedModel::new([
        Ok(escalation_response()),
        Ok(text_response(
            "first project orchestrator",
            FinishReason::Stop,
        )),
    ]));
    first_model
        .gate_key("SOURCE PROJECT", source_gate.clone())
        .await;
    first_model
        .gate_key("[evorch escalation", target_gate.clone())
        .await;
    let second_model = Arc::new(ScriptedModel::new([]));
    let (runtime, bus) = runtime_with(first_model.clone());
    let first_path = first.path().to_path_buf();
    let first_for_resolver = first_path.clone();
    let first_for_model = first_model.clone();
    let second_for_model = second_model.clone();
    let runtime = runtime
        .with_project_rules(Arc::new(runtime::RulesSource::new(
            runtime::ProjectTrust::Approved,
            runtime::RulesSettings::from(&config::RulesConfig::default()),
            None,
            Some(first_path.clone()),
            None,
        )))
        .with_project_models(Arc::new(move |root| {
            Some(if root == first_for_resolver {
                first_for_model.clone() as Arc<dyn runtime::AgentModel>
            } else {
                second_for_model.clone() as Arc<dyn runtime::AgentModel>
            })
        }));
    runtime.set_project_root(first_path.clone()).unwrap();
    let mut events = bus.subscribe();
    let source =
        runtime.delegate_background(Role::Worker, "SOURCE PROJECT".into(), RunConfig::default());
    first_model.wait_for_request(0).await;
    runtime
        .set_project_root(second.path().to_path_buf())
        .unwrap();
    source_gate.notify_one();
    let (target, _) = events_through_escalation(&mut events, source).await;
    first_model.wait_for_request(1).await;
    assert_eq!(
        runtime
            .inspect_agent(target)
            .unwrap()
            .workspace
            .unwrap()
            .active_root,
        Some(first_path)
    );
    target_gate.notify_one();
    assert_eq!(runtime.wait(target).await.unwrap(), AgentRunPhase::Done);
    assert_eq!(
        runtime.run_result(target).unwrap().as_deref(),
        Some("first project orchestrator")
    );
    assert!(second_model.observed().await.is_empty());
}

struct EscalationAdmissionModel {
    script: ScriptedModel,
    entered: Notify,
    release: Notify,
    failure: std::sync::atomic::AtomicU8,
}

#[async_trait::async_trait]
impl runtime::AgentModel for EscalationAdmissionModel {
    fn requires_admission(&self) -> bool {
        true
    }

    async fn admit(
        &self,
        _: &runtime::AgentInvocationContext,
        role: Role,
    ) -> Result<(), runtime::RuntimeError> {
        if role == Role::Orchestrator {
            self.entered.notify_one();
            self.release.notified().await;
            match self.failure.swap(0, std::sync::atomic::Ordering::SeqCst) {
                1 => {
                    return Err(runtime::RuntimeError::Model {
                        reason: "catalog unavailable".into(),
                    });
                }
                2 => panic!("provider admission panicked"),
                _ => {}
            }
        }
        Ok(())
    }

    async fn complete(
        &self,
        context: &runtime::AgentInvocationContext,
        role: Role,
        messages: &[Message],
        specs: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        runtime::AgentModel::complete(&self.script, context, role, messages, specs).await
    }

    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "admission-test".into()
    }
}

#[tokio::test]
async fn escalation_publishes_child_and_goal_before_admission_and_can_stop_while_pending() {
    for cancel in [false, true] {
        let model = Arc::new(EscalationAdmissionModel {
            script: ScriptedModel::new([
                Ok(escalation_response()),
                Ok(text_response("admitted orchestrator", FinishReason::Stop)),
            ]),
            entered: Notify::new(),
            release: Notify::new(),
            failure: std::sync::atomic::AtomicU8::new(0),
        });
        let bus = Arc::new(EventBus::new(128));
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(ToolExecutor::new(bus.clone())),
            model.clone(),
        );
        let mut events = bus.subscribe();
        let source = runtime.reserve_run_id();
        runtime.bind_thread_root("source-thread", source).unwrap();
        let goal = runtime
            .create_thread_goal(
                "source-thread",
                source,
                "Objective".into(),
                vec!["Evidence".into()],
            )
            .unwrap();
        runtime
            .set_goal_checks_paused("source-thread", &goal.goal_id, true)
            .unwrap();
        runtime.spawn_reserved(source, None, Role::Worker, "request", RunConfig::default());
        let (target, before) = events_through_escalation(&mut events, source).await;
        model.entered.notified().await;
        let child = event_bus::escalation_thread_id(&target.to_string());
        assert!(runtime.thread_goal("source-thread").is_none());
        assert_eq!(runtime.thread_goal(&child).unwrap().goal_id, goal.goal_id);
        assert!(matches!(events.recv().await.unwrap().kind,
            EventKind::Orchestrator(event_bus::OrchestratorEvent::ThreadGoalUpdated { snapshot })
                if snapshot.thread_id == child));
        assert!(!runtime.list_agents().iter().any(|run| run.run_id == target));
        assert!(source_done_event_index(&before, source) < before.len() - 1);
        let waiter = tokio::spawn({
            let runtime = runtime.clone();
            async move { runtime.wait(target).await }
        });
        if cancel {
            runtime.stop(target, runtime::StopScope::SelfOnly).unwrap();
        }
        model.release.notify_one();
        let result = waiter.await.unwrap();
        if cancel {
            assert!(matches!(
                result,
                Err(runtime::RuntimeError::RunTerminated { .. })
            ));
            assert!(runtime.thread_goal(&child).unwrap().work_stopped);
            assert!(!runtime.list_agents().iter().any(|run| run.run_id == target));
            assert_eq!(model.script.observed().await.len(), 1);
        } else {
            assert_eq!(result.unwrap(), AgentRunPhase::Done);
            assert_eq!(model.script.observed().await.len(), 2);
        }
    }
}

#[tokio::test]
async fn child_worker_cannot_escalate_into_a_new_root() {
    let model = Arc::new(ScriptedModel::new([]));
    model
        .add_keyed(
            "PARENT",
            [Ok(text_response("parent done", FinishReason::Stop))],
        )
        .await;
    model
        .add_keyed(
            "CHILD",
            [
                Ok(escalation_response()),
                Ok(text_response("child continued", FinishReason::Stop)),
            ],
        )
        .await;
    let (runtime, bus) = runtime_with(model);
    let mut receiver = bus.subscribe();
    let parent = runtime.delegate_background(
        Role::Orchestrator,
        "PARENT".to_string(),
        RunConfig::default(),
    );
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, "CHILD", RunConfig::default())
        .expect("parent run exists");

    assert_eq!(runtime.wait(child).await, Ok(AgentRunPhase::Done));
    assert_eq!(
        runtime.run_result(child),
        Ok(Some("child continued".to_string()))
    );
    assert!(runtime.escalation_memo(child).is_none());
    let events = drain_events(&mut receiver).await;
    assert!(!events.iter().any(|event| {
        matches!(
            &event.kind,
            EventKind::Lifecycle(LifecycleEvent::EscalationRequested { source_run_id, .. })
                if source_run_id == &child.to_string()
        )
    }));
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
    let runtime = AgentRuntime::with_workspace_context(
        Arc::clone(&bus),
        executor,
        model.clone(),
        manager,
        factory,
    )
    .with_sequential_run_ids();
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
    model.wait_for_request(1).await;

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
            active_root: Some(source_path.clone()),
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
            active_root: None,
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
    assert_eq!(runtime.wait(new_run).await, Ok(AgentRunPhase::Done));
    assert!(
        !source_path.exists(),
        "terminal publication follows workspace cleanup"
    );
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
                "write",
                json!({ "path": first_path, "content": "first edit" }),
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
                "write",
                json!({ "path": skipped_path, "content": "must not run" }),
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

#[tokio::test]
async fn failed_or_stopped_admission_can_continue_the_same_child_with_current_authority() {
    use runtime::ownership::{Lease, OwnerPermit, Registry, ThreadOwner};
    for failure in ["error-after-restart", "stop", "cancel", "panic"] {
        let (_repo_dir, repo) = init_git_repo();
        let directory = tempfile::tempdir().unwrap();
        let storage_config = storage::StorageConfig {
            db_path: directory.path().join("events.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(storage_config.clone()).unwrap();
        let registry_path = directory.path().join("owners.db");
        let mut registry = Registry::open(&registry_path).unwrap();
        let lease = Lease {
            owner_id: "host".into(),
            generation: 1,
            expires_at: u64::MAX,
        };
        registry
            .start(&ThreadOwner::new("source-thread".into(), lease.clone()))
            .unwrap();
        let model = Arc::new(EscalationAdmissionModel {
            script: ScriptedModel::new([
                Ok(escalation_response()),
                Ok(text_response("continued in the child", FinishReason::Stop)),
            ]),
            entered: Notify::new(),
            release: Notify::new(),
            failure: std::sync::atomic::AtomicU8::new(match failure {
                "error-after-restart" => 1,
                "panic" => 2,
                _ => 0,
            }),
        });
        let make_runtime = || {
            let bus = Arc::new(EventBus::new(256));
            let executor = Arc::new(ToolExecutor::with_standard_tools(
                bus.clone(),
                Arc::new(DirectSandbox::new_unchecked()),
            ));
            let (factory, _) = recording_factory();
            let runtime = AgentRuntime::with_workspace_context(
                bus.clone(),
                executor,
                model.clone(),
                WorktreeManager::new(Project::new(repo.clone()).unwrap()),
                factory,
            )
            .with_run_store(runtime::RunStore::open(&storage_config, storage.handle()).unwrap());
            runtime.set_project_root(repo.clone()).unwrap();
            (runtime, bus)
        };
        let (mut runtime, mut bus) = make_runtime();
        let mut events = bus.subscribe();
        let source = runtime.reserve_run_id();
        runtime.bind_thread_root("source-thread", source).unwrap();
        let goal = runtime
            .create_thread_goal(
                "source-thread",
                source,
                "Objective".into(),
                vec!["Evidence".into()],
            )
            .unwrap();
        runtime
            .set_goal_checks_paused("source-thread", &goal.goal_id, true)
            .unwrap();
        runtime.spawn_reserved(
            source,
            None,
            Role::Worker,
            "request",
            RunConfig {
                ownership: Some(OwnerPermit {
                    registry_path: registry_path.clone(),
                    thread_id: "source-thread".into(),
                    lease,
                    run_id: None,
                }),
                workspace_mode: WorkspaceMode::Isolated,
                ..Default::default()
            },
        );
        let (target, _) = events_through_escalation(&mut events, source).await;
        model.entered.notified().await;
        let child = event_bus::escalation_thread_id(&target.to_string());
        let retained_path = repo.join(".evorch/worktrees").join(source.to_string());
        std::fs::write(retained_path.join("keep.txt"), "pending handoff changes").unwrap();
        if failure == "stop" {
            runtime.stop(target, runtime::StopScope::SelfOnly).unwrap();
        }
        if failure == "cancel" {
            runtime.cancel_subtree(target).unwrap();
        }
        model.release.notify_one();
        assert!(runtime.wait(target).await.is_err());
        assert!(retained_path.exists());
        assert_eq!(model.script.observed().await.len(), 1);
        let inherited = runtime.thread_goal(&child).unwrap();
        let old_owner = registry.attach(&child).unwrap();
        registry
            .update(&child, |owner| {
                owner.lease.generation += 1;
                Ok(())
            })
            .unwrap();
        let mut current = OwnerPermit {
            registry_path: registry_path.clone(),
            thread_id: child.clone(),
            lease: old_owner.lease,
            run_id: None,
        };
        assert!(matches!(
            runtime.continue_goal(
                target,
                "retry".into(),
                RunConfig {
                    ownership: Some(current.clone()),
                    ..Default::default()
                }
            ),
            Err(runtime::RuntimeError::StaleOwnership { .. })
        ));
        current.lease = registry.attach(&child).unwrap().lease;
        if failure == "error-after-restart" {
            (runtime, bus) = make_runtime();
            runtime.restore_thread_goal(inherited.clone()).unwrap();
        }
        events = bus.subscribe();
        assert_eq!(
            runtime
                .continue_goal(
                    target,
                    "Retry after provider recovery".into(),
                    RunConfig {
                        ownership: Some(current),
                        project_root: Some(directory.path().to_path_buf()),
                        ..Default::default()
                    }
                )
                .unwrap(),
            target
        );
        model.entered.notified().await;
        model.release.notify_one();
        loop {
            if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to, .. }) =
                events.recv().await.unwrap().kind
                && run_id == target.to_string()
            {
                if to == AgentRunPhase::Waiting {
                    break;
                }
                assert!(
                    !matches!(
                        to,
                        AgentRunPhase::Done | AgentRunPhase::Error | AgentRunPhase::Stopped
                    ),
                    "unexpected {to:?}"
                );
            }
        }
        let requests = model.script.observed().await;
        assert_eq!(requests.len(), 2);
        let text = serde_json::to_string(&requests[1]).unwrap();
        assert_eq!(text.matches("[evorch escalation").count(), 1);
        assert!(text.contains("Retry after provider recovery"));
        assert_eq!(
            runtime.inspect_agent(target).unwrap().role_name,
            Role::Orchestrator.name()
        );
        assert_eq!(
            runtime
                .inspect_agent(target)
                .unwrap()
                .workspace
                .unwrap()
                .active_root,
            Some(retained_path.clone())
        );
        assert_eq!(
            std::fs::read_to_string(retained_path.join("keep.txt")).unwrap(),
            "pending handoff changes"
        );
        let resumed_goal = runtime.thread_goal(&child).unwrap();
        assert_eq!(resumed_goal.goal_id, inherited.goal_id);
        assert_eq!(
            resumed_goal.usage.model_requests,
            inherited.usage.model_requests + 1
        );
        runtime.stop(target, runtime::StopScope::SelfOnly).unwrap();
        assert_eq!(runtime.wait(target).await.unwrap(), AgentRunPhase::Stopped);
    }
}

#[tokio::test]
async fn goal_created_after_pending_restart_keeps_host_request_separate_from_the_memo() {
    let directory = tempfile::tempdir().unwrap();
    let storage_config = storage::StorageConfig {
        db_path: directory.path().join("events.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(storage_config.clone()).unwrap();
    let model = Arc::new(EscalationAdmissionModel {
        script: ScriptedModel::new([
            Ok(escalation_response()),
            Ok(tool_response(
                "goal",
                "create_goal",
                json!({"objective":"Coordinate requested investigation", "criteria":["Evidence is reviewed"]}),
            )),
            Ok(text_response("continuation", FinishReason::Stop)),
        ]),
        entered: Notify::new(),
        release: Notify::new(),
        failure: std::sync::atomic::AtomicU8::new(1),
    });
    let model_gate = Arc::new(Notify::new());
    model
        .script
        .gate_key("[evorch escalation", model_gate.clone())
        .await;
    let make_runtime = || {
        let bus = Arc::new(EventBus::new(256));
        (
            AgentRuntime::new(
                bus.clone(),
                Arc::new(ToolExecutor::new(bus.clone())),
                model.clone(),
            )
            .with_run_store(runtime::RunStore::open(&storage_config, storage.handle()).unwrap()),
            bus,
        )
    };
    let (runtime, bus) = make_runtime();
    let mut events = bus.subscribe();
    let original = "Research the requested options under the original agreed constraints";
    let source = runtime
        .delegate_chat(
            "source",
            Role::Worker,
            original.into(),
            RunConfig::default(),
        )
        .unwrap();
    let (target, _) = events_through_escalation(&mut events, source).await;
    model.entered.notified().await;
    model.release.notify_one();
    assert!(runtime.wait(target).await.is_err());
    drop(runtime);
    let (runtime, bus) = make_runtime();
    events = bus.subscribe();
    let child = event_bus::escalation_thread_id(&target.to_string());
    assert_eq!(runtime.latest_chat_run(&child).unwrap(), Some(target));
    runtime
        .continue_goal(
            target,
            "Continue and preserve the agreed constraints".into(),
            RunConfig::default(),
        )
        .unwrap();
    model.entered.notified().await;
    model.release.notify_one();
    model.script.wait_for_request(1).await;
    model_gate.notify_one();
    model.script.wait_for_request(2).await;
    let goal = runtime.thread_goal(&child).unwrap();
    assert!(goal.original_request.starts_with(original));
    assert!(
        goal.original_request
            .contains("Continue and preserve the agreed constraints")
    );
    assert!(!goal.original_request.contains("[evorch escalation"));
    assert!(!goal.original_request.contains("依存関係の更新"));
    runtime
        .set_goal_checks_paused(&child, &goal.goal_id, true)
        .unwrap();
    model_gate.notify_one();
    loop {
        if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
            run_id,
            to: AgentRunPhase::Waiting,
            ..
        }) = events.recv().await.unwrap().kind
            && run_id == target.to_string()
        {
            break;
        }
    }
    runtime.stop(target, runtime::StopScope::SelfOnly).unwrap();
    assert_eq!(runtime.wait(target).await.unwrap(), AgentRunPhase::Stopped);
}
