mod support;

use std::sync::Arc;

use agents::Role;
use event_bus::{AgentRunPhase, EventBus, EventKind, ToolEvent};
use providers::FinishReason;
use runtime::{AgentRuntime, RunConfig};
use sandbox::{ApprovalMode, ApprovalPolicy, DirectSandbox};
use serde_json::json;
use tools::ToolExecutor;

use support::{ScriptedModel, collect_events, drain_events, text_response, tool_response};

fn runtime_with(model: ScriptedModel) -> (AgentRuntime, Arc<EventBus>) {
    let bus = Arc::new(EventBus::new(64));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    (
        AgentRuntime::new(Arc::clone(&bus), executor, Arc::new(model)),
        bus,
    )
}

/// web ツール登録済みで standard(OnRequest) ポリシー・承認ゲート未設定の
/// executor を持つランタイムを生成する (AC4 / AC6 のループレベル検証用)。
fn web_runtime_with(model: ScriptedModel) -> (AgentRuntime, Arc<EventBus>) {
    let bus = Arc::new(EventBus::new(64));
    let mut executor = ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    )
    .with_web_tools()
    .expect("NetworkGuard 初期化");
    executor.set_policy(ApprovalPolicy::standard(ApprovalMode::OnRequest));
    (
        AgentRuntime::new(Arc::clone(&bus), Arc::new(executor), Arc::new(model)),
        bus,
    )
}

#[tokio::test]
async fn orchestrator_edit_is_denied_without_tool_started() {
    // Given
    let (runtime, bus) = runtime_with(ScriptedModel::new([
        Ok(tool_response(
            "edit-1",
            "edit",
            json!({ "path": "ignored", "new_string": "x" }),
        )),
        Ok(text_response("finished", FinishReason::Stop)),
    ]));
    let mut events = bus.subscribe();

    // When
    let run_id = runtime.delegate_background(
        Role::Orchestrator,
        "coordinate".to_string(),
        RunConfig::default(),
    );
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
    let events = collect_events(&mut events, 5).await;

    // Then
    assert!(!events.iter().any(|event| matches!(
        &event.kind,
        EventKind::Tool(ToolEvent::ToolStarted { tool_name, .. }) if tool_name == "edit"
    )));
    assert_eq!(
        runtime
            .inspect_agent(run_id)
            .expect("run exists")
            .message_count,
        4
    );
}

#[tokio::test]
async fn worker_edit_emits_started_and_completed() {
    // Given
    let directory = tempfile::tempdir().expect("temp directory");
    let path = directory.path().join("worker.txt");
    let (runtime, bus) = runtime_with(ScriptedModel::new([
        Ok(tool_response(
            "edit-2",
            "edit",
            json!({ "path": path, "new_string": "written" }),
        )),
        Ok(text_response("finished", FinishReason::Stop)),
    ]));
    let mut events = bus.subscribe();

    // When
    let run_id =
        runtime.delegate_background(Role::Worker, "edit".to_string(), RunConfig::default());
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
    let events = collect_events(&mut events, 7).await;

    // Then
    assert!(events.iter().any(|event| matches!(&event.kind, EventKind::Tool(ToolEvent::ToolStarted { tool_name, call_id, .. }) if tool_name == "edit" && call_id == "edit-2")));
    assert!(events.iter().any(|event| matches!(&event.kind, EventKind::Tool(ToolEvent::ToolCompleted { tool_name, call_id, is_error: false, .. }) if tool_name == "edit" && call_id == "edit-2")));
}

#[tokio::test]
async fn explorer_shell_is_denied_without_execution() {
    // Given
    let (runtime, bus) = runtime_with(ScriptedModel::new([
        Ok(tool_response(
            "shell-1",
            "shell",
            json!({ "command": "false" }),
        )),
        Ok(text_response("finished", FinishReason::Stop)),
    ]));
    let mut events = bus.subscribe();

    // When
    let run_id =
        runtime.delegate_background(Role::Explorer, "inspect".to_string(), RunConfig::default());
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
    let events = collect_events(&mut events, 5).await;

    // Then
    assert!(!events.iter().any(|event| matches!(&event.kind, EventKind::Tool(ToolEvent::ToolStarted { tool_name, .. }) if tool_name == "shell")));
}

#[tokio::test]
async fn orchestrator_web_fetch_passes_role_gate_but_is_denied_without_approval_gate() {
    // Given
    let (runtime, bus) = web_runtime_with(ScriptedModel::new([
        Ok(tool_response(
            "fetch-1",
            "web_fetch",
            json!({ "url": "https://example.invalid/" }),
        )),
        Ok(text_response("finished", FinishReason::Stop)),
    ]));
    let mut events = bus.subscribe();

    // When
    let run_id = runtime.delegate_background(
        Role::Orchestrator,
        "fetch".to_string(),
        RunConfig::default(),
    );
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
    let events = drain_events(&mut events).await;

    // Then: role gate を通過して ToolStarted まで到達するが、承認ゲート未設定のため
    // execute 前に拒否されネットワーク I/O は発生しない (AC6)。
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        EventKind::Tool(ToolEvent::ToolStarted { tool_name, call_id, .. })
            if tool_name == "web_fetch" && call_id == "fetch-1"
    )));
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        EventKind::Tool(ToolEvent::ExecutionDenied { tool_name, call_id, reason })
            if tool_name == "web_fetch" && call_id == "fetch-1" && reason.contains("承認ゲート")
    )));
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        EventKind::Tool(ToolEvent::ToolCompleted { tool_name, call_id, is_error: true, .. })
            if tool_name == "web_fetch" && call_id == "fetch-1"
    )));
}

#[tokio::test]
async fn orchestrator_web_search_is_denied_without_tool_started() {
    // Given
    let (runtime, bus) = web_runtime_with(ScriptedModel::new([
        Ok(tool_response(
            "search-1",
            "web_search",
            json!({ "query": "ignored" }),
        )),
        Ok(text_response("finished", FinishReason::Stop)),
    ]));
    let mut events = bus.subscribe();

    // When
    let run_id = runtime.delegate_background(
        Role::Orchestrator,
        "search".to_string(),
        RunConfig::default(),
    );
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
    let events = drain_events(&mut events).await;

    // Then: web_search は Orchestrator の capability 外のため executor 到達前に
    // 拒否され、ToolStarted は発行されない。
    assert!(!events.iter().any(|event| matches!(
        &event.kind,
        EventKind::Tool(ToolEvent::ToolStarted { tool_name, .. }) if tool_name == "web_search"
    )));
}

#[tokio::test]
async fn librarian_web_search_reaches_executor() {
    // Given
    let (runtime, bus) = web_runtime_with(ScriptedModel::new([
        Ok(tool_response("search-2", "web_search", json!({}))),
        Ok(text_response("finished", FinishReason::Stop)),
    ]));
    let mut events = bus.subscribe();

    // When
    let run_id =
        runtime.delegate_background(Role::Librarian, "search".to_string(), RunConfig::default());
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
    let events = drain_events(&mut events).await;

    // Then: role gate を通過して executor に到達する (AC4)。{} は query 必須の
    // スキーマ違反のため、承認・ネットワーク I/O なしでエラー完了する。
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        EventKind::Tool(ToolEvent::ToolStarted { tool_name, call_id, .. })
            if tool_name == "web_search" && call_id == "search-2"
    )));
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        EventKind::Tool(ToolEvent::ToolCompleted { tool_name, call_id, is_error: true, .. })
            if tool_name == "web_search" && call_id == "search-2"
    )));
}

#[tokio::test]
async fn explorer_web_fetch_is_denied_without_tool_started() {
    // Given
    let (runtime, bus) = web_runtime_with(ScriptedModel::new([
        Ok(tool_response(
            "fetch-2",
            "web_fetch",
            json!({ "url": "https://example.invalid/" }),
        )),
        Ok(text_response("finished", FinishReason::Stop)),
    ]));
    let mut events = bus.subscribe();

    // When
    let run_id =
        runtime.delegate_background(Role::Explorer, "fetch".to_string(), RunConfig::default());
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
    let events = drain_events(&mut events).await;

    // Then: web_fetch は Explorer の capability 外のため executor 到達前に
    // 拒否され、ToolStarted は発行されない。
    assert!(!events.iter().any(|event| matches!(
        &event.kind,
        EventKind::Tool(ToolEvent::ToolStarted { tool_name, .. }) if tool_name == "web_fetch"
    )));
}
