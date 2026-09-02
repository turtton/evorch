mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agents::Role;
use event_bus::{AgentRunPhase, EventBus, EventKind, EventReceiver, LifecycleEvent};
use providers::FinishReason;
use runtime::workspace::{Project, WorktreeManager};
use runtime::{AgentRuntime, RunConfig, RuntimeError, WorkspaceMode};
use sandbox::DirectSandbox;
use tokio::sync::Notify;
use tokio::time::{Duration, sleep, timeout};
use tools::ToolExecutor;

use support::{ScriptedModel, git, init_git_repo, recording_factory, text_response, tool_response};

const POLL_INTERVAL: Duration = Duration::from_millis(25);
const TEST_TIMEOUT: Duration = Duration::from_secs(5);

fn runtime_with_workspace(repo: &Path, model: Arc<ScriptedModel>) -> (AgentRuntime, Arc<EventBus>) {
    let bus = Arc::new(EventBus::new(64));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    let manager =
        WorktreeManager::new(Project::new(repo.to_path_buf()).expect("git リポジトリを検証できる"));
    let (factory, _) = recording_factory();
    let runtime =
        AgentRuntime::with_workspace_context(Arc::clone(&bus), executor, model, manager, factory);
    (runtime, bus)
}

fn isolated_config() -> RunConfig {
    RunConfig {
        workspace_mode: WorkspaceMode::Isolated,
        ..RunConfig::default()
    }
}

fn stop_model(text: &str) -> Arc<ScriptedModel> {
    Arc::new(ScriptedModel::new([Ok(text_response(
        text,
        FinishReason::Stop,
    ))]))
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

fn worktree_paths(repo: &Path) -> Vec<PathBuf> {
    String::from_utf8_lossy(&git(repo, &["worktree", "list", "--porcelain"]).stdout)
        .lines()
        .filter_map(|line| line.strip_prefix("worktree ").map(PathBuf::from))
        .collect()
}

async fn wait_until(mut predicate: impl FnMut() -> bool) {
    timeout(TEST_TIMEOUT, async {
        while !predicate() {
            sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .expect("条件が期限内に成立する");
}

async fn wait_for_model_calls(model: &ScriptedModel, expected: usize) {
    timeout(TEST_TIMEOUT, async {
        while model.observed().await.len() != expected {
            sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .expect("model call が期限内に観測される");
}

async fn error_reason(events: &mut EventReceiver, run_id: &str) -> String {
    timeout(TEST_TIMEOUT, async {
        loop {
            let event = events.recv().await.expect("event bus remains open");
            if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                run_id: event_run_id,
                to: AgentRunPhase::Error,
                reason: Some(reason),
                ..
            }) = event.kind
                && event_run_id == run_id
            {
                break reason;
            }
        }
    })
    .await
    .expect("error event を期限内に観測できる")
}

async fn assert_cleaned(repo: &Path, run_name: &str, branch_expected: bool) {
    let path = repo.join(".evorch/worktrees").join(run_name);
    wait_until(|| !path.exists() && !worktree_paths(repo).contains(&path)).await;
    assert_eq!(
        branch_exists(repo, &format!("evorch/task/{run_name}")),
        branch_expected
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn creation_failure_precreated_branch_preserved() {
    // Given: runtime の採番先と同名の user branch が先に存在する
    let (_temp, repo) = init_git_repo();
    assert!(
        git(&repo, &["branch", "evorch/task/run-1", "HEAD"])
            .status
            .success()
    );
    let model = stop_model("must not run");
    let (runtime, bus) = runtime_with_workspace(&repo, Arc::clone(&model));
    let mut events = bus.subscribe();

    // When: isolated run の workspace setup が branch 衝突で失敗する
    let run_id = runtime.delegate_background(Role::Worker, "work".to_string(), isolated_config());

    // Then: run は model 呼び出し前に失敗し、user branch は保存される
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Error));
    assert!(
        error_reason(&mut events, &run_id.to_string())
            .await
            .contains("workspace")
    );
    assert!(model.observed().await.is_empty());
    assert_cleaned(&repo, &run_id.to_string(), true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn creation_midway_failure_rolls_back_and_errors() {
    // Given: worktree の子ディレクトリを作れない repository
    let (_temp, repo) = init_git_repo();
    let parent = repo.join(".evorch/worktrees");
    fs::create_dir_all(&parent).expect("worktree parent を作成できる");
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o555))
        .expect("worktree parent を読み取り専用にできる");
    let model = stop_model("must not run");
    let (runtime, _) = runtime_with_workspace(&repo, Arc::clone(&model));

    // When: branch 作成後の git worktree add が失敗する
    let run_id = runtime.delegate_background(Role::Worker, "work".to_string(), isolated_config());
    let phase = runtime.wait(run_id).await;
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o755))
        .expect("worktree parent の権限を復元できる");

    // Then: run は Error となり、途中生成物はすべて rollback される
    assert_eq!(phase, Ok(AgentRunPhase::Error));
    assert!(model.observed().await.is_empty());
    assert_cleaned(&repo, &run_id.to_string(), false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_cleans_worktree_keeps_branch() {
    // Given: model 応答待ちで停止する isolated run
    let (_temp, repo) = init_git_repo();
    let gate = Arc::new(Notify::new());
    let model = Arc::new(ScriptedModel::gated(
        [Ok(text_response("unused", FinishReason::Stop))],
        gate,
    ));
    let (runtime, bus) = runtime_with_workspace(&repo, Arc::clone(&model));
    let mut events = bus.subscribe();
    let run_id = runtime.delegate_background(Role::Worker, "work".to_string(), isolated_config());
    let path = repo.join(".evorch/worktrees").join(run_id.to_string());
    wait_until(|| path.exists()).await;
    wait_for_model_calls(&model, 1).await;

    // When: cooperative cancellation を通知する
    runtime.cancel(run_id).expect("run を cancel できる");

    // Then: cancelled Error 後に worktree だけが消え、branch は残る
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Error));
    assert_eq!(
        error_reason(&mut events, &run_id.to_string()).await,
        "cancelled"
    );
    assert_cleaned(&repo, &run_id.to_string(), true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_error_cleans_worktree() {
    // Given: model 境界がエラーを返す isolated run
    let (_temp, repo) = init_git_repo();
    let model = Arc::new(ScriptedModel::new([Err(RuntimeError::Model {
        reason: "injected model failure".to_string(),
    })]));
    let (runtime, _) = runtime_with_workspace(&repo, model);

    // When: run を終端まで実行する
    let run_id = runtime.delegate_background(Role::Worker, "work".to_string(), isolated_config());

    // Then: Error 経路でも worktree は消え、branch は残る
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Error));
    assert_cleaned(&repo, &run_id.to_string(), true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn normal_completion_cleans_worktree_keeps_branch() {
    // Given: 正常応答する isolated run
    let (_temp, repo) = init_git_repo();
    let model = stop_model("done");
    let (runtime, _) = runtime_with_workspace(&repo, model);

    // When: run が正常終了する
    let run_id = runtime.delegate_background(Role::Worker, "work".to_string(), isolated_config());

    // Then: Done 後に worktree は消え、merge 用 branch は残る
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
    assert_cleaned(&repo, &run_id.to_string(), true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preexisting_user_worktree_never_touched() {
    // Given: runtime 管理外の user branch と worktree が登録済みである
    let (temp, repo) = init_git_repo();
    let user_branch = "user/preserved";
    let user_path = temp.path().join("user-wt");
    assert!(
        git(&repo, &["branch", user_branch, "HEAD"])
            .status
            .success()
    );
    assert!(
        git(
            &repo,
            &[
                "worktree",
                "add",
                user_path.to_string_lossy().as_ref(),
                user_branch
            ]
        )
        .status
        .success()
    );
    let user_head = String::from_utf8_lossy(&git(&repo, &["rev-parse", user_branch]).stdout)
        .trim()
        .to_string();
    let model = stop_model("done");
    let (runtime, _) = runtime_with_workspace(&repo, model);

    // When: runtime 所有 worktree の lifecycle が完了する
    let run_id = runtime.delegate_background(Role::Worker, "work".to_string(), isolated_config());
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
    assert_cleaned(&repo, &run_id.to_string(), true).await;

    // Then: user 所有 path、branch、Git 登録はそのまま残る
    assert!(user_path.is_dir());
    assert!(branch_exists(&repo, user_branch));
    assert_eq!(
        String::from_utf8_lossy(&git(&repo, &["rev-parse", user_branch]).stdout).trim(),
        user_head
    );
    assert!(worktree_paths(&repo).contains(&user_path));
    assert!(user_path.join("README.md").is_file());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dirty_worktree_still_removed() {
    // Given: worktree 内へ未追跡ファイルを書き、その後に正常応答する model script
    let (_temp, repo) = init_git_repo();
    let gate = Arc::new(Notify::new());
    let run_path = repo.join(".evorch/worktrees/run-1");
    let model = Arc::new(ScriptedModel::gated(
        [
            Ok(tool_response(
                "dirty-write",
                "shell",
                serde_json::json!({
                    "command": "sh",
                    "args": ["-c", "echo x > dirty.txt"],
                    "cwd": run_path,
                }),
            )),
            Ok(text_response("done", FinishReason::Stop)),
        ],
        Arc::clone(&gate),
    ));
    let (runtime, _) = runtime_with_workspace(&repo, Arc::clone(&model));
    let run_id = runtime.delegate_background(Role::Worker, "work".to_string(), isolated_config());
    wait_for_model_calls(&model, 1).await;

    // When: shell tool が dirty file を作成し、最終応答まで進む
    gate.notify_one();
    wait_for_model_calls(&model, 2).await;
    wait_until(|| run_path.join("dirty.txt").is_file()).await;
    gate.notify_one();

    // Then: --force cleanup により dirty worktree も確実に削除され、branch は残る
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
    assert_cleaned(&repo, &run_id.to_string(), true).await;
}
