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

#[derive(Debug, Default)]
struct Measurements {
    host_ns: u64,
    seal_ns: u64,
    stop_ns: u64,
    writer_ns: u64,
    foreground_baseline_ns: u64,
    foreground_concurrent_ns: u64,
    foreground_extra_ns: u64,
    holds: usize,
}
impl Measurements {
    fn observe(&mut self, r: &crate::thread_retention::RetentionReport) {
        self.host_ns = self.host_ns.max(r.host_lock_ns);
        self.seal_ns = self.seal_ns.max(r.seal_ns);
        self.stop_ns = self.stop_ns.max(r.idle_stop_ns);
        self.writer_ns = self.writer_ns.max(r.writer_roundtrip_ns);
        self.holds += r.holds.values().sum::<usize>();
    }
    fn query(host: &Arc<std::sync::Mutex<crate::CommandHost>>) {
        use workbench_protocol::{
            ids::SessionId,
            namespaces::workspace::WorkspaceQuery,
            queries::{PageRequest, Query},
        };
        host.lock()
            .unwrap()
            .handle_query(
                &SessionId::new("retention-measure").unwrap(),
                &Query::Workspace(WorkspaceQuery::ListProjects(PageRequest::default())),
                &crate::provenance::local_desktop(),
            )
            .unwrap();
    }
    fn tick(
        &mut self,
        core: &BootstrappedHost,
        now: i64,
        measure: bool,
    ) -> crate::thread_retention::RetentionReport {
        if !measure {
            return core.tick_retention_for_test(now).unwrap();
        }
        let host = core.host.clone();
        let ns = |start: std::time::Instant| {
            u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX)
        };
        let start = std::time::Instant::now();
        Self::query(&host);
        let baseline_ns = ns(start);
        self.foreground_baseline_ns = self.foreground_baseline_ns.max(baseline_ns);
        // A bounded pair of real requests, not an unbounded load generator. The
        // barrier only starts them together; there is no artificial seal delay.
        let ready = std::sync::Barrier::new(2);
        let (mut report, foreground_ns) = std::thread::scope(|scope| {
            let query = scope.spawn(|| {
                ready.wait();
                let start = std::time::Instant::now();
                Self::query(&host);
                ns(start)
            });
            ready.wait();
            let report = core.tick_retention_for_test(now).unwrap();
            (report, query.join().unwrap())
        });
        self.foreground_concurrent_ns = self.foreground_concurrent_ns.max(foreground_ns);
        self.foreground_extra_ns = self
            .foreground_extra_ns
            .max(foreground_ns.saturating_sub(baseline_ns));
        self.observe(&report);
        if report.holds.contains_key("ForegroundPending") {
            // Record the contention, then allow a bounded uncontended tick to
            // progress. Performance runs must not depend on which thread wins.
            let next = core.tick_retention_for_test(now).unwrap();
            self.observe(&next);
            report.confirmed += next.confirmed;
            report.stopped_runs += next.stopped_runs;
        }
        report
    }
}

async fn lifecycle(interactive: bool, unrelated: usize, pin_after_stop: bool) -> Measurements {
    let mut measurements = Measurements::default();
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
    for i in 0..unrelated {
        let other = ThreadId::new(&format!("zz-unrelated-{i:04}"));
        doc.sidebar.threads.push(ThreadRecord::new(
            other.clone(),
            ProjectId::new("project"),
            "unrelated",
        ));
        doc.sidebar.set_family_pinned(&other, true).unwrap();
    }
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
            "hello ".repeat(4_096),
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
    if interactive && unrelated == 0 {
        // Input is accepted first, without yielding to its consumer. A completed
        // admission still invalidates Stop's preflight, and its turn runs normally.
        let before = [String::new(), "t:online-family".into(), format!("r:{run}")]
            .into_iter()
            .map(|scope| {
                let stamp = core.runtime.retention_gate_stamp(&scope);
                (scope, stamp)
            })
            .collect::<Vec<_>>();
        core.runtime
            .send_user_input(run, "accepted first".into(), None)
            .unwrap();
        assert!(matches!(
            core.runtime.stop_retention_idle(run, &before),
            Err(runtime::RuntimeError::RestartBusy)
        ));
        loop {
            let event = events.recv().await.unwrap();
            if matches!(&event.kind, EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to: AgentRunPhase::Waiting, .. }) if run_id == &run.to_string())
            {
                break;
            }
        }
        core.bridge.as_mut().unwrap().flush().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
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
        measurements.tick(&core, archive_now, unrelated > 0);
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
    // An attachment left behind by a failed archive cleanup. Register before
    // publishing the fixture file, just like the production attachment writer.
    let attachments = core.runtime.attachment_root().unwrap();
    let files = attachments.files("online-family").unwrap();
    {
        let _admission = core.runtime.retention_workspace_admission();
        std::fs::write(
            files.path().join("left-after-archive.txt"),
            b"retained attachment",
        )
        .unwrap();
    }
    let pages_before: i64 = read
        .query_row("PRAGMA page_count", [], |r| r.get(0))
        .unwrap();
    let mut stopped = 0;
    let mut confirmed = 0;
    for _ in 0..12 {
        let report = measurements.tick(&core, due, unrelated > 0);
        stopped += report.stopped_runs;
        confirmed += report.confirmed;
        if report.stopped_runs > 0 {
            core.runtime.wait_retention_task(run).await.unwrap();
            core.bridge.as_mut().unwrap().flush().unwrap();
            if pin_after_stop {
                assert_eq!(confirmed, 0, "Stop is not a deletion commit");
                let _admission = core.runtime.retention_workspace_admission();
                let mut sidebar = store.read_unlocked().unwrap();
                sidebar.sidebar.set_family_pinned(&id, true).unwrap();
                store.write(&store.lock().unwrap(), &sidebar).unwrap();
                drop(_admission);
                let calls_after_stop = calls.load(Ordering::SeqCst);
                for _ in 0..4 {
                    assert_eq!(measurements.tick(&core, due, false).confirmed, 0);
                }
                assert!(!db.tombstones().unwrap().threads.contains("online-family"));
                assert_eq!(
                    core.runtime.inspect_agent(run).unwrap().phase,
                    AgentRunPhase::Stopped
                );
                assert_eq!(
                    calls.load(Ordering::SeqCst),
                    calls_after_stop,
                    "a failed confirmation never auto-resumes"
                );
                core.shutdown().await.unwrap();
                return measurements;
            }
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
    assert!(core.runtime.wake_chat(run, RunConfig::default()).is_err());
    assert!(
        core.runtime
            .continue_goal(run, "late".into(), RunConfig::default())
            .is_err()
    );
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
    for _ in 0..2 {
        measurements.tick(&core, due, unrelated > 0);
    }
    assert!(
        !files.path().exists(),
        "tombstone-keyed attachment recovery survives sidebar removal"
    );
    let remaining: i64 = read
        .query_row(
            "SELECT COUNT(*) FROM events WHERE run_id = ?1",
            [run.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(remaining, 0);
    core.storage_writer().checkpoint_now().unwrap();
    let pages_after: i64 = read
        .query_row("PRAGMA page_count", [], |r| r.get(0))
        .unwrap();
    assert!(
        pages_after < pages_before,
        "existing budgeted vacuum reclaims pages: {pages_before} -> {pages_after}"
    );
    core.shutdown().await.unwrap();
    measurements
}

#[tokio::test]
async fn online_retention_bootstrap_terminal_family() {
    lifecycle(false, 0, false).await;
}
#[tokio::test]
async fn online_retention_bootstrap_idle_waiting_family() {
    lifecycle(true, 0, false).await;
}

#[tokio::test]
async fn online_retention_pin_after_idle_stop_never_auto_resumes() {
    lifecycle(true, 0, true).await;
}

/// Synthetic, warm-cache workbench-host measurement; deliberately no wall-clock
/// assertions. Run serially with --ignored --nocapture; never touches real XDG.
#[tokio::test]
#[ignore = "manual host/fence/foreground performance measurement"]
async fn online_retention_host_measurement() {
    for threads in [1_000, 2_000] {
        for repetition in 0..6 {
            let m = lifecycle(true, threads - 1, false).await;
            eprintln!("retention_host threads={threads} repetition={repetition} {m:?}");
        }
    }
}
