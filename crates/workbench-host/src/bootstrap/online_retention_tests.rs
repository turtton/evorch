//! Real bootstrap, writer, event bridge and owner; only provider/sandbox are seams.
use super::{BootstrapOptions, BootstrappedHost, TestSeams};
use event_bus::{AgentMessageKind, AgentRunPhase, EventKind, LifecycleEvent};
use providers::{ChatResponse, ContentBlock, FinishReason, Message, Role as MessageRole};
use runtime::{AgentInvocationContext, AgentModel, Role, RunConfig, RuntimeError};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use storage::thread_retention::TombstoneSet;
use workspace_state::{ProjectId, SidebarDocument, SidebarStore, ThreadId, ThreadRecord};
const DAY_NS: i64 = 86_400_000_000_000;
struct CountedModel(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl AgentModel for CountedModel {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "local/test".into()
    }
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[Message],
        _: &[providers::ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ChatResponse {
            message: Message {
                role: MessageRole::Assistant,
                content: vec![ContentBlock::Text {
                    text: "done".into(),
                }],
            },
            usage: Default::default(),
            usage_reported: true,
            finish_reason: FinishReason::Stop,
        })
    }
}
struct FixtureSandbox;
impl runtime::SandboxFactory for FixtureSandbox {
    fn build(
        &self,
        _: &runtime::ExecutionPolicy,
        _: &runtime::WorkspaceMounts,
    ) -> Result<Arc<dyn sandbox::Sandbox>, sandbox::SandboxError> {
        Ok(Arc::new(sandbox::DirectSandbox::new_unchecked()))
    }
}

async fn lifecycle(interactive: bool) {
    let root = test_socket_dir::SplitDir::new();
    let config = root.path().join("config");
    std::fs::create_dir_all(&config).unwrap();
    let store = SidebarStore::new(config.join("sidebar.json"));
    let id = ThreadId::new("online-family");
    let mut doc = SidebarDocument::default();
    doc.sidebar
        .add_project(ProjectId::new("project"), "project", root.path())
        .unwrap();
    doc.sidebar.threads.push(ThreadRecord::new(
        id.clone(),
        ProjectId::new("project"),
        "online family",
    ));
    doc.sidebar.refresh_derived();
    store.write(&store.lock().unwrap(), &doc).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut core = BootstrappedHost::open_with_test_seams(
        BootstrapOptions {
            config_dir: config.clone(),
            ownership_root: root.sockets().into(),
            project_root: root.path().into(),
        },
        TestSeams {
            model: Arc::new(CountedModel(calls.clone())),
            sandbox: Arc::new(FixtureSandbox),
        },
    )
    .await
    .unwrap();
    core.enable_retention_producers_for_test().unwrap();
    let mut events = core.runtime.event_bus().subscribe();
    let permit = core.owner.start("online-family").unwrap();
    let run = core
        .runtime
        .delegate_chat(
            "online-family",
            Role::Worker,
            "hello".into(),
            RunConfig {
                interactive,
                keep_alive: interactive,
                ownership: Some(permit),
                ..Default::default()
            },
        )
        .unwrap();
    let expected = if interactive {
        AgentRunPhase::Waiting
    } else {
        AgentRunPhase::Done
    };
    loop {
        let event = events.recv().await.unwrap();
        if matches!(&event.kind, EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to, .. })
            if run_id == &run.to_string() && *to == expected)
        {
            break;
        }
    }
    if !interactive {
        core.runtime.wait_retention_task(run).await.unwrap();
    }
    core.bridge.as_mut().unwrap().flush().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // Canonical sidebar root, under the same mutation gate as workspace commands.
    {
        let _admission = core.runtime.retention_workspace_admission();
        let mut sidebar = store.read_unlocked().unwrap();
        sidebar
            .sidebar
            .threads
            .iter_mut()
            .find(|t| t.id == id)
            .unwrap()
            .root_run_id = Some(run.to_string());
        store.write(&store.lock().unwrap(), &sidebar).unwrap();
    }
    let db = storage::Database::open_for_inspection(&core.storage_config().db_path).unwrap();
    let read = rusqlite::Connection::open_with_flags(
        &core.storage_config().db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let latest: i64 = read
        .query_row(
            "SELECT COALESCE(MAX(wall_clock_ns), 0) FROM events",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let archive_now = latest.checked_add(8 * DAY_NS).unwrap();
    for _ in 0..12 {
        core.tick_retention_for_test(archive_now).unwrap();
        if store.read_unlocked().unwrap().sidebar.resolve_states()[&id].archived {
            break;
        }
    }
    let archived_at = store.read_unlocked().unwrap().sidebar.resolve_states()[&id]
        .archived_at
        .expect("automatic archive");
    let due = archived_at
        .checked_mul(1_000_000_000)
        .unwrap()
        .checked_add(
            i64::from(
                core.config
                    .storage
                    .thread_retention
                    .archived_delete_after_days,
            ) * DAY_NS,
        )
        .unwrap();
    let mut stopped = 0;
    let mut confirmed = 0;
    for _ in 0..12 {
        let report = core.tick_retention_for_test(due).unwrap();
        stopped += report.stopped_runs;
        confirmed += report.confirmed;
        if report.stopped_runs > 0 {
            core.runtime.wait_retention_task(run).await.unwrap();
            core.bridge.as_mut().unwrap().flush().unwrap();
        }
        if confirmed > 0 {
            break;
        }
    }
    assert_eq!(stopped, usize::from(interactive));
    assert_eq!(
        confirmed, 1,
        "deletion without advancing the clock after idle Stop"
    );
    let tombstones: TombstoneSet = db.tombstones().unwrap();
    assert!(tombstones.threads.contains("online-family"));
    assert!(tombstones.runs.contains(&run.to_string()));
    assert!(
        !store
            .read_unlocked()
            .unwrap()
            .sidebar
            .threads
            .iter()
            .any(|t| t.id == id)
    );
    // Exercise retained terminal entries, not an unload/restart shortcut.
    let before = calls.load(Ordering::SeqCst);
    let mut after_delete = core.runtime.event_bus().subscribe();
    assert!(core.runtime.inspect_agent(run).is_err());
    assert!(!core.runtime.list_agents().iter().any(|a| a.run_id == run));
    assert!(
        core.runtime
            .send_user_input(run, "late".into(), None)
            .is_err()
    );
    assert!(
        core.runtime
            .send_agent_message(run, run, AgentMessageKind::Send, "late", None)
            .is_err()
    );
    assert!(core.runtime.deliver_notice(run, "late").is_err());
    assert!(core.runtime.deliver_follow_ups_next_turn(run).is_err());
    assert!(core.runtime.compact(run).is_err());
    assert!(core.runtime.reserve_child_run_id(run).is_err());
    assert!(
        core.runtime
            .spawn_reserved(run, None, Role::Worker, "late", RunConfig::default())
            .is_err()
    );
    assert!(core.runtime.resume_chat(run, RunConfig::default()).is_err());
    assert!(
        core.runtime
            .delegate_chat(
                "online-family",
                Role::Worker,
                "late".into(),
                RunConfig::default()
            )
            .is_err()
    );
    assert_eq!(calls.load(Ordering::SeqCst), before);
    assert!(
        after_delete
            .drain_pending_snapshot()
            .iter()
            .all(|e| !matches!(e.kind, EventKind::Tool(_)))
    );
    let mut reclaimed = 0;
    for _ in 0..32 {
        let report = core.tick_reclaim_for_test().unwrap();
        reclaimed += report.rows_reclaimed;
        let remaining: i64 = read
            .query_row(
                "SELECT COUNT(*) FROM reclaim_jobs WHERE state != 'done'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        if remaining == 0 {
            break;
        }
    }
    assert!(reclaimed > 0);
    let remaining: i64 = read
        .query_row(
            "SELECT COUNT(*) FROM events WHERE run_id = ?1",
            [run.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(remaining, 0);
    core.storage_writer().checkpoint_now().unwrap();
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn online_retention_bootstrap_terminal_family() {
    lifecycle(false).await;
}
#[tokio::test]
async fn online_retention_bootstrap_idle_waiting_family() {
    lifecycle(true).await;
}
