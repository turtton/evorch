//! Runtime regression tests using the real shell, snapshot locks, and storage.
#[path = "shell_jobs_integration/finalization.rs"]
mod finalization;
mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use event_bus::{AgentRunPhase, EventBus};
use futures_util::FutureExt;
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolResultContent, ToolSpec};
use runtime::snapshot::SnapshotService;
use runtime::workspace::{Project, WorktreeManager};
use runtime::{
    AgentInvocationContext, AgentModel, AgentRuntime, Role, RunConfig, RunId, RunStore,
    RuntimeError, WorkspaceMode,
};
use serde_json::json;
use storage::{Storage, StorageConfig};
use support::{init_git_repo, recording_factory, text_response, tool_response};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Duration, timeout};
use tools::ToolExecutor;

const DEADLINE: Duration = Duration::from_secs(10);

struct Invocation {
    messages: Vec<Message>,
    reply: oneshot::Sender<ChatResponse>,
}
impl Invocation {
    fn respond(self, response: ChatResponse) {
        self.reply.send(response).expect("model call remains live");
    }
    fn result(&self, id: &str) -> (String, bool) {
        self.messages
            .iter()
            .flat_map(|message| &message.content)
            .find_map(|block| match block {
                ContentBlock::ToolResult {
                    tool_call_id,
                    content,
                    is_error,
                } if tool_call_id == id => Some((
                    content
                        .iter()
                        .map(|part| match part {
                            ToolResultContent::Text { text } => text.as_str(),
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                    *is_error,
                )),
                _ => None,
            })
            .unwrap_or_else(|| panic!("missing result {id}: {:?}", self.messages))
    }
    fn job(&self, id: &str) -> String {
        field(&self.result(id).0, "shell job: ").into()
    }
}
fn field<'a>(text: &'a str, prefix: &str) -> &'a str {
    text.lines()
        .find_map(|line| line.strip_prefix(prefix))
        .unwrap_or_else(|| panic!("missing {prefix} in {text}"))
}

struct ExchangeModel(mpsc::UnboundedSender<Invocation>);
#[async_trait]
impl AgentModel for ExchangeModel {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "shell-integration-test".into()
    }
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        messages: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        let (reply, receiver) = oneshot::channel();
        self.0
            .send(Invocation {
                messages: messages.to_vec(),
                reply,
            })
            .expect("test receiver remains live");
        receiver.await.map_err(|_| RuntimeError::Model {
            reason: "test response channel closed".into(),
        })
    }
}
fn model() -> (Arc<ExchangeModel>, mpsc::UnboundedReceiver<Invocation>) {
    let (sender, receiver) = mpsc::unbounded_channel();
    (Arc::new(ExchangeModel(sender)), receiver)
}
async fn next(receiver: &mut mpsc::UnboundedReceiver<Invocation>) -> Invocation {
    timeout(DEADLINE, receiver.recv())
        .await
        .expect("runtime must advance without deadlock")
        .expect("model call")
}
fn executor(bus: &Arc<EventBus>, root: &Path) -> Arc<ToolExecutor> {
    Arc::new(ToolExecutor::with_standard_tools_in(
        bus.clone(),
        Arc::new(sandbox::DirectSandbox::new_unchecked()),
        Some(root.into()),
    ))
}
fn snapshots(root: &Path, base: &Path) -> Arc<SnapshotService> {
    Arc::new(SnapshotService::new(root, &base.join("snapshots")).unwrap())
}
async fn wait(runtime: &AgentRuntime, run: RunId) -> AgentRunPhase {
    timeout(DEADLINE, runtime.wait(run))
        .await
        .expect("terminal deadline")
        .unwrap()
}
fn assert_pid_reaped(pid: &str) {
    assert!(
        !PathBuf::from(format!("/proc/{pid}")).exists(),
        "process {pid} must be reaped before releasing its workspace"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn yielded_job_allows_reads_rejects_mutation_then_releases_workspace_after_observation() {
    let (temp, root) = init_git_repo();
    let bus = Arc::new(EventBus::new(256));
    let executor = executor(&bus, &root);
    let snapshots = snapshots(&root, temp.path());
    let (model, mut calls) = model();
    let runtime = AgentRuntime::new(bus, executor.clone(), model).with_snapshots(snapshots.clone());
    let run =
        runtime.delegate_background(Role::Worker, "exercise shell".into(), RunConfig::default());
    next(&mut calls).await.respond(tool_response("start", "shell", json!({"command":"printf 'ready\\n'; IFS= read -r reply; printf '%s' \"$reply\" > result.txt; printf 'done\\n'", "yield_ms":1000})));
    let call = next(&mut calls).await;
    let job = call.job("start");
    assert!(executor.has_running_shell_jobs(&run.to_string()));
    assert!(
        snapshots.lock(None).now_or_never().is_none(),
        "running process owns snapshot lock"
    );
    call.respond(tool_response("read", "read", json!({"path":"README.md"})));
    let call = next(&mut calls).await;
    assert!(!call.result("read").1);
    assert!(call.result("read").0.contains("# test"));
    call.respond(tool_response(
        "blocked-write",
        "write",
        json!({"path":"README.md", "content":"must not write"}),
    ));
    let call = next(&mut calls).await;
    assert!(call.result("blocked-write").1);
    assert!(
        call.result("blocked-write")
            .0
            .contains("owns this workspace")
    );
    assert_eq!(
        std::fs::read_to_string(root.join("README.md")).unwrap(),
        "# test\n"
    );
    call.respond(tool_response(
        "input",
        "shell",
        json!({"action":"stdin", "job_id":job, "input":"answer\n", "yield_ms":0}),
    ));
    let mut call = next(&mut calls).await;
    let mut cursor: u64 = field(&call.result("input").0, "cursor: ").parse().unwrap();
    for attempt in 0..4 {
        let id = format!("poll-{attempt}");
        call.respond(tool_response(
            &id,
            "shell",
            json!({"action":"poll", "job_id":job, "cursor":cursor, "yield_ms":1000}),
        ));
        call = next(&mut calls).await;
        let (result, error) = call.result(&id);
        assert!(!error, "{result}");
        if field(&result, "status: ") == "completed" {
            break;
        }
        cursor = field(&result, "cursor: ").parse().unwrap();
    }
    assert!(!executor.has_unobserved_shell_jobs(&run.to_string()));
    assert_eq!(
        std::fs::read_to_string(root.join("result.txt")).unwrap(),
        "answer"
    );
    drop(
        timeout(DEADLINE, snapshots.lock(None))
            .await
            .unwrap()
            .unwrap(),
    );
    call.respond(tool_response(
        "write-after",
        "write",
        json!({"path":"README.md", "content":"after completion"}),
    ));
    let call = next(&mut calls).await;
    assert!(!call.result("write-after").1);
    call.respond(text_response("all observed", FinishReason::Stop));
    assert_eq!(wait(&runtime, run).await, AgentRunPhase::Done);
    assert_eq!(
        runtime.run_result(run).unwrap().as_deref(),
        Some("all observed")
    );
    assert_eq!(
        std::fs::read_to_string(root.join("README.md")).unwrap(),
        "after completion"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn natural_stop_rejects_running_and_drained_but_unobserved_jobs() {
    let (temp, root) = init_git_repo();
    let bus = Arc::new(EventBus::new(256));
    let executor = executor(&bus, &root);
    let (model, mut calls) = model();
    let runtime = AgentRuntime::new(bus, executor.clone(), model)
        .with_snapshots(snapshots(&root, temp.path()));
    let run =
        runtime.delegate_background(Role::Worker, "completion gate".into(), RunConfig::default());
    next(&mut calls).await.respond(tool_response(
        "start",
        "shell",
        json!({"command":"read value", "yield_ms":0}),
    ));
    let call = next(&mut calls).await;
    let job = call.job("start");
    call.respond(text_response("premature natural stop", FinishReason::Stop));
    let call = next(&mut calls).await;
    assert_ne!(
        runtime.inspect_agent(run).unwrap().phase,
        AgentRunPhase::Done
    );
    assert_eq!(runtime.run_result(run).unwrap(), None);
    executor.drain_shell_jobs(&run.to_string()).await.unwrap();
    assert!(executor.has_unobserved_shell_jobs(&run.to_string()));
    call.respond(text_response("drained is not observed", FinishReason::Stop));
    let call = next(&mut calls).await;
    assert_ne!(
        runtime.inspect_agent(run).unwrap().phase,
        AgentRunPhase::Done
    );
    assert_eq!(runtime.run_result(run).unwrap(), None);
    call.respond(tool_response(
        "observe",
        "shell",
        json!({"action":"poll", "job_id":job}),
    ));
    let call = next(&mut calls).await;
    assert_eq!(field(&call.result("observe").0, "status: "), "cancelled");
    call.respond(text_response("observed cancellation", FinishReason::Stop));
    assert_eq!(wait(&runtime, run).await, AgentRunPhase::Done);
    assert_eq!(
        runtime.run_result(run).unwrap().as_deref(),
        Some("observed cancellation")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_reaps_job_before_unlock_and_preserves_uncertain_effects_in_storage() {
    let (temp, root) = init_git_repo();
    let bus = Arc::new(EventBus::new(256));
    let executor = executor(&bus, &root);
    let snapshots = snapshots(&root, temp.path());
    let config = StorageConfig {
        db_path: temp.path().join("history.sqlite3"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let (model, mut calls) = model();
    let runtime = AgentRuntime::new(bus, executor.clone(), model)
        .with_snapshots(snapshots.clone())
        .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let run = runtime.delegate_background(
        Role::Worker,
        "cancel with effects".into(),
        RunConfig {
            name: Some("chat:Worker:cancel-thread".into()),
            ..RunConfig::default()
        },
    );
    next(&mut calls).await.respond(tool_response("start", "shell", json!({"command":"printf partial > partial.txt; printf 'pid:%s\\n' \"$$\"; read value", "yield_ms":1000})));
    let call = next(&mut calls).await;
    let pid = field(&call.result("start").0, "pid:").to_owned();
    assert_eq!(
        std::fs::read_to_string(root.join("partial.txt")).unwrap(),
        "partial"
    );
    assert!(snapshots.lock(None).now_or_never().is_none());
    runtime.cancel(run).unwrap();
    assert_eq!(wait(&runtime, run).await, AgentRunPhase::Error);
    drop(
        timeout(DEADLINE, snapshots.lock(None))
            .await
            .unwrap()
            .unwrap(),
    );
    assert_pid_reaped(&pid);
    assert_eq!(runtime.run_result(run).unwrap(), None);
    assert!(
        !executor.has_unobserved_shell_jobs(&run.to_string()),
        "terminal handles are released only after persisting the uncertainty"
    );
    let diagnostics = runtime.restore_diagnostics(run).unwrap().unwrap();
    assert!(!diagnostics.disk_restorable);
    assert!(
        diagnostics
            .interrupted_tool_calls
            .iter()
            .any(|call| call.tool_name == "shell"
                && call.may_have_side_effects
                && !call.result_observed)
    );
    let restore = runtime.delegate_chat(
        "cancel-thread",
        Role::Worker,
        "continue".into(),
        RunConfig::default(),
    );
    assert!(
        matches!(restore, Err(RuntimeError::RunRestoreFailed { .. })),
        "uncertain writes must not be silently replayed: {restore:?}"
    );
    drop(call);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn isolated_cleanup_happens_after_cancelled_job_is_reaped() {
    let (temp, root) = init_git_repo();
    let bus = Arc::new(EventBus::new(256));
    let executor = executor(&bus, &root);
    let snapshots = snapshots(&root, temp.path());
    let (model, mut calls) = model();
    let manager = WorktreeManager::new(Project::new(root.clone()).unwrap());
    let (factory, _) = recording_factory();
    let runtime = AgentRuntime::with_workspace_context(bus, executor, model, manager, factory)
        .with_snapshots(snapshots.clone());
    let run = runtime.delegate_background(
        Role::Worker,
        "isolated cancellation".into(),
        RunConfig {
            workspace_mode: WorkspaceMode::Isolated,
            ..RunConfig::default()
        },
    );
    next(&mut calls).await.respond(tool_response("start", "shell", json!({"command":"printf partial > partial.txt; printf 'pid:%s\\n' \"$$\"; read value", "yield_ms":1000})));
    let call = next(&mut calls).await;
    let pid = field(&call.result("start").0, "pid:").to_owned();
    let path = runtime
        .inspect_agent(run)
        .unwrap()
        .workspace
        .unwrap()
        .worktree_path
        .unwrap();
    assert!(path.join("partial.txt").exists());
    assert!(snapshots.lock(Some(&path)).now_or_never().is_none());
    runtime.cancel(run).unwrap();
    assert_eq!(wait(&runtime, run).await, AgentRunPhase::Error);
    drop(
        timeout(DEADLINE, snapshots.lock(Some(&path)))
            .await
            .unwrap()
            .unwrap(),
    );
    assert_pid_reaped(&pid);
    // Cleanup exposes inspection state but no watch. Yield to the cleanup task;
    // no guessed sleep is used as proof of completion.
    timeout(DEADLINE, async {
        while runtime
            .inspect_agent(run)
            .unwrap()
            .workspace
            .unwrap()
            .worktree_path
            .is_some()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("isolated workspace cleanup");
    assert!(!path.exists());
    assert_pid_reaped(&pid);
    drop(call);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_start_before_job_id_delivery_keeps_lease_until_real_process_teardown() {
    let (temp, root) = init_git_repo();
    let bus = Arc::new(EventBus::new(256));
    let executor = executor(&bus, &root);
    let snapshots = snapshots(&root, temp.path());
    let (model, mut calls) = model();
    let runtime = AgentRuntime::new(bus, executor.clone(), model).with_snapshots(snapshots.clone());
    let run = runtime.delegate_background(
        Role::Worker,
        "cancel pending start".into(),
        RunConfig::default(),
    );
    next(&mut calls).await.respond(tool_response("pending-start", "shell", json!({"command":"printf '%s\\n' \"$$\" > process.pid; printf partial > partial.txt; read value", "yield_ms":60000})));
    // The command deliberately emits no live output, so the starting tool is
    // still waiting and no job ID has been delivered when cancellation arrives.
    let pid = timeout(DEADLINE, async {
        loop {
            if let Ok(pid) = tokio::fs::read_to_string(root.join("process.pid")).await
                && !pid.trim().is_empty()
                && root.join("partial.txt").exists()
            {
                break pid.trim().to_owned();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("real child started and wrote partial effects");
    assert!(executor.has_running_shell_jobs(&run.to_string()));
    assert!(snapshots.lock(None).now_or_never().is_none());
    runtime.cancel(run).unwrap();
    assert_eq!(wait(&runtime, run).await, AgentRunPhase::Error);
    drop(
        timeout(DEADLINE, snapshots.lock(None))
            .await
            .unwrap()
            .unwrap(),
    );
    assert_pid_reaped(&pid);
    assert!(!executor.has_unobserved_shell_jobs(&run.to_string()));
    assert_eq!(runtime.run_result(run).unwrap(), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn finish_checks_unobserved_job_effects_before_accepting_completion() {
    let (_temp, root) = init_git_repo();
    let bus = Arc::new(EventBus::new(256));
    let executor = executor(&bus, &root);
    let (model, mut calls) = model();
    let runtime = AgentRuntime::new(bus, executor.clone(), model);
    let run = runtime.delegate_background(
        Role::Orchestrator,
        "finish gate".into(),
        RunConfig::default(),
    );
    let call = next(&mut calls).await;
    // Only Orchestrator exposes finish, and its model cannot spawn shell jobs.
    // Seed a real owner-scoped job through the public executor to exercise the
    // defensive finish gate without granting the model forbidden capabilities.
    let ctx = tools::ToolExecutionContext {
        run_id: run.to_string(),
        thread_id: None,
        call_id: None,
    };
    let started = executor
        .execute(
            &ctx,
            "shell",
            "external-start",
            json!({"command":"read value", "yield_ms":0}),
        )
        .await
        .unwrap();
    let job = started.detail.unwrap()["shell_job"]["job_id"]
        .as_str()
        .unwrap()
        .to_owned();
    call.respond(tool_response(
        "premature-finish",
        "finish",
        json!({"result":"unobserved result"}),
    ));
    let call = next(&mut calls).await;
    let (result, error) = call.result("premature-finish");
    assert!(error && result.contains("Shell jobs"), "{result}");
    assert_eq!(runtime.run_result(run).unwrap(), None);
    executor.drain_shell_jobs(&run.to_string()).await.unwrap();
    call.respond(tool_response(
        "drained-finish",
        "finish",
        json!({"result":"still unobserved"}),
    ));
    let call = next(&mut calls).await;
    let (result, error) = call.result("drained-finish");
    assert!(error && result.contains("Shell jobs"), "{result}");
    executor
        .execute(
            &ctx,
            "shell",
            "external-poll",
            json!({"action":"poll", "job_id":job}),
        )
        .await
        .unwrap();
    call.respond(tool_response(
        "safe-finish",
        "finish",
        json!({"result":"effects observed"}),
    ));
    assert_eq!(wait(&runtime, run).await, AgentRunPhase::Done);
    assert_eq!(
        runtime.run_result(run).unwrap().as_deref(),
        Some("effects observed")
    );
}
