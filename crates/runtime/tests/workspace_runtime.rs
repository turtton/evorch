mod support;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agents::{NetworkAccess, Role};
use event_bus::{AgentRunPhase, EventBus, EventKind, LifecycleEvent, ToolEvent};
use providers::FinishReason;
use runtime::workspace::{Project, WorktreeManager};
use runtime::{
    AgentRuntime, IsolatedMounts, MergeMode, RunConfig, WorkspaceInspection, WorkspaceMode,
};
use sandbox::DirectSandbox;
use serde_json::json;
use tokio::sync::Notify;
use tokio::time::{Duration, sleep, timeout};
use tools::ToolExecutor;

use support::{
    ScriptedModel, collect_events, gated_factory, git, init_git_repo, recording_factory,
    text_response, tool_response,
};

const POLL_INTERVAL: Duration = Duration::from_millis(25);
const SETUP_TIMEOUT: Duration = Duration::from_secs(5);

fn runtime_with_workspace(
    repo: &Path,
    model: Arc<ScriptedModel>,
) -> (AgentRuntime, Arc<Mutex<Vec<IsolatedMounts>>>, Arc<EventBus>) {
    let bus = Arc::new(EventBus::new(64));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    let manager =
        WorktreeManager::new(Project::new(repo.to_path_buf()).expect("git リポジトリを検証できる"));
    let (factory, mounts) = recording_factory();
    let runtime =
        AgentRuntime::with_workspace_context(Arc::clone(&bus), executor, model, manager, factory);
    (runtime, mounts, bus)
}

fn isolated_config() -> RunConfig {
    RunConfig {
        workspace_mode: WorkspaceMode::Isolated,
        ..RunConfig::default()
    }
}

async fn wait_until(mut predicate: impl FnMut() -> bool) {
    timeout(SETUP_TIMEOUT, async {
        while !predicate() {
            sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .expect("条件が期限内に成立する");
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

fn captured_mounts(mounts: &Mutex<Vec<IsolatedMounts>>) -> Vec<IsolatedMounts> {
    mounts
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

fn branch_exists(repo: &Path, branch: &str) -> bool {
    git(
        repo,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .status
    .success()
}

fn registered_worktrees(repo: &Path) -> Vec<PathBuf> {
    String::from_utf8_lossy(&git(repo, &["worktree", "list", "--porcelain"]).stdout)
        .lines()
        .filter_map(|line| line.strip_prefix("worktree ").map(PathBuf::from))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_mode_spawns_without_worktree() {
    // Given
    let (_temp, repo) = init_git_repo();
    let model = Arc::new(ScriptedModel::new([Ok(text_response(
        "done",
        FinishReason::Stop,
    ))]));
    let (runtime, mounts, _bus) = runtime_with_workspace(&repo, Arc::clone(&model));

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
async fn isolated_mode_creates_worktree_with_per_run_sandbox() {
    // Given
    let (_temp, repo) = init_git_repo();
    let gate = Arc::new(Notify::new());
    let model = Arc::new(ScriptedModel::gated(
        [Ok(text_response("done", FinishReason::Stop))],
        Arc::clone(&gate),
    ));
    let (runtime, mounts, _bus) = runtime_with_workspace(&repo, Arc::clone(&model));

    // When
    let run_id = runtime.delegate_background(Role::Worker, "work".to_string(), isolated_config());
    let worktree = repo.join(".evorch/worktrees").join(run_id.to_string());
    let branch = format!("evorch/task/{run_id}");
    wait_until(|| {
        worktree.exists() && branch_exists(&repo, &branch) && captured_mounts(&mounts).len() == 1
    })
    .await;
    wait_for_model_calls(&model, 1).await;

    // Then
    let captured = captured_mounts(&mounts);
    let git_common_dir = repo.join(".git");
    assert_eq!(captured[0].workspace_root, worktree);
    assert_eq!(captured[0].ro_binds, vec![git_common_dir.clone()]);
    assert_eq!(
        captured[0].rw_binds,
        vec![
            git_common_dir.join("worktrees").join(run_id.to_string()),
            git_common_dir.join("objects"),
            git_common_dir.join("refs/heads"),
            git_common_dir.join("logs"),
        ]
    );
    gate.notify_waiters();
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inspect_agent_reports_isolated_workspace() {
    // Given: recording factory を持つ isolated run
    let (_temp, repo) = init_git_repo();
    let gate = Arc::new(Notify::new());
    let model = Arc::new(ScriptedModel::gated(
        [Ok(text_response("done", FinishReason::Stop))],
        Arc::clone(&gate),
    ));
    let (runtime, mounts, _bus) = runtime_with_workspace(&repo, model);
    let run_id = runtime.delegate_background(Role::Worker, "work".to_string(), isolated_config());
    let worktree_path = repo.join(".evorch/worktrees").join(run_id.to_string());
    let branch = format!("evorch/task/{run_id}");
    wait_until(|| captured_mounts(&mounts).len() == 1).await;

    // When: setup 完了中と cleanup 後に inspection する
    let active = runtime
        .inspect_agent(run_id)
        .expect("run を inspection できる");

    // Then: active 中は path を、cleanup 後は branch を保持する
    assert_eq!(
        active.workspace,
        Some(WorkspaceInspection {
            mode: WorkspaceMode::Isolated,
            branch: Some(branch.clone()),
            worktree_path: Some(worktree_path.clone()),
            merge_mode: MergeMode::Branch,
        })
    );
    // ここでの同期点は mounts 記録 (= model 呼出し前) なので、待機者不在のまま通知すると
    // 失われる。permit を保持する notify_one で model の notified() 到着を保証する。
    gate.notify_one();
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
    wait_until(|| !worktree_path.exists()).await;
    assert_eq!(
        runtime
            .inspect_agent(run_id)
            .expect("cleanup 後の run を inspection できる")
            .workspace,
        Some(WorkspaceInspection {
            mode: WorkspaceMode::Isolated,
            branch: Some(branch),
            worktree_path: None,
            merge_mode: MergeMode::Branch,
        })
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inspect_agent_reports_isolated_workspace_during_sandbox_build() {
    // Given: sandbox build が proceed まで停止する factory を持つ isolated run
    let (_temp, repo) = init_git_repo();
    let model = Arc::new(ScriptedModel::new([Ok(text_response(
        "done",
        FinishReason::Stop,
    ))]));
    let bus = Arc::new(EventBus::new(64));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    let manager =
        WorktreeManager::new(Project::new(repo.clone()).expect("git リポジトリを検証できる"));
    let (factory, entered_rx, proceed_tx) = gated_factory();
    let runtime =
        AgentRuntime::with_workspace_context(Arc::clone(&bus), executor, model, manager, factory);
    let run_id = runtime.delegate_background(Role::Worker, "work".to_string(), isolated_config());
    let worktree_path = repo.join(".evorch/worktrees").join(run_id.to_string());
    let branch = format!("evorch/task/{run_id}");
    timeout(SETUP_TIMEOUT, async {
        while entered_rx.try_recv().is_err() {
            sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .expect("sandbox build が期限内に開始される");

    // When: sandbox build の完了前に inspection する
    let active = runtime
        .inspect_agent(run_id)
        .expect("run を inspection できる");
    // inspection (snapshot) 取得直後に gate を解放する。assert 失敗時に build が
    // 滞留して runtime drop が応答不能になる経路を残さないため。
    let _ = proceed_tx.send(());

    // Then: build 完了前でも Isolated の branch / worktree path が観測できる
    assert_eq!(
        active.workspace,
        Some(WorkspaceInspection {
            mode: WorkspaceMode::Isolated,
            branch: Some(branch),
            worktree_path: Some(worktree_path),
            merge_mode: MergeMode::Branch,
        })
    );
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn isolated_run_executor_registers_web_tools() {
    // Given: isolated run では setup_isolated_workspace が executor を再構築する。
    // Librarian は web_fetch を capability に持つ
    let (_temp, repo) = init_git_repo();
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response("fetch-1", "web_fetch", json!({}))),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let (runtime, _mounts, bus) = runtime_with_workspace(&repo, model);
    let mut events = bus.subscribe();

    // When
    let run_id = runtime.delegate_background(
        Role::Librarian,
        "fetch".to_string(),
        RunConfig {
            workspace_mode: WorkspaceMode::Isolated,
            network_access: NetworkAccess::Allowed,
            ..RunConfig::default()
        },
    );

    // Then: 再構築された executor が web_fetch を登録済みなら ToolStarted が
    // 観測され、引数スキーマ検証 (url 必須) で is_error 完了する
    // (検証は実行前のためネットワーク I/O は発生しない)
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
    let events = collect_events(&mut events, 6).await;
    assert!(events.iter().any(|event| matches!(&event.kind, EventKind::Tool(ToolEvent::ToolStarted { tool_name, call_id, .. }) if tool_name == "web_fetch" && call_id == "fetch-1")));
    assert!(events.iter().any(|event| matches!(&event.kind, EventKind::Tool(ToolEvent::ToolCompleted { tool_name, call_id, is_error: true, .. }) if tool_name == "web_fetch" && call_id == "fetch-1")));
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
    let runtime = AgentRuntime::new(bus, executor, model);
    let run_id =
        runtime.delegate_background(Role::Worker, "work".to_string(), RunConfig::default());

    // When: run を inspection する
    let inspection = runtime
        .inspect_agent(run_id)
        .expect("run を inspection できる");

    // Then: registry 未登録でも shared workspace を合成する
    assert_eq!(
        inspection.workspace,
        Some(WorkspaceInspection {
            mode: WorkspaceMode::Shared,
            branch: None,
            worktree_path: None,
            merge_mode: MergeMode::Branch,
        })
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_isolated_runs_get_distinct_worktrees() {
    // Given
    let (_temp, repo) = init_git_repo();
    let gate = Arc::new(Notify::new());
    let model = Arc::new(ScriptedModel::gated(
        [
            Ok(text_response("first", FinishReason::Stop)),
            Ok(text_response("second", FinishReason::Stop)),
        ],
        Arc::clone(&gate),
    ));
    let (runtime, mounts, _bus) = runtime_with_workspace(&repo, Arc::clone(&model));

    // When
    let first = runtime.delegate_background(Role::Worker, "first".to_string(), isolated_config());
    let second = runtime.delegate_background(Role::Worker, "second".to_string(), isolated_config());
    wait_until(|| captured_mounts(&mounts).len() == 2).await;
    wait_for_model_calls(&model, 2).await;

    // Then
    let paths: Vec<PathBuf> = captured_mounts(&mounts)
        .into_iter()
        .map(|mount| mount.workspace_root)
        .collect();
    assert_ne!(paths[0], paths[1]);
    assert!(paths.iter().all(|path| path.exists()));
    let first_branch = format!("evorch/task/{first}");
    let second_branch = format!("evorch/task/{second}");
    assert_ne!(first_branch, second_branch);
    assert!(branch_exists(&repo, &first_branch));
    assert!(branch_exists(&repo, &second_branch));

    gate.notify_waiters();
    let (first_phase, second_phase) = tokio::join!(runtime.wait(first), runtime.wait(second));
    assert_eq!(first_phase, Ok(AgentRunPhase::Done));
    assert_eq!(second_phase, Ok(AgentRunPhase::Done));
    wait_until(|| registered_worktrees(&repo) == vec![repo.clone()]).await;
    assert!(branch_exists(&repo, &first_branch));
    assert!(branch_exists(&repo, &second_branch));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn isolated_run_can_check_out_existing_branch() {
    // Given: run A を isolated で完了させ、deliverable branch を残す
    let (_temp, repo) = init_git_repo();
    let gate = Arc::new(Notify::new());
    let model = Arc::new(ScriptedModel::gated(
        [
            Ok(text_response("first", FinishReason::Stop)),
            Ok(text_response("second", FinishReason::Stop)),
        ],
        Arc::clone(&gate),
    ));
    let (runtime, mounts, _bus) = runtime_with_workspace(&repo, Arc::clone(&model));
    let run_a = runtime.delegate_background(Role::Worker, "first".to_string(), isolated_config());
    wait_for_model_calls(&model, 1).await;
    gate.notify_one();
    assert_eq!(runtime.wait(run_a).await, Ok(AgentRunPhase::Done));
    let branch = format!("evorch/task/{run_a}");
    assert!(branch_exists(&repo, &branch));
    wait_until(|| {
        !repo
            .join(".evorch/worktrees")
            .join(run_a.to_string())
            .exists()
    })
    .await;

    // When: run B が既存 branch を workspace_branch に指定して isolated 開始する
    let run_b = runtime.delegate_background(
        Role::Worker,
        "second".to_string(),
        RunConfig {
            workspace_mode: WorkspaceMode::Isolated,
            workspace_branch: Some(branch.clone()),
            ..RunConfig::default()
        },
    );
    wait_until(|| {
        repo.join(".evorch/worktrees")
            .join(run_b.to_string())
            .exists()
            && branch_exists(&repo, &branch)
            && captured_mounts(&mounts).len() == 2
    })
    .await;
    wait_for_model_calls(&model, 2).await;

    // Then: B の inspection は指定 branch と、branch 名に由来しない worktree path を報告する
    let inspection = runtime
        .inspect_agent(run_b)
        .expect("run B を inspection できる")
        .workspace
        .expect("isolated run B の workspace inspection が常にある");
    assert_eq!(inspection.branch.as_deref(), Some(branch.as_str()));
    assert_eq!(
        inspection.worktree_path,
        Some(repo.join(".evorch/worktrees").join(run_b.to_string()))
    );
    assert_ne!(
        inspection.worktree_path,
        Some(repo.join(".evorch/worktrees").join(&branch))
    );

    // cleanup も成功し、既存 branch は保持される
    gate.notify_one();
    assert_eq!(runtime.wait(run_b).await, Ok(AgentRunPhase::Done));
    wait_until(|| {
        !repo
            .join(".evorch/worktrees")
            .join(run_b.to_string())
            .exists()
    })
    .await;
    assert!(branch_exists(&repo, &branch));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn isolated_without_workspace_context_fails_closed() {
    // Given
    let (_temp, repo) = init_git_repo();
    let model = Arc::new(ScriptedModel::new([Ok(text_response(
        "must not run",
        FinishReason::Stop,
    ))]));
    let bus = Arc::new(EventBus::new(64));
    let mut events = bus.subscribe();
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    let runtime = AgentRuntime::new(Arc::clone(&bus), executor, model.clone());

    // When
    let run_id = runtime.delegate_background(Role::Worker, "work".to_string(), isolated_config());

    // Then
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Error));
    let reason = timeout(SETUP_TIMEOUT, async {
        loop {
            let event = events.recv().await.expect("event bus remains open");
            if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                run_id: event_run_id,
                to: AgentRunPhase::Error,
                reason: Some(reason),
                ..
            }) = event.kind
                && event_run_id == run_id.to_string()
            {
                break reason;
            }
        }
    })
    .await
    .expect("error event を期限内に観測できる");
    assert!(reason.contains("workspace"));
    assert!(model.observed().await.is_empty());
    assert!(!repo.join(".evorch").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worktree_removed_on_done_and_on_cancel() {
    // Given
    let (_temp, repo) = init_git_repo();
    let done_model = Arc::new(ScriptedModel::new([Ok(text_response(
        "done",
        FinishReason::Stop,
    ))]));
    let (done_runtime, done_mounts, _done_bus) = runtime_with_workspace(&repo, done_model);

    // When
    let done_id =
        done_runtime.delegate_background(Role::Worker, "done".to_string(), isolated_config());

    // Then
    assert_eq!(done_runtime.wait(done_id).await, Ok(AgentRunPhase::Done));
    wait_until(|| {
        !repo
            .join(".evorch/worktrees")
            .join(done_id.to_string())
            .exists()
    })
    .await;
    assert!(branch_exists(&repo, &format!("evorch/task/{done_id}")));
    assert_eq!(captured_mounts(&done_mounts).len(), 1);

    // Given
    let (_cancel_temp, cancel_repo) = init_git_repo();
    let cancel_gate = Arc::new(Notify::new());
    let cancel_model = Arc::new(ScriptedModel::gated(
        [Ok(text_response("unused", FinishReason::Stop))],
        Arc::clone(&cancel_gate),
    ));
    let (cancel_runtime, cancel_mounts, _cancel_bus) =
        runtime_with_workspace(&cancel_repo, cancel_model);
    let cancel_id =
        cancel_runtime.delegate_background(Role::Worker, "cancel".to_string(), isolated_config());
    let cancel_path = cancel_repo
        .join(".evorch/worktrees")
        .join(cancel_id.to_string());
    wait_until(|| cancel_path.exists() && captured_mounts(&cancel_mounts).len() == 1).await;

    // When
    cancel_runtime
        .cancel(cancel_id)
        .expect("run を cancel できる");

    // Then
    assert_eq!(
        cancel_runtime.wait(cancel_id).await,
        Ok(AgentRunPhase::Error)
    );
    wait_until(|| !cancel_path.exists()).await;
    assert!(branch_exists(
        &cancel_repo,
        &format!("evorch/task/{cancel_id}")
    ));
}
