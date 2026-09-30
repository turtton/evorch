mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use event_bus::{AgentRunPhase, EventBus, EventKind, EventReceiver, LifecycleEvent};
use providers::FinishReason;
use runtime::workspace::{Project, WorktreeManager};
use runtime::{AgentRuntime, Role, RunConfig, RunId, RunStore, StopScope, WorkspaceMode};
use serde_json::json;
use storage::{Storage, StorageConfig};
use support::{ScriptedModel, git, init_git_repo, recording_factory, text_response, tool_response};
use tokio::sync::Notify;
use tokio::time::{sleep, timeout};
use tools::ToolExecutor;

const DEADLINE: Duration = Duration::from_secs(10);

struct Fixture {
    runtime: AgentRuntime,
    model: Arc<ScriptedModel>,
    child_gate: Arc<Notify>,
    events: EventReceiver,
    repo: PathBuf,
    config: StorageConfig,
    _storage: Storage,
    _dir: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> Self {
        let (dir, repo) = init_git_repo();
        let config = StorageConfig {
            db_path: dir.path().join("stop.sqlite3"),
            ..Default::default()
        };
        let storage = Storage::open(config.clone()).unwrap();
        let bus = Arc::new(EventBus::new(512));
        let events = bus.subscribe();
        let model = Arc::new(ScriptedModel::new([]));
        model
            .add_keyed(
                "PARENT",
                [Ok(tool_response(
                    "await-child",
                    "delegate",
                    json!({"prompt":"CHILD", "role":"worker"}),
                ))],
            )
            .await;
        let child_gate = Arc::new(Notify::new());
        model.gate_key("CHILD", child_gate.clone()).await;
        model
            .add_keyed(
                "CHILD",
                [Ok(text_response("child done", FinishReason::Stop))],
            )
            .await;
        let manager = WorktreeManager::new(Project::new(repo.clone()).unwrap());
        let (factory, _) = recording_factory();
        let runtime = AgentRuntime::with_workspace_context(
            bus.clone(),
            Arc::new(ToolExecutor::new(bus)),
            model.clone(),
            manager,
            factory,
        )
        .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
        Self {
            runtime,
            model,
            child_gate,
            events,
            repo,
            config,
            _storage: storage,
            _dir: dir,
        }
    }

    async fn start(&self, isolated: bool) -> (RunId, RunId) {
        let parent = self.runtime.delegate_background(
            Role::Orchestrator,
            "PARENT".into(),
            RunConfig {
                workspace_mode: if isolated {
                    WorkspaceMode::Isolated
                } else {
                    WorkspaceMode::Shared
                },
                ..Default::default()
            },
        );
        let child = timeout(DEADLINE, async {
            loop {
                let child = self.runtime.list_agents().into_iter().find(|run| {
                    run.parent_run_id == Some(parent) && run.phase == AgentRunPhase::Running
                });
                if let Some(child) = child
                    && self.runtime.inspect_agent(parent).unwrap().phase == AgentRunPhase::Waiting
                {
                    break child.run_id;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("parent waits on gated child");
        (parent, child)
    }

    fn dirty_workspace(&self, parent: RunId) -> PathBuf {
        let workspace = self
            .runtime
            .inspect_agent(parent)
            .unwrap()
            .workspace
            .unwrap();
        let path = workspace.worktree_path.unwrap();
        assert_eq!(path, self.repo.join(format!(".evorch/worktrees/{parent}")));
        std::fs::write(path.join("dirty.txt"), "retained unfinished work").unwrap();
        path
    }

    async fn wait(&self, run: RunId, phase: AgentRunPhase) {
        assert_eq!(
            timeout(DEADLINE, self.runtime.wait(run))
                .await
                .expect("terminal timeout")
                .unwrap(),
            phase
        );
    }

    async fn observed_calls(&self, count: usize) {
        timeout(DEADLINE, async {
            while self.model.observed().await.len() < count {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("model observation timeout");
    }
}

#[tokio::test]
async fn stage1_self_only_stops_parent_and_keeps_children() {
    let fixture = Fixture::new().await;
    let (parent, child) = fixture.start(false).await;
    fixture.runtime.stop(parent, StopScope::SelfOnly).unwrap();
    fixture.wait(parent, AgentRunPhase::Stopped).await;
    assert_eq!(
        fixture.runtime.inspect_agent(child).unwrap().phase,
        AgentRunPhase::Running
    );
    assert_eq!(fixture.runtime.live_descendants(parent), vec![child]);
    fixture.child_gate.notify_one();
    fixture.wait(child, AgentRunPhase::Done).await;
    assert_eq!(
        fixture.runtime.run_result(child).unwrap().as_deref(),
        Some("child done")
    );
}

#[tokio::test]
async fn stage1_persists_stopped_snapshot_and_retains_worktree() {
    let fixture = Fixture::new().await;
    let (parent, child) = fixture.start(true).await;
    let path = fixture.dirty_workspace(parent);
    fixture.runtime.stop(parent, StopScope::SelfOnly).unwrap();
    fixture.wait(parent, AgentRunPhase::Stopped).await;
    let record = storage::Database::open(&fixture.config)
        .unwrap()
        .run_context(&parent.to_string())
        .unwrap()
        .unwrap();
    assert_eq!(record.terminal_phase, "Stopped");
    assert!(record.restorable);
    assert_eq!(
        std::fs::read_to_string(path.join("dirty.txt")).unwrap(),
        "retained unfinished work"
    );
    let reopened = WorktreeManager::new(Project::new(fixture.repo.clone()).unwrap())
        .open_existing(parent)
        .unwrap();
    assert_eq!(reopened.path, path);
    fixture.child_gate.notify_one();
    fixture.wait(child, AgentRunPhase::Done).await;
}

#[tokio::test]
async fn stage2_subtree_stops_descendants() {
    let fixture = Fixture::new().await;
    let (parent, child) = fixture.start(false).await;
    fixture.runtime.stop(parent, StopScope::Subtree).unwrap();
    fixture.wait(parent, AgentRunPhase::Stopped).await;
    fixture.wait(child, AgentRunPhase::Stopped).await;
    assert!(fixture.runtime.live_descendants(parent).is_empty());
}

#[tokio::test]
async fn stage2_after_stage1_stops_remaining_descendants() {
    let fixture = Fixture::new().await;
    let (parent, child) = fixture.start(false).await;
    fixture.runtime.stop(parent, StopScope::SelfOnly).unwrap();
    fixture.wait(parent, AgentRunPhase::Stopped).await;
    fixture.runtime.stop(parent, StopScope::Subtree).unwrap();
    fixture.wait(child, AgentRunPhase::Stopped).await;
    assert_eq!(
        fixture.runtime.inspect_agent(parent).unwrap().phase,
        AgentRunPhase::Stopped
    );
}

#[tokio::test]
async fn cancel_regression() {
    let mut fixture = Fixture::new().await;
    let (parent, child) = fixture.start(true).await;
    let path = fixture.dirty_workspace(parent);
    fixture.runtime.cancel(parent).unwrap();
    fixture.wait(parent, AgentRunPhase::Error).await;
    fixture.wait(child, AgentRunPhase::Error).await;
    timeout(DEADLINE, async {
        loop {
            if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                run_id,
                to: AgentRunPhase::Error,
                reason,
                ..
            }) = fixture.events.recv().await.unwrap().kind
                && run_id == parent.to_string()
            {
                assert_eq!(reason.as_deref(), Some("cancelled"));
                break;
            }
        }
    })
    .await
    .unwrap();
    assert!(
        !path.exists(),
        "hard cancellation must still remove the worktree"
    );
    assert!(
        git(
            &fixture.repo,
            &[
                "show-ref",
                "--verify",
                &format!("refs/heads/evorch/task/{parent}")
            ]
        )
        .status
        .success()
    );
}

#[tokio::test]
async fn resume_after_stage1_reattaches_and_reports_unknown() {
    let fixture = Fixture::new().await;
    let (parent, child) = fixture.start(true).await;
    let path = fixture.dirty_workspace(parent);
    fixture.runtime.stop(parent, StopScope::SelfOnly).unwrap();
    fixture.wait(parent, AgentRunPhase::Stopped).await;
    // Keep the original child live: only stopped-root continuation may relax this fence.
    assert_eq!(fixture.runtime.live_descendants(parent), vec![child]);
    let gate = Arc::new(Notify::new());
    fixture.model.gate_key("PARENT", gate.clone()).await;
    fixture
        .model
        .add_keyed(
            "PARENT",
            [Ok(tool_response(
                "read-retained",
                "read",
                json!({"path":"dirty.txt"}),
            ))],
        )
        .await;
    fixture.observed_calls(2).await;
    let before = fixture.model.observed().await.len();
    let resumed = fixture
        .runtime
        .continue_goal(
            parent,
            "inspect retained work".into(),
            RunConfig {
                workspace_mode: WorkspaceMode::Isolated,
                ..Default::default()
            },
        )
        .expect("stopped root can resume while its own child remains live");
    assert_eq!(resumed, parent);
    fixture.observed_calls(before + 1).await;
    let observed = fixture.model.observed().await;
    let history = serde_json::to_string(observed.last().unwrap()).unwrap();
    assert!(history.contains("ToolExecutionOutcomeUnknown"), "{history}");
    assert!(history.contains("await-child"), "{history}");
    assert!(history.contains("inspect retained work"));
    let workspace = fixture
        .runtime
        .inspect_agent(parent)
        .unwrap()
        .workspace
        .unwrap();
    assert_eq!(workspace.worktree_path.as_ref(), Some(&path));
    assert_eq!(
        workspace.branch.as_deref(),
        Some(format!("evorch/task/{parent}").as_str())
    );
    gate.notify_one();
    fixture.observed_calls(before + 2).await;
    let observed = fixture.model.observed().await;
    let history = serde_json::to_string(observed.last().unwrap()).unwrap();
    assert!(
        history.contains("retained unfinished work"),
        "reattached executor read: {history}"
    );
    fixture.runtime.stop(parent, StopScope::SelfOnly).unwrap();
    fixture.wait(parent, AgentRunPhase::Stopped).await;
    fixture.child_gate.notify_one();
    fixture.wait(child, AgentRunPhase::Done).await;
}

#[tokio::test]
async fn stop_while_waiting_for_user_input_is_not_an_error() {
    let fixture = Fixture::new().await;
    fixture
        .model
        .add_keyed(
            "INTERACTIVE",
            [Ok(text_response("ready", FinishReason::Stop))],
        )
        .await;
    let run = fixture.runtime.delegate_background(
        Role::Worker,
        "INTERACTIVE".into(),
        RunConfig {
            interactive: true,
            keep_alive: true,
            ..Default::default()
        },
    );
    timeout(DEADLINE, async {
        while fixture.runtime.inspect_agent(run).unwrap().phase != AgentRunPhase::Waiting {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    fixture.runtime.stop(run, StopScope::SelfOnly).unwrap();
    fixture.wait(run, AgentRunPhase::Stopped).await;
}

#[tokio::test]
async fn stop_before_loop_start_reaches_stopped() {
    let fixture = Fixture::new().await;
    // This current-thread test does not yield between registration and stop.
    let parent = fixture.runtime.delegate_background(
        Role::Worker,
        "not started".into(),
        RunConfig::default(),
    );
    fixture.runtime.stop(parent, StopScope::SelfOnly).unwrap();
    fixture.wait(parent, AgentRunPhase::Stopped).await;
    assert!(fixture.model.observed().await.is_empty());
}

struct BlockedTool(Arc<Notify>);

#[async_trait::async_trait]
impl tools::Tool for BlockedTool {
    fn name(&self) -> &str {
        "write"
    }
    fn schema(&self) -> serde_json::Value {
        json!({"type":"object"})
    }
    fn permissions(&self) -> tools::Permissions {
        tools::Permissions::read_write()
    }
    async fn execute(&self, _: serde_json::Value) -> Result<tools::ToolResult, tools::ToolError> {
        self.0.notify_one();
        std::future::pending().await
    }
}

#[tokio::test]
async fn stopped_tool_result_reports_stopped_and_persists_unknown_outcome() {
    let fixture = Fixture::new().await;
    let bus = Arc::new(EventBus::new(128));
    let mut events = bus.subscribe();
    let entered = Arc::new(Notify::new());
    let mut executor = ToolExecutor::new(bus.clone());
    executor
        .register(Arc::new(BlockedTool(entered.clone())))
        .unwrap();
    let model = Arc::new(ScriptedModel::new([Ok(tool_response(
        "blocked-write",
        "write",
        json!({}),
    ))]));
    let runtime = AgentRuntime::new(bus, Arc::new(executor), model)
        .with_run_store(RunStore::open(&fixture.config, fixture._storage.handle()).unwrap());
    let run =
        runtime.delegate_background(Role::Worker, "blocked write".into(), RunConfig::default());
    timeout(DEADLINE, entered.notified()).await.unwrap();
    runtime.stop(run, StopScope::SelfOnly).unwrap();
    assert_eq!(
        timeout(DEADLINE, runtime.wait(run)).await.unwrap().unwrap(),
        AgentRunPhase::Stopped
    );
    timeout(DEADLINE, async {
        loop {
            if let EventKind::Tool(event_bus::ToolEvent::ToolCompleted {
                call_id,
                output,
                is_error,
                ..
            }) = events.recv().await.unwrap().kind
                && call_id == "blocked-write"
            {
                assert!(is_error);
                assert_eq!(output.as_deref(), Some("stopped"));
                break;
            }
        }
    })
    .await
    .unwrap();
    let record = storage::Database::open(&fixture.config)
        .unwrap()
        .run_context(&run.to_string())
        .unwrap()
        .unwrap();
    assert_eq!(record.terminal_phase, "Stopped");
    let descriptor: serde_json::Value = serde_json::from_str(&record.config_json).unwrap();
    assert_eq!(
        descriptor["interrupted_tool_calls"][0]["call_id"],
        "blocked-write"
    );
    assert!(record.messages_json.contains("stopped"));
}
