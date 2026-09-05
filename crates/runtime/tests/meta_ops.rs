mod support;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agents::Role;
use config::{CompactionConfig, SummarizerKind};
use event_bus::{
    AgentRunPhase, CompactionEvent, CompactionReason, Event, EventBus, EventKind, LifecycleEvent,
};
use providers::{ContentBlock, FinishReason, ToolResultContent};
use runtime::workspace::{Project, WorktreeManager};
use runtime::{AgentRuntime, EscalationMemo, IsolatedMounts, RunConfig, WorkspaceMode};
use sandbox::DirectSandbox;
use serde_json::json;
use tokio::time::{Duration, sleep, timeout};
use tools::ToolExecutor;

use support::{
    ScriptedModel, drain_events, init_git_repo, recording_factory, text_response, tool_response,
};

fn runtime_with(model: Arc<ScriptedModel>) -> AgentRuntime {
    let bus = Arc::new(EventBus::new(128));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    AgentRuntime::new(bus, executor, model)
}

fn runtime_with_workspace(
    repo: &Path,
    model: Arc<ScriptedModel>,
) -> (AgentRuntime, Arc<Mutex<Vec<IsolatedMounts>>>) {
    let bus = Arc::new(EventBus::new(128));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    let manager =
        WorktreeManager::new(Project::new(repo.to_path_buf()).expect("git リポジトリを検証できる"));
    let (factory, mounts) = recording_factory();
    (
        AgentRuntime::with_workspace_context(bus, executor, model, manager, factory),
        mounts,
    )
}

fn tool_result(messages: &[providers::Message], call_id: &str) -> Option<(String, bool)> {
    messages.iter().find_map(|message| {
        message.content.iter().find_map(|block| match block {
            ContentBlock::ToolResult {
                tool_call_id,
                content,
                is_error,
            } if tool_call_id == call_id => content.first().map(|item| match item {
                ToolResultContent::Text { text } => (text.clone(), *is_error),
            }),
            ContentBlock::Text { .. }
            | ContentBlock::Reasoning { .. }
            | ContentBlock::ToolUse { .. }
            | ContentBlock::ToolResult { .. } => None,
        })
    })
}

#[tokio::test]
async fn orchestrator_dispatches_remaining_runtime_meta_operations() {
    // Given: foreground delegate、一覧・検査、cancel、compact、finish を順に要求する Orchestrator
    let model = Arc::new(ScriptedModel::new([]));
    model
        .add_keyed(
            "META",
            [
                Ok(tool_response(
                    "delegate",
                    "delegate",
                    json!({ "role": "worker", "prompt": "SYNC" }),
                )),
                Ok(tool_response(
                    "spawn-interactive",
                    "delegate_background",
                    json!({
                        "role": "explorer",
                        "prompt": "HOLD",
                        "interactive": true,
                        "name": "held-explorer"
                    }),
                )),
                Ok(tool_response("list", "list_agents", json!({}))),
                Ok(tool_response(
                    "inspect",
                    "inspect_agent",
                    json!({ "run_id": "run-3" }),
                )),
                Ok(tool_response(
                    "cancel",
                    "cancel",
                    json!({ "run_id": "run-3" }),
                )),
                Ok(tool_response("compact", "compact", json!({}))),
                Ok(tool_response(
                    "finish",
                    "finish",
                    json!({ "result": "meta done" }),
                )),
            ],
        )
        .await;
    model
        .add_keyed("SYNC", [Ok(text_response("done", FinishReason::Stop))])
        .await;
    model
        .add_keyed("HOLD", [Ok(text_response("waiting", FinishReason::Stop))])
        .await;
    let runtime = runtime_with(Arc::clone(&model));

    // When: Orchestrator を finish まで実行する
    let orchestrator =
        runtime.delegate_background(Role::Orchestrator, "META".to_string(), RunConfig::default());
    assert_eq!(runtime.wait(orchestrator).await, Ok(AgentRunPhase::Done));
    assert_eq!(
        runtime.wait(runtime.list_agents()[2].run_id).await,
        Ok(AgentRunPhase::Error)
    );

    // Then: 各 ToolResult が実 API の成功値、または圧縮不可能 context を示す
    // CompactionError::NothingToCompact の診断を返す
    let observed = model.observed().await;
    let final_turn = observed
        .iter()
        .rfind(|messages| {
            messages.first().is_some_and(|message| {
                message
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Text { text } if text == "META"))
            })
        })
        .expect("orchestrator final model turn");
    assert_eq!(
        tool_result(final_turn, "delegate"),
        Some(("Done".to_string(), false))
    );
    assert_eq!(
        tool_result(final_turn, "spawn-interactive"),
        Some(("run-3".to_string(), false))
    );
    let (list, list_error) = tool_result(final_turn, "list").expect("list result");
    assert!(!list_error);
    let summaries: serde_json::Value = serde_json::from_str(&list).expect("list JSON");
    assert_eq!(summaries.as_array().expect("summary array").len(), 3);
    // Then: 一覧の identity 項目は meta-op 経由の委譲でも反映される
    assert_eq!(summaries[2]["name"], json!("held-explorer"));
    assert_eq!(summaries[2]["role_name"], json!("Explorer"));
    assert!(
        summaries[2]["model"]
            .as_str()
            .is_some_and(|model| !model.is_empty())
    );
    assert_eq!(summaries[0]["name"], json!("Orchestrator"));
    let (inspection, inspect_error) = tool_result(final_turn, "inspect").expect("inspect result");
    assert!(!inspect_error);
    let inspection: serde_json::Value = serde_json::from_str(&inspection).expect("inspection JSON");
    assert_eq!(inspection["run_id"], json!(3));
    assert_eq!(inspection["workspace"]["mode"], json!("shared"));
    assert_eq!(inspection["workspace"]["merge_mode"], json!("branch"));
    assert_eq!(
        tool_result(final_turn, "cancel"),
        Some(("cancelled".to_string(), false))
    );
    assert_eq!(
        tool_result(final_turn, "compact"),
        Some((
            "compaction has no safe message range to replace".to_string(),
            true
        ))
    );
}

async fn compaction_events_until_done(
    receiver: &mut event_bus::EventReceiver,
    run_id: &str,
) -> Vec<Event> {
    let mut compacted = Vec::new();
    timeout(Duration::from_secs(5), async {
        loop {
            let event = receiver.recv().await.expect("event receiver remains open");
            if matches!(&event.kind, EventKind::Compaction(_)) {
                compacted.push(event.clone());
            }
            if matches!(
                &event.kind,
                EventKind::Lifecycle(event_bus::LifecycleEvent::BackgroundTaskCompleted { task_id })
                    if task_id == run_id
            ) {
                return;
            }
        }
    })
    .await
    .expect("run completion event timeout");
    compacted
}

#[tokio::test]
async fn compact_meta_op_compacts_context_and_emits_agent_reason_event() {
    // Given: 大きな prompt で始まり compact → finish を要求する Orchestrator。
    // window を十分大きくして自動圧縮を発火させず、keep_recent_tokens を 1 にして
    // Agent 起因の cut が成立するようにする
    let model = Arc::new(ScriptedModel::new([
        Ok(text_response(&"old answer ".repeat(40), FinishReason::Stop)),
        Ok(tool_response("compact-1", "compact", json!({}))),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let bus = Arc::new(EventBus::new(128));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    let runtime = AgentRuntime::new(Arc::clone(&bus), executor, model.clone()).with_compaction(
        CompactionConfig {
            context_window_tokens: 1_000_000,
            keep_recent_tokens: 1,
            max_summary_bytes: 1_024,
            summarizer: SummarizerKind::Structural,
            ..CompactionConfig::default()
        },
    );
    let mut receiver = bus.subscribe();

    // When: Orchestrator を finish まで実行する
    let orchestrator = runtime.delegate_background(
        Role::Orchestrator,
        "large prompt ".repeat(40),
        RunConfig {
            interactive: true,
            ..RunConfig::default()
        },
    );
    timeout(Duration::from_secs(5), async {
        loop {
            if runtime
                .inspect_agent(orchestrator)
                .expect("run remains inspectable")
                .phase
                == AgentRunPhase::Waiting
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("waiting phase timeout");
    runtime
        .send_message(orchestrator, "resume".to_string())
        .expect("waiting run resumes");
    assert_eq!(runtime.wait(orchestrator).await, Ok(AgentRunPhase::Done));
    let events = compaction_events_until_done(&mut receiver, &orchestrator.to_string()).await;

    // Then: compact は checkpoint ID を含む JSON の成功結果を返し、bus には
    // Agent reason の CompactionEvent が 1 件だけ流れる
    let observed = model.observed().await;
    let final_turn = observed.last().expect("orchestrator final model turn");
    let (content, is_error) = tool_result(final_turn, "compact-1").expect("compact result");
    assert!(!is_error, "compact failed: {content}");
    let payload: serde_json::Value = serde_json::from_str(&content).expect("compact JSON");
    assert!(
        payload["checkpoint_id"]
            .as_str()
            .is_some_and(|id| id.starts_with("ckpt-"))
    );
    assert!(payload["estimated_tokens_before"].is_u64());
    assert!(payload["estimated_tokens_after"].is_u64());
    assert_eq!(payload["still_above_threshold"], json!(false));
    assert_eq!(payload["reason"], json!("agent"));
    assert_eq!(events.len(), 1);
    assert!(matches!(
        events[0].kind,
        EventKind::Compaction(CompactionEvent::Compacted {
            reason: CompactionReason::Agent,
            ..
        })
    ));
}

#[tokio::test]
async fn delegate_background_accepts_isolated_workspace_mode() {
    // Given: isolated workspace を要求する delegate_background を返す Orchestrator
    let (_temp, repo) = init_git_repo();
    let model = Arc::new(ScriptedModel::new([]));
    model
        .add_keyed(
            "META",
            [
                Ok(tool_response(
                    "delegate-isolated",
                    "delegate_background",
                    json!({
                        "role": "worker",
                        "prompt": "ISOLATED",
                        "interactive": true,
                        "workspace_mode": "isolated"
                    }),
                )),
                Ok(tool_response(
                    "finish",
                    "finish",
                    json!({ "result": "done" }),
                )),
            ],
        )
        .await;
    let (runtime, mounts) = runtime_with_workspace(&repo, Arc::clone(&model));

    // When: meta op を経由して child を生成する
    let parent =
        runtime.delegate_background(Role::Orchestrator, "META".to_string(), RunConfig::default());
    assert_eq!(runtime.wait(parent).await, Ok(AgentRunPhase::Done));
    let child = runtime.list_agents()[1].run_id;
    timeout(Duration::from_secs(5), async {
        while mounts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
        {
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("isolated child workspace setup が期限内に完了する");

    // Then: child inspection が isolated mode を報告する
    assert_eq!(
        runtime
            .inspect_agent(child)
            .expect("child を inspection できる")
            .workspace
            .expect("workspace inspection が常にある")
            .mode,
        WorkspaceMode::Isolated
    );
    runtime
        .cancel(child)
        .expect("interactive child を cancel できる");
    assert_eq!(runtime.wait(child).await, Ok(AgentRunPhase::Error));
}

#[tokio::test]
async fn delegate_background_rejects_unknown_workspace_mode() {
    // Given: 未知の workspace_mode を返す Orchestrator
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response(
            "invalid-workspace",
            "delegate_background",
            json!({ "role": "worker", "prompt": "unused", "workspace_mode": "hybrid" }),
        )),
        Ok(tool_response(
            "finish",
            "finish",
            json!({ "result": "done" }),
        )),
    ]));
    let runtime = runtime_with(Arc::clone(&model));

    // When: meta op を実行する
    let parent =
        runtime.delegate_background(Role::Orchestrator, "META".to_string(), RunConfig::default());
    assert_eq!(runtime.wait(parent).await, Ok(AgentRunPhase::Done));

    // Then: ToolResult error で child を spawn しない
    let observed = model.observed().await;
    let final_turn = observed.last().expect("orchestrator final model turn");
    assert!(matches!(
        tool_result(final_turn, "invalid-workspace"),
        Some((_, true))
    ));
    assert_eq!(runtime.list_agents().len(), 1);
}

#[tokio::test]
async fn finish_without_goal_gate_keeps_legacy_immediate_accept() {
    // Given: goal gate 未接続の runtime で finish を要求する Orchestrator
    let model = Arc::new(ScriptedModel::new([Ok(tool_response(
        "finish",
        "finish",
        json!({ "result": "legacy accept" }),
    ))]));
    let runtime = runtime_with(Arc::clone(&model));

    // When: finish meta-op で run を終端させる
    let run_id =
        runtime.delegate_background(Role::Orchestrator, "META".to_string(), RunConfig::default());

    // Then: gate なしでは finish は即時に受理され、run が Done になる。
    // finish の ToolResult は run 終端で model に返らないため、観測可能な契約は
    // result の公開 (run_result) と、追加 model 呼び出しがないことである
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
    assert_eq!(
        runtime.run_result(run_id),
        Ok(Some("legacy accept".to_string()))
    );
    assert_eq!(model.observed().await.len(), 1);
}

#[tokio::test]
async fn invalid_meta_arguments_return_error_and_run_continues() {
    // Given: 不正引数・未知 run・未知 role の後に finish を要求する Orchestrator
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response("bad-wait", "wait", json!({ "run_id": 9 }))),
        Ok(tool_response(
            "unknown-message",
            "send_message",
            json!({ "run_id": "run-999", "message": "hello" }),
        )),
        Ok(tool_response(
            "unknown-inspect",
            "inspect_agent",
            json!({ "run_id": "run-999" }),
        )),
        Ok(tool_response(
            "unknown-cancel",
            "cancel",
            json!({ "run_id": "run-999" }),
        )),
        Ok(tool_response(
            "unknown-role",
            "delegate_background",
            json!({ "role": "unknown", "prompt": "unused" }),
        )),
        Ok(tool_response(
            "finish",
            "finish",
            json!({ "result": "recovered" }),
        )),
    ]));
    let runtime = runtime_with(Arc::clone(&model));

    // When: run を終端まで実行する
    let run_id = runtime.delegate_background(
        Role::Orchestrator,
        "INVALID".to_string(),
        RunConfig::default(),
    );

    // Then: 不正引数は ToolResult error になり、次の turn の finish で Done になる
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
    let observed = model.observed().await;
    let second_turn = observed.get(1).expect("second model turn");
    assert!(matches!(
        tool_result(second_turn, "bad-wait"),
        Some((_, true))
    ));
    let final_turn = observed.last().expect("final model turn");
    for call_id in [
        "unknown-message",
        "unknown-inspect",
        "unknown-cancel",
        "unknown-role",
    ] {
        assert!(matches!(tool_result(final_turn, call_id), Some((_, true))));
    }
    assert_eq!(runtime.list_agents().len(), 1);
}

#[tokio::test]
async fn escalate_records_memo_and_terminates_run_done() {
    // Given: 有効な引数の escalate を 1 件要求する Worker root run
    // (バスは run 開始前に購読しておく)
    let model = Arc::new(ScriptedModel::new([Ok(tool_response(
        "esc",
        "escalate",
        json!({
            "original_request": "依存関係の更新を Direct run で完了する",
            "escalation_reason": "編集失敗が連続し単独では解消できない",
            "findings": ["cargo test が失敗する"],
            "files_touched": ["crates/runtime/src/lib.rs"],
            "blockers": ["権限が不足している"],
            "workspace_state": "M crates/runtime/src/lib.rs",
            "suggested_next": "Orchestrator で担当を分割する"
        }),
    ))]));
    let bus = Arc::new(EventBus::new(128));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    let runtime = AgentRuntime::new(Arc::clone(&bus), executor, model.clone());
    let mut receiver = bus.subscribe();

    // When: run を実行して終端を待つ
    let run_id =
        runtime.delegate_background(Role::Worker, "ESCALATE".to_string(), RunConfig::default());
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));

    // Then: メモは呼び出し元 run を source_run_id として記録され、
    // run は 1 回のモデル呼び出しで Done ("escalated") に終端する
    assert_eq!(
        runtime.escalation_memo(run_id),
        Some(EscalationMemo {
            source_run_id: run_id,
            original_request: "依存関係の更新を Direct run で完了する".to_string(),
            findings: vec!["cargo test が失敗する".to_string()],
            files_touched: vec![PathBuf::from("crates/runtime/src/lib.rs")],
            blockers: vec!["権限が不足している".to_string()],
            workspace_state: "M crates/runtime/src/lib.rs".to_string(),
            escalation_reason: "編集失敗が連続し単独では解消できない".to_string(),
            suggested_next: "Orchestrator で担当を分割する".to_string(),
        })
    );
    assert_eq!(model.observed().await.len(), 1);
    let events = drain_events(&mut receiver).await;
    assert!(
        events.iter().any(|event| matches!(
            &event.kind,
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                to: AgentRunPhase::Done,
                reason: Some(reason),
                ..
            }) if reason == "escalated"
        )),
        "escalated 理由の Done 遷移イベントが欠落: {events:?}"
    );
}

#[tokio::test]
async fn escalate_with_invalid_args_is_rejected_and_run_continues() {
    // Given: 必須フィールド欠落・モデル供給 source_run_id・空 escalation_reason の
    // escalate を順に要求した後、自然 Stop する Worker root run
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response(
            "bad-missing",
            "escalate",
            json!({ "original_request": "元の依頼" }),
        )),
        Ok(tool_response(
            "bad-run-id",
            "escalate",
            json!({
                "original_request": "元の依頼",
                "escalation_reason": "理由",
                "source_run_id": "run-99"
            }),
        )),
        Ok(tool_response(
            "bad-empty",
            "escalate",
            json!({ "original_request": "元の依頼", "escalation_reason": "" }),
        )),
        Ok(text_response("fallback done", FinishReason::Stop)),
    ]));
    let runtime = runtime_with(Arc::clone(&model));

    // When: run を終端まで実行する
    let run_id =
        runtime.delegate_background(Role::Worker, "INVALID".to_string(), RunConfig::default());

    // Then: 3 件とも fail-closed の error result になり、メモは記録されず
    // run は自然 Stop で Done に至る
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
    assert_eq!(runtime.escalation_memo(run_id), None);
    let observed = model.observed().await;
    assert_eq!(observed.len(), 4);
    let (missing_text, missing_error) =
        tool_result(&observed[1], "bad-missing").expect("missing result");
    assert!(missing_error);
    assert!(
        missing_text.contains("escalation_reason"),
        "欠落フィールド識別子が欠落: {missing_text}"
    );
    let (runid_text, runid_error) = tool_result(&observed[2], "bad-run-id").expect("run-id result");
    assert!(runid_error);
    assert!(
        runid_text.contains("source_run_id"),
        "モデル供給 source_run_id の拒否識別子が欠落: {runid_text}"
    );
    let (empty_text, empty_error) = tool_result(&observed[3], "bad-empty").expect("empty result");
    assert!(empty_error);
    assert!(
        empty_text.contains("escalation_reason"),
        "空 escalation_reason の拒否識別子が欠落: {empty_text}"
    );
}

#[tokio::test]
async fn orchestrator_cannot_escalate() {
    // Given: escalate を要求する Orchestrator (capability 外)
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response(
            "esc-orch",
            "escalate",
            json!({ "original_request": "元の依頼", "escalation_reason": "理由" }),
        )),
        Ok(text_response("orchestrator done", FinishReason::Stop)),
    ]));
    let runtime = runtime_with(Arc::clone(&model));

    // When: run を終端まで実行する
    let run_id =
        runtime.delegate_background(Role::Orchestrator, "ORCH".to_string(), RunConfig::default());

    // Then: capability 拒否の error result になり、メモは記録されず
    // run は継続して自然 Stop で Done に至る
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
    assert_eq!(runtime.escalation_memo(run_id), None);
    let observed = model.observed().await;
    assert_eq!(observed.len(), 2);
    let (content, is_error) = tool_result(&observed[1], "esc-orch").expect("escalate result");
    assert!(is_error);
    assert!(
        content.contains("escalate"),
        "capability 拒否にはツール名が含まれる: {content}"
    );
}
