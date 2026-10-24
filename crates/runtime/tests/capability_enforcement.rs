mod support;

use std::sync::Arc;

use agents::Role;
use event_bus::{AgentRunPhase, EventBus, EventKind, ToolEvent};
use providers::FinishReason;
use runtime::{AgentRuntime, RunConfig};
use sandbox::{ApprovalPolicy, DirectSandbox, PolicyDecision};
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
    let mut executor = ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    )
    .with_web_tools()
    .expect("NetworkGuard 初期化");
    executor
        .set_policy(ApprovalPolicy::allow_all().with_override("web_fetch", PolicyDecision::Ask));
    let runtime = AgentRuntime::new(Arc::clone(&bus), Arc::new(executor), Arc::new(model));
    runtime.set_web_tools_enabled(true);
    (runtime, bus)
}

#[tokio::test]
async fn orchestrator_edit_is_denied_without_tool_started() {
    // Given
    let (runtime, bus) = runtime_with(ScriptedModel::new([
        Ok(tool_response(
            "edit-1",
            "write",
            json!({ "path": "ignored", "content": "x" }),
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
        EventKind::Tool(ToolEvent::ToolStarted { tool_name, .. }) if tool_name == "write"
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
            "write",
            json!({ "path": path, "content": "written" }),
        )),
        Ok(text_response("finished", FinishReason::Stop)),
    ]));
    let mut events = bus.subscribe();

    // When
    let run_id =
        runtime.delegate_background(Role::Worker, "write".to_string(), RunConfig::default());
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
    let events = drain_events(&mut events).await;

    // Then
    assert!(events.iter().any(|event| matches!(&event.kind, EventKind::Tool(ToolEvent::ToolStarted { tool_name, call_id, .. }) if tool_name == "write" && call_id == "edit-2")));
    assert!(events.iter().any(|event| matches!(&event.kind, EventKind::Tool(ToolEvent::ToolCompleted { tool_name, call_id, is_error: false, .. }) if tool_name == "write" && call_id == "edit-2")));
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
async fn disabled_web_tools_are_denied_before_executor() {
    // Given: Web tools are disabled for the runtime.
    let (runtime, bus) = web_runtime_with(ScriptedModel::new([
        Ok(tool_response(
            "fetch-1",
            "web_fetch",
            json!({ "url": "https://example.invalid/" }),
        )),
        Ok(text_response("finished", FinishReason::Stop)),
    ]));
    runtime.set_web_tools_enabled(false);
    let mut events = bus.subscribe();

    // When
    let run_id = runtime.delegate_background(
        Role::Orchestrator,
        "fetch".to_string(),
        RunConfig::default(),
    );
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
    let events = drain_events(&mut events).await;

    // Then: no approval or execution is started for a disabled Web tool.
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
async fn web_fetch_auto_review_audits_actual_url_and_verdict() {
    for (approve, should_start) in [(true, true), (false, false)] {
        let bus = Arc::new(EventBus::new(128));
        let executor = ToolExecutor::with_standard_tools(
            Arc::clone(&bus),
            Arc::new(DirectSandbox::new_unchecked()),
        )
        .with_web_tools()
        .expect("Web tools");
        let model = Arc::new(ScriptedModel::new([
            Ok(tool_response(
                "fetch-auto",
                "web_fetch",
                json!({"url":"http://127.0.0.1/"}),
            )),
            Ok(text_response(
                &format!(
                    r#"{{"approve":{approve},"reason":"reviewed","risk_level":"low","authorization_level":"medium"}}"#
                ),
                FinishReason::Stop,
            )),
            Ok(text_response("finished", FinishReason::Stop)),
        ]));
        let runtime = AgentRuntime::new(Arc::clone(&bus), Arc::new(executor), model.clone());
        let mut events = bus.subscribe();
        let run_id = runtime
            .delegate_chat(
                "review",
                Role::Orchestrator,
                "Read a public page".into(),
                RunConfig::default(),
            )
            .unwrap();
        assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
        let events = drain_events(&mut events).await;
        let audits: Vec<_> = events
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::Diagnostic(diag)
                    if diag.code == "tool_call_access"
                        && diag.call_id.as_deref() == Some("fetch-auto") =>
                {
                    Some(diag)
                }
                _ => None,
            })
            .collect();
        assert_eq!(audits.len(), 1);
        let audit: serde_json::Value = serde_json::from_str(&audits[0].detail).expect("audit JSON");
        assert_eq!(audit["input"]["url"], "http://127.0.0.1/");
        assert_eq!(audit["decision"], if approve { "allow" } else { "deny" });
        assert_eq!(audit["reviewer"], "auto_review");
        assert_eq!(
            events.iter().any(|event| matches!(
                &event.kind, EventKind::Tool(ToolEvent::ToolStarted { tool_name, call_id, .. })
                    if tool_name == "web_fetch" && call_id == "fetch-auto"
            )),
            should_start
        );
        assert!(!events.iter().any(|event| matches!(
            &event.kind,
            EventKind::Tool(ToolEvent::ApprovalRequested { .. })
        )));
        let calls = model.observed().await;
        assert!(calls.iter().any(|messages| messages.iter().any(|message|
            message.content.iter().any(|content| matches!(content, providers::ContentBlock::Text { text } if text.contains("http://127.0.0.1/") && text.contains("real_user_requests"))
        ))));
    }
}

// Given: per-call automatic review / When: the reviewer approves at critical risk /
// Then: the user decides through the approval gate, and only their answer is audited.
#[tokio::test]
async fn web_fetch_critical_approval_is_handed_to_the_user() {
    for approved in [true, false] {
        let bus = Arc::new(EventBus::new(128));
        let executor = ToolExecutor::with_standard_tools(
            Arc::clone(&bus),
            Arc::new(DirectSandbox::new_unchecked()),
        )
        .with_web_tools()
        .expect("Web tools");
        let model = Arc::new(ScriptedModel::new([
            Ok(tool_response(
                "fetch-critical",
                "web_fetch",
                json!({"url":"http://127.0.0.1/"}),
            )),
            Ok(text_response(
                r#"{"approve":true,"reason":"sends private data","risk_level":"critical","authorization_level":"high"}"#,
                FinishReason::Stop,
            )),
            Ok(text_response("finished", FinishReason::Stop)),
        ]));
        let runtime = AgentRuntime::new(Arc::clone(&bus), Arc::new(executor), model.clone());
        let mut events = bus.subscribe();
        let responder = spawn_approval_responder(Arc::clone(&bus), bus.subscribe(), approved);
        let run_id = runtime
            .delegate_chat(
                "review",
                Role::Orchestrator,
                "Read a public page".into(),
                RunConfig::default(),
            )
            .unwrap();
        assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
        responder.await.expect("responder");
        let events = drain_events(&mut events).await;
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            EventKind::Tool(ToolEvent::ApprovalRequested { tool_name, call_id, .. })
                if tool_name == "web_fetch" && call_id == &format!("{run_id}:fetch-critical")
        )));
        assert_eq!(
            events.iter().any(|event| matches!(
                &event.kind, EventKind::Tool(ToolEvent::ToolStarted { tool_name, call_id, .. })
                    if tool_name == "web_fetch" && call_id == "fetch-critical"
            )),
            approved
        );
        let audits: Vec<serde_json::Value> = events
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::Diagnostic(diag)
                    if diag.code == "tool_call_access"
                        && diag.call_id.as_deref() == Some("fetch-critical") =>
                {
                    Some(serde_json::from_str(&diag.detail).expect("audit JSON"))
                }
                _ => None,
            })
            .collect();
        assert_eq!(audits.len(), 1, "{audits:?}");
        assert_eq!(audits[0]["reviewer"], "user_review");
        assert_eq!(
            audits[0]["decision"],
            if approved { "allow" } else { "deny" }
        );
    }
}

#[tokio::test]
async fn web_fetch_user_review_executes_only_after_approval() {
    // Given: per-call user review with an approving responder.
    let (runtime, bus) = web_runtime_with(ScriptedModel::new([
        Ok(tool_response(
            "fetch-1",
            "web_fetch",
            json!({"url":"http://127.0.0.1/"}),
        )),
        Ok(text_response("finished", FinishReason::Stop)),
    ]));
    let mut events = bus.subscribe();
    let responder = spawn_approval_responder(Arc::clone(&bus), bus.subscribe(), true);

    // When
    let run_id = runtime.delegate_background(
        Role::Orchestrator,
        "fetch".to_string(),
        RunConfig::default(),
    );
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
    let events = drain_events(&mut events).await;

    // Then: 承認要求 (相関キーは run スコープの `{run_id}:{call_id}`) が
    // ToolStarted より前に発行され、承認後も private-address guard が
    // ローカルアドレスへのネットワーク I/O を拒否する。
    let approval_position = events
        .iter()
        .position(|event| {
            matches!(
                &event.kind,
            EventKind::Tool(ToolEvent::ApprovalRequested { tool_name, call_id, .. })
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
async fn web_fetch_denied_user_review_never_starts() {
    // Given: per-call user review with a denying responder.
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
        RunConfig::default(),
    );
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
    let events = drain_events(&mut events).await;

    // Then: 承認 (相関キーは run スコープの `{run_id}:{call_id}`) は要求されるが、
    // 拒否されたため executor に到達しない (AC6)。
    assert!(events.iter().any(|event| matches!(
        &event.kind,
            EventKind::Tool(ToolEvent::ApprovalRequested { tool_name, call_id, input: Some(input) })
            if tool_name == "web_fetch" && call_id == &format!("{run_id}:fetch-1")
                && input == &json!({"url": "https://example.invalid/"})
    )));
    assert!(!events.iter().any(|event| matches!(
        &event.kind,
        EventKind::Tool(ToolEvent::ToolStarted { tool_name, .. }) if tool_name == "web_fetch"
    )));
    responder.await.expect("応答タスクが完了するはずです");
}

#[tokio::test]
async fn parallel_runs_do_not_cross_accept_approval_resolutions() {
    // Given: 同一 EventBus を共有する 1 ランタイム上の 2 つの run が
    // 同一 model call_id "fetch-1" (run-local) で web_fetch を要求し、応答者は
    // run A の run スコープ相関キーだけを 1 回承認する (run B には応答しない)。
    let model = ScriptedModel::new([]);
    model
        .add_keyed(
            "RUN-A",
            [
                Ok(tool_response(
                    "fetch-1",
                    "web_fetch",
                    json!({"url":"http://127.0.0.1/"}),
                )),
                Ok(text_response("finished", FinishReason::Stop)),
            ],
        )
        .await;
    model
        .add_keyed(
            "RUN-B",
            [
                Ok(tool_response(
                    "fetch-1",
                    "web_fetch",
                    json!({"url":"http://127.0.0.1/"}),
                )),
                Ok(text_response("finished", FinishReason::Stop)),
            ],
        )
        .await;
    let (runtime, bus) = web_runtime_with(model);
    let mut events = bus.subscribe();
    let config = RunConfig::default();

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
            EventKind::Tool(ToolEvent::ApprovalRequested { tool_name, call_id, .. })
            if tool_name == "web_fetch" && call_id == &format!("{run_a}:fetch-1")
    )));
    assert!(events.iter().any(|event| matches!(
        &event.kind,
            EventKind::Tool(ToolEvent::ApprovalRequested { tool_name, call_id, .. })
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
async fn web_researcher_web_search_reaches_executor() {
    // Given
    let (runtime, bus) = web_runtime_with(ScriptedModel::new([
        Ok(tool_response("search-2", "web_search", json!({}))),
        Ok(text_response("finished", FinishReason::Stop)),
    ]));
    let mut events = bus.subscribe();

    // When
    let run_id = runtime.delegate_background(
        Role::WebResearcher,
        "search".to_string(),
        RunConfig::default(),
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

/// A page whose outline names one file input, `e5`, inside a form served by
/// `https://forms.example.test`, and that records every upload.
#[derive(Default)]
struct UploadPage {
    uploads: std::sync::Mutex<Vec<serde_json::Value>>,
}

#[async_trait::async_trait]
impl browser::agent::CdpTarget for UploadPage {
    async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, browser::agent::AgentError> {
        Ok(match method {
            "Accessibility.getFullAXTree" => json!({"nodes": [{
                "nodeId": "1", "role": {"value": "button"}, "name": {"value": "Attachment"},
                "backendDOMNodeId": 5
            }]}),
            "DOM.resolveNode" => json!({"object": {"objectId": "input-5"}}),
            "Runtime.callFunctionOn" => {
                json!({"result": {"value": "https://forms.example.test/vendor/upload"}})
            }
            "Runtime.evaluate" => json!({"result": {"value": "https://portal.example.test/apply"}}),
            "DOM.setFileInputFiles" => {
                self.uploads.lock().unwrap().push(params["files"].clone());
                json!({})
            }
            _ => json!({}),
        })
    }

    async fn navigate(&self, _url: &str) -> Result<(), browser::agent::AgentError> {
        Ok(())
    }
}

struct OnePage(Arc<browser::agent::PageAgent>);

#[async_trait::async_trait]
impl browser::agent::PageProvider for OnePage {
    async fn page(
        &self,
        _key: &str,
    ) -> Result<Arc<browser::agent::PageAgent>, browser::agent::AgentError> {
        Ok(Arc::clone(&self.0))
    }

    async fn open_page(&self, _key: &str) -> Option<Arc<browser::agent::PageAgent>> {
        Some(Arc::clone(&self.0))
    }

    async fn close(&self, _key: &str) -> bool {
        false
    }

    fn release(&self, _key: &str) {}
}

#[tokio::test]
async fn browser_upload_runs_only_after_its_review_approves() {
    let approve = r#"{"approve":true,"reason":"requested","risk_level":"medium","authorization_level":"high"}"#;
    let deny = r#"{"approve":false,"reason":"unrelated site","risk_level":"high","authorization_level":"none"}"#;
    for (verdict, uploads) in [(approve, true), (deny, false), ("not a verdict", false)] {
        // Given: a run whose page shows a file input and a workspace file to
        // upload, with a reviewer that answers `verdict`.
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("docs")).unwrap();
        std::fs::write(root.join("docs/report.txt"), "quarterly numbers").unwrap();
        let page = Arc::new(UploadPage::default());
        let agent = Arc::new(browser::agent::PageAgent::new(page.clone()));
        agent.snapshot().await.unwrap();
        let bus = Arc::new(EventBus::new(128));
        let executor = ToolExecutor::with_standard_tools_in(
            Arc::clone(&bus),
            Arc::new(DirectSandbox::new_unchecked()),
            Some(root.clone()),
        )
        .with_browser_provider(Arc::new(OnePage(agent)), Default::default());
        executor.set_file_boundary(&sandbox::SandboxBoundary::new(root.clone()));
        let model = Arc::new(ScriptedModel::new([
            Ok(tool_response(
                "upload-1",
                "browser_upload_file",
                json!({"ref": "e5", "paths": [root.join("docs/report.txt")]}),
            )),
            Ok(text_response(verdict, FinishReason::Stop)),
            Ok(text_response("finished", FinishReason::Stop)),
        ]));
        let runtime = AgentRuntime::new(Arc::clone(&bus), Arc::new(executor), model.clone());
        runtime.set_web_tools_enabled(true);
        let mut events = bus.subscribe();

        // When: the agent uploads the file.
        let run_id = runtime
            .delegate_chat(
                "upload",
                Role::WebResearcher,
                "Send docs/report.txt through the vendor form".into(),
                RunConfig::default(),
            )
            .unwrap();
        assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
        let events = drain_events(&mut events).await;

        // Then: the reviewer saw the receiving site and the file, and the
        // page got the file only when the review approved.
        let started = events.iter().any(|event| {
            matches!(
                &event.kind,
                EventKind::Tool(ToolEvent::ToolStarted { tool_name, call_id, .. })
                    if tool_name == "browser_upload_file" && call_id == "upload-1"
            )
        });
        assert_eq!(started, uploads, "{verdict}");
        assert_eq!(
            page.uploads.lock().unwrap().len(),
            usize::from(uploads),
            "{verdict}"
        );
        let review = &model.observed().await[1];
        let providers::ContentBlock::Text { text } = &review[1].content[0] else {
            panic!("review payload");
        };
        let payload: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(
            payload["upload"]["form_origin"],
            "https://forms.example.test"
        );
        assert_eq!(
            payload["upload"]["page_url"],
            "https://portal.example.test/apply"
        );
        assert_eq!(
            payload["upload"]["files"],
            json!([{"path": "docs/report.txt", "bytes": 17}])
        );
    }
}
