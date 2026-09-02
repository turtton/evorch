mod support;

use std::path::Path;
use std::sync::Arc;

use agents::Role;
use event_bus::{AgentRunPhase, Event, EventBus, EventKind, LifecycleEvent, ToolEvent};
use providers::{ContentBlock, FinishReason, Message, ToolResultContent};
use runtime::{AgentRuntime, ExecutionPolicy, RunConfig, WorkspaceMode};
use sandbox::{BwrapConfig, BwrapSandbox};
use serde_json::json;
use tokio::time::{Duration, timeout};

use support::{ScriptedModel, git, init_git_repo, text_response, tool_response};

const RUN_TIMEOUT: Duration = Duration::from_secs(10);

fn isolated_config() -> RunConfig {
    RunConfig {
        workspace_mode: WorkspaceMode::Isolated,
        ..RunConfig::default()
    }
}

fn runtime(repo: &Path, model: Arc<ScriptedModel>) -> (AgentRuntime, Arc<EventBus>) {
    let probe = tempfile::tempdir().expect("bwrap probe workspace can be created");
    BwrapSandbox::detect(BwrapConfig::new(probe.path().to_path_buf()))
        .expect("bwrap must be installed and usable for workspace_sandbox_e2e");
    let bus = Arc::new(EventBus::new(64));
    let runtime = AgentRuntime::production_with_project(
        Arc::clone(&bus),
        &ExecutionPolicy::for_role(Role::Worker),
        repo.to_path_buf(),
        model,
    )
    .expect("production runtime with real bwrap can be constructed");
    (runtime, bus)
}

async fn wait_done(runtime: &AgentRuntime, run_id: runtime::RunId) {
    assert_eq!(
        timeout(RUN_TIMEOUT, runtime.wait(run_id))
            .await
            .expect("isolated run finishes before timeout"),
        Ok(AgentRunPhase::Done)
    );
}

fn tool_result(messages: &[Vec<Message>], call_id: &str) -> (String, bool) {
    messages
        .iter()
        .flatten()
        .flat_map(|message| &message.content)
        .find_map(|block| match block {
            ContentBlock::ToolResult {
                tool_call_id,
                content,
                is_error,
            } if tool_call_id == call_id => content.first().map(|content| match content {
                ToolResultContent::Text { text } => (text.clone(), *is_error),
            }),
            ContentBlock::Text { .. }
            | ContentBlock::Reasoning { .. }
            | ContentBlock::ToolUse { .. }
            | ContentBlock::ToolResult { .. } => None,
        })
        .expect("scripted model observes the requested tool result")
}

fn stdout_from_shell(result: &str) -> &str {
    result
        .split_once("--- stdout ---\n")
        .and_then(|(_, output)| output.split_once("\n--- stderr ---"))
        .map(|(stdout, _)| stdout)
        .expect("shell result has stdout and stderr sections")
}

async fn events_until_completed(
    receiver: &mut event_bus::EventReceiver,
    run_id: &str,
) -> Vec<Event> {
    timeout(RUN_TIMEOUT, async {
        let mut events = Vec::new();
        loop {
            let event = receiver.recv().await.expect("event bus remains open");
            let completed = matches!(
                &event.kind,
                EventKind::Lifecycle(LifecycleEvent::BackgroundTaskCompleted { task_id })
                    if task_id == run_id
            );
            events.push(event);
            if completed {
                return events;
            }
        }
    })
    .await
    .expect("background completion event arrives before timeout")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_shell_can_git_add_commit_in_worktree() {
    // Given: a real repository and a Worker script that commits inside its isolated worktree.
    let (_temp, repo) = init_git_repo();
    let parent_head = git(&repo, &["rev-parse", "HEAD"]).stdout;
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response(
            "commit",
            "shell",
            json!({ "command": "sh", "args": ["-c", "echo evorch > f.txt && git add f.txt && git commit -m isolated"] }),
        )),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let (runtime, _bus) = runtime(&repo, model);

    // When: the isolated run executes through the production bwrap factory.
    let run_id = runtime.delegate_background(Role::Worker, "commit".into(), isolated_config());
    wait_done(&runtime, run_id).await;

    // Then: only the run branch contains the new commit and file.
    let branch = format!("evorch/task/{run_id}");
    let log = git(&repo, &["log", &branch, "--oneline", "-1"]);
    assert!(log.status.success());
    assert!(String::from_utf8_lossy(&log.stdout).contains("isolated"));
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]).stdout, parent_head);
    let parent_file = git(&repo, &["cat-file", "-e", "HEAD:f.txt"]);
    assert!(!parent_file.status.success());
    eprintln!(
        "run branch log: {}",
        String::from_utf8_lossy(&log.stdout).trim()
    );
    eprintln!("parent HEAD:f.txt status: {}", parent_file.status);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_cwd_is_worktree_root() {
    // Given: a shell call with no explicit cwd.
    let (_temp, repo) = init_git_repo();
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response("pwd", "shell", json!({ "command": "pwd" }))),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let (runtime, _bus) = runtime(&repo, Arc::clone(&model));

    // When: a Worker runs in an isolated workspace.
    let run_id = runtime.delegate_background(Role::Worker, "pwd".into(), isolated_config());
    wait_done(&runtime, run_id).await;

    // Then: bwrap defaults the command cwd to the run worktree root.
    let expected = repo.join(".evorch/worktrees").join(run_id.to_string());
    let (result, is_error) = tool_result(&model.observed().await, "pwd");
    assert!(!is_error, "{result}");
    eprintln!("isolated pwd: {}", stdout_from_shell(&result).trim());
    assert_eq!(
        stdout_from_shell(&result).trim(),
        expected.to_string_lossy()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_outside_allowed_set_rejected() {
    // Given: a command that tests writes to the parent repository and /etc.
    let (_temp, repo) = init_git_repo();
    let readme = repo.join("README.md");
    let original = std::fs::read(&readme).expect("README fixture can be read");
    let script = format!(
        "if echo x > '{}'; then exit 11; fi; if touch /etc/evorch-x; then exit 12; fi; exit 7",
        readme.display()
    );
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response(
            "outside",
            "shell",
            json!({ "command": "sh", "args": ["-c", script] }),
        )),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let (runtime, _bus) = runtime(&repo, Arc::clone(&model));

    // When: the Worker attempts both writes through real bwrap.
    let run_id = runtime.delegate_background(Role::Worker, "outside".into(), isolated_config());
    wait_done(&runtime, run_id).await;

    // Then: both writes fail and the host repository remains unchanged.
    let (result, is_error) = tool_result(&model.observed().await, "outside");
    assert!(is_error, "{result}");
    assert!(result.contains("exit_code: 7"), "{result}");
    assert_eq!(
        std::fs::read(readme).expect("README remains readable"),
        original
    );
    assert!(!Path::new("/etc/evorch-x").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_metadata_minimality() {
    // Given: the repository config outside the writable Git metadata subset.
    let (_temp, repo) = init_git_repo();
    let config = repo.join(".git/config");
    let original = std::fs::read(&config).expect("git config fixture can be read");
    let script = format!("echo hacked >> '{}'", config.display());
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response(
            "git-config",
            "shell",
            json!({ "command": "sh", "args": ["-c", script] }),
        )),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let (runtime, _bus) = runtime(&repo, Arc::clone(&model));

    // When: an isolated Worker attempts to modify the common Git config.
    let run_id = runtime.delegate_background(Role::Worker, "git config".into(), isolated_config());
    wait_done(&runtime, run_id).await;

    // Then: the read-only common-dir mount rejects the write.
    let (result, is_error) = tool_result(&model.observed().await, "git-config");
    assert!(is_error, "{result}");
    assert_eq!(
        std::fs::read(config).expect("git config remains readable"),
        original
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn denied_tool_spawns_no_process_despite_writable_git() {
    // Given: an Explorer shell call targeting the intentionally writable objects directory.
    let (_temp, repo) = init_git_repo();
    let side_effect = repo.join(".git/objects/side_effect.txt");
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response(
            "denied-shell",
            "shell",
            json!({ "command": "sh", "args": ["-c", format!("touch '{}'", side_effect.display())] }),
        )),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let (runtime, bus) = runtime(&repo, Arc::clone(&model));
    let mut receiver = bus.subscribe();

    // When: capability authorization rejects shell before executor dispatch.
    let run_id = runtime.delegate_background(Role::Explorer, "deny".into(), isolated_config());
    wait_done(&runtime, run_id).await;
    let events = events_until_completed(&mut receiver, &run_id.to_string()).await;

    // Then: the model receives an error, no process-start event occurs, and no file is created.
    let (result, is_error) = tool_result(&model.observed().await, "denied-shell");
    assert!(is_error, "{result}");
    assert!(result.contains("拒否"), "{result}");
    assert!(!events.iter().any(|event| matches!(
        &event.kind,
        EventKind::Tool(ToolEvent::ToolStarted { call_id, .. }) if call_id == "denied-shell"
    )));
    assert!(!side_effect.exists());
}
