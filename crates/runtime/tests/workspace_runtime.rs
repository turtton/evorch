mod support;

use std::sync::{Arc, Mutex};

use agents::Role;
use event_bus::{AgentRunPhase, EventBus, EventKind, ToolEvent};
use providers::FinishReason;
use runtime::{
    AgentRuntime, ProjectTrust, RulesSettings, RulesSource, RunConfig, WorkspaceInspection,
    WorkspaceMounts,
};
use sandbox::DirectSandbox;
use serde_json::json;
use tokio::sync::Notify;
use tokio::time::{Duration, sleep, timeout};
use tools::ToolExecutor;

use support::{ScriptedModel, git, init_git_repo, recording_factory, text_response, tool_response};

const POLL_INTERVAL: Duration = Duration::from_millis(25);
const SETUP_TIMEOUT: Duration = Duration::from_secs(5);

fn runtime_with_factory(
    model: Arc<ScriptedModel>,
) -> (
    AgentRuntime,
    Arc<Mutex<Vec<WorkspaceMounts>>>,
    Arc<EventBus>,
) {
    let bus = Arc::new(EventBus::new(64));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    let (factory, mounts) = recording_factory();
    let runtime = AgentRuntime::with_sandbox_factory(Arc::clone(&bus), executor, model, factory)
        .with_sequential_run_ids();
    (runtime, mounts, bus)
}

async fn wait_for_model_calls(model: &ScriptedModel, expected: usize) {
    timeout(SETUP_TIMEOUT, async {
        while model.observed().await.len() != expected {
            sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .expect("model call が期限内に観測される");
}

fn captured_mounts(mounts: &Mutex<Vec<WorkspaceMounts>>) -> Vec<WorkspaceMounts> {
    mounts
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_creates_no_worktree_or_task_branch() {
    // Given
    let (_temp, repo) = init_git_repo();
    let model = Arc::new(ScriptedModel::new([Ok(text_response(
        "done",
        FinishReason::Stop,
    ))]));
    let (runtime, mounts, _bus) = runtime_with_factory(Arc::clone(&model));

    // When
    let run_id =
        runtime.delegate_background(Role::Worker, "work".to_string(), RunConfig::default());

    // Then
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
    assert!(captured_mounts(&mounts).is_empty());
    assert!(!repo.join(".evorch").exists());
    assert!(
        String::from_utf8_lossy(&git(&repo, &["branch", "--list", "evorch/task/*"]).stdout)
            .trim()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn per_run_executor_registers_web_tools() {
    // Given: sandbox root を持つ run は run ごとに executor を再構築する。
    // WebResearcher は web_search を capability に持つ
    let (_temp, repo) = init_git_repo();
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response("search-1", "web_search", json!({}))),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let (runtime, mounts, bus) = runtime_with_factory(model);
    let runtime = runtime.with_sandbox_root(repo.clone());
    runtime.set_web_tools_enabled(true);
    let mut events = bus.subscribe();

    // When
    let run_id = runtime.delegate_background(
        Role::WebResearcher,
        "search".to_string(),
        RunConfig::default(),
    );

    // Then: 再構築された executor が web_search を登録済みなら ToolStarted が
    // 観測され、引数スキーマ検証 (query 必須) で is_error 完了する
    // (検証は実行前のためネットワーク I/O は発生しない)
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
    let roots: Vec<_> = captured_mounts(&mounts)
        .into_iter()
        .map(|mounts| mounts.workspace_root)
        .collect();
    assert_eq!(roots, std::slice::from_ref(&repo));
    let target = run_id.to_string();
    timeout(SETUP_TIMEOUT, async {
        let mut started = false;
        loop {
            match events.recv().await.expect("event bus remains open").kind {
                EventKind::Tool(ToolEvent::ToolStarted {
                    tool_name,
                    call_id,
                    run_id: Some(event_run),
                    ..
                }) if tool_name == "web_search" && call_id == "search-1" && event_run == target => {
                    started = true;
                }
                EventKind::Tool(ToolEvent::ToolCompleted {
                    tool_name,
                    call_id,
                    run_id: Some(event_run),
                    is_error,
                    ..
                }) if tool_name == "web_search" && call_id == "search-1" && event_run == target => {
                    assert!(
                        started,
                        "registered tool must emit Started before Completed"
                    );
                    assert!(is_error, "missing query must fail argument validation");
                    break;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("web_search completion must be observed independently of progress event count");
}

#[tokio::test]
async fn inspect_agent_reports_shared_workspace_default() {
    // Given: workspace context を持たない shared run
    let model = Arc::new(ScriptedModel::gated(
        [Ok(text_response("done", FinishReason::Stop))],
        Arc::new(Notify::new()),
    ));
    let bus = Arc::new(EventBus::new(64));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    let runtime = AgentRuntime::new(bus, executor, model).with_sequential_run_ids();
    let run_id =
        runtime.delegate_background(Role::Worker, "work".to_string(), RunConfig::default());

    // When: run を inspection する
    let inspection = runtime
        .inspect_agent(run_id)
        .expect("run を inspection できる");

    // Then: registry 未登録でも shared workspace を合成する
    assert_eq!(
        inspection.workspace,
        Some(WorkspaceInspection { active_root: None })
    );
}

#[tokio::test]
async fn shared_inspection_keeps_startup_root_after_default_cwd_changes() {
    // Given: rules root を持つ shared run。最初の model 呼出しを gate する。
    let root = tempfile::tempdir().expect("rules root");
    let next_root = tempfile::tempdir().expect("next cwd");
    let gate = Arc::new(Notify::new());
    let model = Arc::new(ScriptedModel::gated(
        [Ok(text_response("done", FinishReason::Stop))],
        Arc::clone(&gate),
    ));
    let bus = Arc::new(EventBus::new(64));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    let runtime = AgentRuntime::new(bus, executor, model.clone())
        .with_sequential_run_ids()
        .with_project_rules(Arc::new(RulesSource::new(
            ProjectTrust::Approved,
            RulesSettings::from(&config::RulesConfig::default()),
            None,
            Some(root.path().to_path_buf()),
            None,
        )));
    let run_id = runtime.delegate_background(Role::Worker, "work".into(), RunConfig::default());

    // When: current-thread runtime が run を実行する前は、設定から root を合成しない。
    assert_eq!(
        runtime
            .inspect_agent(run_id)
            .unwrap()
            .workspace
            .unwrap()
            .active_root,
        None,
    );
    wait_for_model_calls(&model, 1).await;
    let expected = Some(WorkspaceInspection {
        active_root: Some(root.path().to_path_buf()),
    });
    let started = runtime.inspect_agent(run_id).expect("started inspection");
    assert_eq!(started.phase, AgentRunPhase::Running);
    assert_eq!(started.workspace, expected);

    // Then: 設定を変更しても Running/Done の既存 run は開始時の root を保持する。
    runtime
        .set_default_cwd(next_root.path().to_path_buf())
        .expect("update cwd");
    assert_eq!(runtime.inspect_agent(run_id).unwrap().workspace, expected);
    gate.notify_one();
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
    assert_eq!(runtime.inspect_agent(run_id).unwrap().workspace, expected);
}

#[tokio::test]
async fn inspecting_workspace_preserves_already_sent_model_prefix() {
    let root = tempfile::tempdir().expect("rules root");
    let next_root = tempfile::tempdir().expect("next cwd");
    let gate = Arc::new(Notify::new());
    let model = Arc::new(ScriptedModel::gated(
        [
            Ok(tool_response(
                "inspect-1",
                "inspect_agent",
                json!({"run_id": "run-1"}),
            )),
            Ok(tool_response(
                "inspect-2",
                "inspect_agent",
                json!({"run_id": "run-1"}),
            )),
            Ok(text_response("done", FinishReason::Stop)),
        ],
        Arc::clone(&gate),
    ));
    let bus = Arc::new(EventBus::new(64));
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
        .with_sequential_run_ids()
        .with_project_rules(Arc::new(RulesSource::new(
            ProjectTrust::Approved,
            RulesSettings::from(&config::RulesConfig::default()),
            None,
            Some(root.path().to_path_buf()),
            None,
        )));
    let run = runtime.delegate_background(
        Role::Orchestrator,
        "inspect workspace".into(),
        RunConfig::default(),
    );
    wait_for_model_calls(&model, 1).await;
    gate.notify_one();
    wait_for_model_calls(&model, 2).await;
    runtime
        .set_default_cwd(next_root.path().to_path_buf())
        .unwrap();
    gate.notify_one();
    wait_for_model_calls(&model, 3).await;
    gate.notify_one();
    assert_eq!(runtime.wait(run).await, Ok(AgentRunPhase::Done));

    let requests = model.observed().await;
    for pair in requests.windows(2) {
        assert!(
            pair[1].starts_with(&pair[0]),
            "inspection rewrote the sent prefix"
        );
    }
    let inspections: Vec<serde_json::Value> = requests
        .last()
        .unwrap()
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            providers::ContentBlock::ToolResult {
                content, is_error, ..
            } => {
                assert!(!is_error);
                let providers::ToolResultContent::Text { text } = &content[0] else {
                    panic!("expected text result");
                };
                Some(serde_json::from_str(text).unwrap())
            }
            _ => None,
        })
        .collect();
    assert_eq!(inspections.len(), 2);
    for inspection in inspections {
        assert_eq!(inspection["workspace"]["active_root"], json!(root.path()));
    }
}
