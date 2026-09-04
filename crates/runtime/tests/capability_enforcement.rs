mod support;

use std::sync::Arc;

use agents::{NetworkAccess, Role};
use event_bus::{AgentRunPhase, EventBus, EventKind, ToolEvent};
use providers::FinishReason;
use runtime::{AgentRuntime, RunConfig};
use sandbox::DirectSandbox;
use serde_json::json;
use tools::ToolExecutor;

use support::{
    ScriptedModel, collect_events, drain_events, spawn_approval_responder,
    spawn_run_scoped_approval_responder, text_response, tool_response,
};

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

/// web ツール登録済みで production 同等 (allow_all ポリシー)・承認ゲート未設定の
/// executor を持つランタイムを生成する (AC2 / AC4 / AC6 のループレベル検証用)。
/// 承認は loop 側ゲートが担うため executor 側の設定は不要である。
fn web_runtime_with(model: ScriptedModel) -> (AgentRuntime, Arc<EventBus>) {
    let bus = Arc::new(EventBus::new(64));
    let executor = ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    )
    .with_web_tools()
    .expect("NetworkGuard 初期化");
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
async fn orchestrator_web_fetch_default_session_is_denied_before_executor() {
    // Given: session の NetworkAccess が既定 (Denied) の run
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

    // Then: session 層の拒否が executor 到達前に行われ、承認要求も ToolStarted も
    // 発行されない (AC2)。
    assert!(!events.iter().any(|event| matches!(
        &event.kind,
        EventKind::Tool(ToolEvent::ApprovalRequested { .. })
    )));
    assert!(!events.iter().any(|event| matches!(
        &event.kind,
        EventKind::Tool(ToolEvent::ToolStarted { tool_name, .. }) if tool_name == "web_fetch"
    )));
}

#[tokio::test]
async fn orchestrator_web_fetch_session_opt_in_executes_only_after_approval() {
    // Given: session の NetworkAccess が OptIn の run と、承認する応答者
    let (runtime, bus) = web_runtime_with(ScriptedModel::new([
        Ok(tool_response("fetch-1", "web_fetch", json!({}))),
        Ok(text_response("finished", FinishReason::Stop)),
    ]));
    let mut events = bus.subscribe();
    let responder = spawn_approval_responder(Arc::clone(&bus), bus.subscribe(), true);

    // When
    let run_id = runtime.delegate_background(
        Role::Orchestrator,
        "fetch".to_string(),
        RunConfig {
            network_access: NetworkAccess::OptIn,
            ..RunConfig::default()
        },
    );
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
    let events = drain_events(&mut events).await;

    // Then: 承認要求 (相関キーは run スコープの `{run_id}:{call_id}`) が
    // ToolStarted より前に発行され、承認後の実行は {} が
    // url 必須のスキーマ違反のためネットワーク I/O なしでエラー完了する (AC6)。
    let approval_position = events
        .iter()
        .position(|event| {
            matches!(
                &event.kind,
                EventKind::Tool(ToolEvent::ApprovalRequested { tool_name, call_id })
                    if tool_name == "web_fetch" && call_id == &format!("{run_id}:fetch-1")
            )
        })
        .expect("ApprovalRequested が発行される");
    let started_position = events
        .iter()
        .position(|event| {
            matches!(
                &event.kind,
                EventKind::Tool(ToolEvent::ToolStarted { tool_name, call_id, .. })
                    if tool_name == "web_fetch" && call_id == "fetch-1"
            )
        })
        .expect("承認後に ToolStarted が発行される");
    assert!(approval_position < started_position);
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        EventKind::Tool(ToolEvent::ToolCompleted { tool_name, call_id, is_error: true, .. })
            if tool_name == "web_fetch" && call_id == "fetch-1"
    )));
    responder.await.expect("応答タスクが完了するはずです");
}

#[tokio::test]
async fn orchestrator_web_fetch_session_opt_in_denied_approval_never_starts() {
    // Given: session の NetworkAccess が OptIn の run と、拒否する応答者
    let (runtime, bus) = web_runtime_with(ScriptedModel::new([
        Ok(tool_response(
            "fetch-1",
            "web_fetch",
            json!({ "url": "https://example.invalid/" }),
        )),
        Ok(text_response("finished", FinishReason::Stop)),
    ]));
    let mut events = bus.subscribe();
    let responder = spawn_approval_responder(Arc::clone(&bus), bus.subscribe(), false);

    // When
    let run_id = runtime.delegate_background(
        Role::Orchestrator,
        "fetch".to_string(),
        RunConfig {
            network_access: NetworkAccess::OptIn,
            ..RunConfig::default()
        },
    );
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
    let events = drain_events(&mut events).await;

    // Then: 承認 (相関キーは run スコープの `{run_id}:{call_id}`) は要求されるが、
    // 拒否されたため executor に到達しない (AC6)。
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        EventKind::Tool(ToolEvent::ApprovalRequested { tool_name, call_id })
            if tool_name == "web_fetch" && call_id == &format!("{run_id}:fetch-1")
    )));
    assert!(!events.iter().any(|event| matches!(
        &event.kind,
        EventKind::Tool(ToolEvent::ToolStarted { tool_name, .. }) if tool_name == "web_fetch"
    )));
    responder.await.expect("応答タスクが完了するはずです");
}

#[tokio::test]
async fn parallel_optin_runs_do_not_cross_accept_approval_resolutions() {
    // Given: 同一 EventBus を共有する 1 ランタイム上の 2 つの OptIn run が
    // 同一 model call_id "fetch-1" (run-local) で web_fetch を要求し、応答者は
    // run A の run スコープ相関キーだけを 1 回承認する (run B には応答しない)。
    let model = ScriptedModel::new([]);
    model
        .add_keyed(
            "RUN-A",
            [
                Ok(tool_response("fetch-1", "web_fetch", json!({}))),
                Ok(text_response("finished", FinishReason::Stop)),
            ],
        )
        .await;
    model
        .add_keyed(
            "RUN-B",
            [
                Ok(tool_response("fetch-1", "web_fetch", json!({}))),
                Ok(text_response("finished", FinishReason::Stop)),
            ],
        )
        .await;
    let (runtime, bus) = web_runtime_with(model);
    let mut events = bus.subscribe();
    let config = RunConfig {
        network_access: NetworkAccess::OptIn,
        ..RunConfig::default()
    };

    // When: 2 run を同一バス上で並列に起動し、run A の完了のみを待つ
    let run_a =
        runtime.delegate_background(Role::Orchestrator, "RUN-A".to_string(), config.clone());
    let run_b =
        runtime.delegate_background(Role::Orchestrator, "RUN-B".to_string(), config.clone());
    let responder =
        spawn_run_scoped_approval_responder(Arc::clone(&bus), bus.subscribe(), format!("{run_a}:"));
    assert_eq!(runtime.wait(run_a).await, Ok(AgentRunPhase::Done));
    let events = drain_events(&mut events).await;

    // Then: web_fetch の ToolStarted は run A の 1 件だけである。run B 宛ての
    // 承認解決は存在しないため run B が web_fetch を開始することはない。
    let started_count = events
        .iter()
        .filter(|event| {
            matches!(
                &event.kind,
                EventKind::Tool(ToolEvent::ToolStarted { tool_name, call_id, .. })
                    if tool_name == "web_fetch" && call_id == "fetch-1"
            )
        })
        .count();
    assert_eq!(
        started_count, 1,
        "run A 宛ての承認解決が他 run の gate にも受理されている (承認横取り)"
    );
    // 承認要求は run ごとに run スコープ相関キー (`{run_id}:{call_id}`) で発行される。
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        EventKind::Tool(ToolEvent::ApprovalRequested { tool_name, call_id })
            if tool_name == "web_fetch" && call_id == &format!("{run_a}:fetch-1")
    )));
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        EventKind::Tool(ToolEvent::ApprovalRequested { tool_name, call_id })
            if tool_name == "web_fetch" && call_id == &format!("{run_b}:fetch-1")
    )));
    // ToolStarted / ToolCompleted の call_id は生 call_id のまま (相関キー化は
    // 承認イベントのみで、ツールライフサイクルイベントは変更しない)。
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        EventKind::Tool(ToolEvent::ToolCompleted { tool_name, call_id, is_error: true, .. })
            if tool_name == "web_fetch" && call_id == "fetch-1"
    )));

    // run B は承認待ちのまま残るため cancel で停止させる (タスク漏れ防止)。
    assert_eq!(runtime.cancel(run_b), Ok(()));
    assert_eq!(runtime.wait(run_b).await, Ok(AgentRunPhase::Error));
    responder.await.expect("応答タスクが完了するはずです");
}

#[tokio::test]
async fn orchestrator_web_fetch_session_allowed_executes_without_prompt() {
    // Given: session の NetworkAccess が Allowed の run
    let (runtime, bus) = web_runtime_with(ScriptedModel::new([
        Ok(tool_response("fetch-1", "web_fetch", json!({}))),
        Ok(text_response("finished", FinishReason::Stop)),
    ]));
    let mut events = bus.subscribe();

    // When
    let run_id = runtime.delegate_background(
        Role::Orchestrator,
        "fetch".to_string(),
        RunConfig {
            network_access: NetworkAccess::Allowed,
            ..RunConfig::default()
        },
    );
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
    let events = drain_events(&mut events).await;

    // Then: 承認要求なしで実行が開始され、{} が url 必須のスキーマ違反のため
    // ネットワーク I/O なしでエラー完了する (AC2)。
    assert!(!events.iter().any(|event| matches!(
        &event.kind,
        EventKind::Tool(ToolEvent::ApprovalRequested { .. })
    )));
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        EventKind::Tool(ToolEvent::ToolStarted { tool_name, call_id, .. })
            if tool_name == "web_fetch" && call_id == "fetch-1"
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
    let run_id = runtime.delegate_background(
        Role::Librarian,
        "search".to_string(),
        RunConfig {
            network_access: NetworkAccess::Allowed,
            ..RunConfig::default()
        },
    );
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
