use super::*;
use event_bus::lane::{LaneNudgeState, LaneStallClassification as Class, LaneStallEpisode};
use event_bus::{Event, ToolEvent};
const MINUTE: i64 = 60_000_000_000;
struct OwnedHost {
    thread: String,
    target: LaneDeliveryTarget,
    now: AtomicI64,
    restores: AtomicUsize,
    fail: AtomicBool,
}
impl LaneDeliveryHost for OwnedHost {
    fn resolve(&self, thread: &str) -> Result<LaneDeliveryTarget, LaneDeliveryNack> {
        if thread == self.thread {
            Ok(self.target.clone())
        } else {
            Err(LaneDeliveryNack::UnknownThread)
        }
    }
    fn now_ns(&self) -> i64 {
        self.now.load(Ordering::SeqCst)
    }
    fn restore_root(
        &self,
        runtime: &AgentRuntime,
        target: &LaneDeliveryTarget,
    ) -> Result<(), LaneDeliveryNack> {
        self.restores.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            return Err(LaneDeliveryNack::RestoreFailed);
        }
        runtime
            .wake_chat(target.root_run_id, target.authority.clone())
            .map(|_| ())
            .map_err(|_| LaneDeliveryNack::RestoreFailed)
    }
}
fn owned(
    f: &Fixture,
    lane: &crate::lanes::LaneOpened,
) -> (Arc<crate::ownership::OwnerHost>, Arc<OwnedHost>) {
    let owner = Arc::new(
        crate::ownership::OwnerHost::open(
            f._dir.path(),
            Default::default(),
            f.runtime.shared.bus.clone(),
        )
        .unwrap(),
    );
    let permit = owner.start(&lane.thread_id).unwrap();
    let host = Arc::new(OwnedHost {
        thread: lane.thread_id.clone(),
        target: LaneDeliveryTarget {
            root_run_id: lane.root_run_id,
            generation: 1,
            authority: RunConfig {
                ownership: Some(permit),
                conversation: true,
                ..Default::default()
            },
        },
        now: AtomicI64::new(
            f.storage
                .handle()
                .lane(&lane.lane_id)
                .unwrap()
                .unwrap()
                .created_at_ns,
        ),
        restores: AtomicUsize::new(0),
        fail: AtomicBool::new(false),
    });
    *f.runtime.shared.lanes.delivery.host.lock().unwrap() = Some(host.clone());
    f.runtime.start_lane_monitor();
    (owner, host)
}
fn episodes(f: &Fixture) -> Vec<LaneStallEpisode> {
    let conn = rusqlite::Connection::open(&f.config.db_path).unwrap();
    conn.prepare("SELECT payload FROM lane_stall_episodes ORDER BY episode_id")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| serde_json::from_str(&r.unwrap()).unwrap())
        .collect()
}
#[tokio::test(start_paused = true)]
async fn no_report_and_work_stalled_boundaries_and_live_nudge_share_one_slot() {
    let mut f = Fixture::new();
    let (lead, _) = f.lead().await;
    let lane = f.lane(lead, "boundaries").await;
    let (_owner, host) = owned(&f, &lane);
    let base = host.now_ns();
    f.runtime.monitor_lanes().unwrap();
    host.now.store(base + 30 * MINUTE - 1, Ordering::SeqCst);
    f.runtime.monitor_lanes().unwrap();
    assert!(episodes(&f).is_empty());
    host.now.store(base + 30 * MINUTE, Ordering::SeqCst);
    f.runtime.monitor_lanes().unwrap();
    assert_eq!(episodes(&f)[0].classification, Class::NoReport);
    host.now.store(base + 30 * MINUTE + 1, Ordering::SeqCst);
    f.runtime.monitor_lanes().unwrap();
    assert_eq!(episodes(&f).len(), 1);
    assert_eq!(
        f.runtime
            .drive_lane_deliveries(&lane.thread_id)
            .await
            .unwrap(),
        LaneDeliveryOutcome::Delivered { notifications: 1 }
    );
    f.phase(lane.root_run_id, AgentRunPhase::Waiting).await;
    f.runtime.monitor_lanes().unwrap(); // observe the provider/tool exit at this clock boundary
    let initial = episodes(&f)[0].clone();
    assert_eq!(initial.nudge_state, LaneNudgeState::Accepted);
    assert_eq!(f.receipts(), 1);
    host.now.store(base + 60 * MINUTE - 1, Ordering::SeqCst);
    f.runtime.monitor_lanes().unwrap();
    assert_eq!(episodes(&f)[0].classification, Class::NoReport);
    host.now.store(base + 60 * MINUTE, Ordering::SeqCst);
    f.runtime.monitor_lanes().unwrap();
    assert_eq!(episodes(&f)[0].classification, Class::WorkStalled);
    host.now.store(base + 60 * MINUTE + 1, Ordering::SeqCst);
    f.runtime.monitor_lanes().unwrap();
    assert_eq!(
        f.runtime
            .drive_lane_deliveries(&lane.thread_id)
            .await
            .unwrap(),
        LaneDeliveryOutcome::Empty
    );
    assert_eq!(f.receipts(), 1);
    assert_eq!(episodes(&f)[0].episode_id, initial.episode_id);
    // No new human input: that would legitimately create a new anchor/slot.
    f.model.finish.store(true, Ordering::SeqCst);
    f.runtime
        .send_internal_message(lane.root_run_id, "Finish this existing turn".into())
        .unwrap();
    f.runtime.wait(lane.root_run_id).await.unwrap();
    f.runtime.monitor_lanes().unwrap();
    host.now.fetch_add(5 * MINUTE, Ordering::SeqCst);
    f.runtime.monitor_lanes().unwrap();
    assert_eq!(episodes(&f)[0].classification, Class::SilentDone);
    assert_eq!(episodes(&f)[0].nudge_state, LaneNudgeState::Accepted);
    assert_eq!(episodes(&f)[0].episode_id, initial.episode_id);
    assert_eq!(
        f.runtime
            .drive_lane_deliveries(&lane.thread_id)
            .await
            .unwrap(),
        LaneDeliveryOutcome::Empty
    );
    assert_eq!(host.restores.load(Ordering::SeqCst), 0);
    assert_eq!(f.receipts(), 1);
    assert_eq!(
        f.storage
            .handle()
            .lane_wake_guard(lane.lane_id)
            .unwrap()
            .no_progress_batches,
        0
    );
}
#[tokio::test(start_paused = true)]
async fn long_tool_exemption_and_five_minute_grace_do_not_create_progress_anchor() {
    let mut f = Fixture::new();
    let (lead, _) = f.lead().await;
    let lane = f.lane(lead, "tool-grace").await;
    let (_owner, host) = owned(&f, &lane);
    let base = host.now_ns();
    f.runtime.monitor_lanes().unwrap();
    let anchor = f
        .storage
        .handle()
        .lane_progress_anchor(lane.lane_id.clone())
        .unwrap();
    f.runtime
        .shared
        .bus
        .emit(Event::new(event_bus::ToolEvent::ToolStarted {
            tool_name: "shell".into(),
            call_id: "long".into(),
            input: None,
            run_id: Some(lane.root_run_id.to_string()),
            parent_call_id: None,
        }));
    host.now.store(base + 120 * MINUTE, Ordering::SeqCst);
    f.runtime.monitor_lanes().unwrap();
    assert!(episodes(&f).is_empty());
    f.runtime
        .shared
        .bus
        .emit(Event::new(event_bus::ToolEvent::ToolCompleted {
            tool_name: "shell".into(),
            call_id: "long".into(),
            is_error: false,
            output: None,
            detail: None,
            run_id: Some(lane.root_run_id.to_string()),
            parent_call_id: None,
        }));
    f.runtime.monitor_lanes().unwrap();
    host.now.store(base + 125 * MINUTE - 1, Ordering::SeqCst);
    f.runtime.monitor_lanes().unwrap();
    assert!(episodes(&f).is_empty());
    host.now.store(base + 125 * MINUTE, Ordering::SeqCst);
    f.runtime.monitor_lanes().unwrap();
    assert_eq!(episodes(&f)[0].classification, Class::WorkStalled);
    assert_eq!(
        f.storage
            .handle()
            .lane_progress_anchor(lane.lane_id)
            .unwrap(),
        anchor
    );
}
#[tokio::test(start_paused = true)]
async fn silent_done_wakes_once_despite_three_different_working_reports_and_restart_observation() {
    let mut f = Fixture::new();
    let (lead, _) = f.lead().await;
    let lane = f.lane(lead, "silent").await;
    let (_owner, host) = owned(&f, &lane);
    f.finish(lane.root_run_id).await;
    f.runtime.monitor_lanes().unwrap();
    assert_eq!(episodes(&f)[0].classification, Class::SilentDone);
    assert_eq!(
        f.runtime
            .drive_lane_deliveries(&lane.thread_id)
            .await
            .unwrap(),
        LaneDeliveryOutcome::Delivered { notifications: 1 }
    );
    f.runtime.wait(lane.root_run_id).await.unwrap();
    let anchor = f
        .storage
        .handle()
        .lane_progress_anchor(lane.lane_id.clone())
        .unwrap();
    for n in 0..3 {
        // Explicit test replays, not automatic nudges or real user input. Exercise
        // actual restore, lane_report and finish, rather than editing the ledger.
        f.model.next_tools.lock().unwrap().insert(lane.root_run_id.to_string(), ContentBlock::ToolUse {
            id: format!("working-{n}"), name: "lane_report".into(),
            input: serde_json::json!({"status":"working","summary":format!("different prose {n}"),"artifact_refs":[]}),
        });
        f.runtime
            .wake_chat(lane.root_run_id, host.target.authority.clone())
            .unwrap();
        f.runtime
            .send_internal_message(lane.root_run_id, "Replay a working-only turn".into())
            .unwrap();
        f.runtime.wait(lane.root_run_id).await.unwrap();
        assert_eq!(
            f.runtime.inspect_agent(lane.root_run_id).unwrap().phase,
            AgentRunPhase::Done
        );
        assert_eq!(
            f.storage
                .handle()
                .lane_progress_anchor(lane.lane_id.clone())
                .unwrap(),
            anchor
        );
        assert_eq!(
            f.storage
                .handle()
                .lane_reports(&lane.lane_id, 0, 20)
                .unwrap()
                .last()
                .unwrap()
                .summary,
            format!("different prose {n}")
        );
        *f.runtime.shared.lanes.delivery.monitor.lock().unwrap() = Default::default();
        f.runtime.start_lane_monitor();
        f.runtime.monitor_lanes().unwrap();
        assert_eq!(
            f.runtime
                .drive_lane_deliveries(&lane.thread_id)
                .await
                .unwrap(),
            LaneDeliveryOutcome::Empty
        );
    }
    let restarted = AgentRuntime::new(
        f.runtime.shared.bus.clone(),
        Arc::new(tools::ToolExecutor::new(f.runtime.shared.bus.clone())),
        f.model.clone(),
    )
    .with_run_store(crate::RunStore::open(&f.config, f.storage.handle()).unwrap());
    restarted
        .bind_thread_root(&lane.thread_id, lane.root_run_id)
        .unwrap();
    *restarted.shared.lanes.delivery.host.lock().unwrap() = Some(host.clone());
    restarted.start_lane_monitor();
    restarted.monitor_lanes().unwrap();
    assert_eq!(
        restarted
            .drive_lane_deliveries(&lane.thread_id)
            .await
            .unwrap(),
        LaneDeliveryOutcome::Empty
    );
    assert_eq!(host.restores.load(Ordering::SeqCst), 1);
    assert_eq!(episodes(&f).len(), 1);
    assert_eq!(f.receipts(), 1);
    assert!(
        !f.runtime
            .shared
            .auto_wakes
            .lock()
            .unwrap()
            .contains_key(&lane.root_run_id)
    );
}
#[tokio::test(start_paused = true)]
async fn nonowner_process_cannot_observe_or_fabricate_stall() {
    const CHILD_DB: &str = "EVORCH_2B_NONOWNER_DB";
    const CHILD_THREAD: &str = "EVORCH_2B_NONOWNER_THREAD";
    if let Some(db) = std::env::var_os(CHILD_DB) {
        let config = storage::StorageConfig {
            db_path: db.into(),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let thread = std::env::var(CHILD_THREAD).unwrap();
        let lane = storage.handle().lane_by_thread(&thread).unwrap().unwrap();
        let root = lane.root_run_id.as_deref().unwrap().parse().unwrap();
        let bus = Arc::new(EventBus::new(128));
        let owner = crate::ownership::OwnerHost::open(
            config.db_path.parent().unwrap(),
            Default::default(),
            bus.clone(),
        )
        .unwrap();
        assert!(
            owner.start(&thread).is_err(),
            "the parent process retains the owner lease"
        );
        let model = Arc::new(Model::default());
        let foreign = AgentRuntime::new(
            bus.clone(),
            Arc::new(tools::ToolExecutor::new(bus)),
            model.clone(),
        )
        .with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
        foreign.bind_thread_root(&thread, root).unwrap();
        *foreign.shared.lanes.delivery.host.lock().unwrap() = Some(Arc::new(OwnedHost {
            target: LaneDeliveryTarget {
                root_run_id: root,
                generation: lane.generation,
                authority: RunConfig::default(),
            },
            thread,
            now: AtomicI64::new(lane.created_at_ns + 120 * MINUTE),
            restores: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
        }));
        foreign.monitor_lanes().unwrap();
        let count: i64 = rusqlite::Connection::open(&config.db_path)
            .unwrap()
            .query_row("SELECT count(*) FROM lane_stall_episodes", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
        assert!(model.requests.lock().unwrap().is_empty());
        return;
    }
    let mut f = Fixture::new();
    let (lead, _) = f.lead().await;
    let lane = f.lane(lead, "foreign").await;
    let (owner, host) = owned(&f, &lane);
    let second = crate::ownership::OwnerHost::open(
        f._dir.path(),
        Default::default(),
        f.runtime.shared.bus.clone(),
    )
    .unwrap();
    assert!(second.owned_permit(&lane.thread_id).is_err());
    let foreign = AgentRuntime::new(
        f.runtime.shared.bus.clone(),
        Arc::new(tools::ToolExecutor::new(f.runtime.shared.bus.clone())),
        f.model.clone(),
    )
    .with_run_store(crate::RunStore::open(&f.config, f.storage.handle()).unwrap());
    foreign
        .bind_thread_root(&lane.thread_id, lane.root_run_id)
        .unwrap();
    // A reader knows the binding but has no OwnerPermit; no ownership DTO can substitute.
    let readonly = Arc::new(OwnedHost {
        target: LaneDeliveryTarget {
            authority: RunConfig::default(),
            ..host.target.clone()
        },
        thread: lane.thread_id.clone(),
        now: AtomicI64::new(host.now_ns() + 120 * MINUTE),
        restores: AtomicUsize::new(0),
        fail: AtomicBool::new(false),
    });
    *foreign.shared.lanes.delivery.host.lock().unwrap() = Some(readonly);
    foreign.monitor_lanes().unwrap();
    assert!(episodes(&f).is_empty());
    // Exercise the real OS lease and the shared DB from a second process, not
    // merely a second runtime in this process. Process exit is the barrier.
    let child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "lane_delivery::tests::monitor::nonowner_process_cannot_observe_or_fabricate_stall",
            "--nocapture",
        ])
        .env(CHILD_DB, &f.config.db_path)
        .env(CHILD_THREAD, &lane.thread_id)
        .output()
        .await
        .unwrap();
    assert!(
        child.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
    assert!(episodes(&f).is_empty());
    assert!(owner.owned_permit(&lane.thread_id).is_ok());
    f.runtime.monitor_lanes().unwrap();
    host.now.fetch_add(30 * MINUTE, Ordering::SeqCst);
    f.runtime.monitor_lanes().unwrap();
    assert_eq!(episodes(&f).len(), 1);
}
#[tokio::test(start_paused = true)]
async fn worker_startup_retry_skip_and_weak_shutdown_are_real_lifecycle_boundaries() {
    let mut f = Fixture::new();
    let (lead, host) = f.lead().await;
    let lane = f.lane(lead, "timer").await;
    f.report(&lane, Some(1), LaneStatus::Ready);
    *host.deny.lock().unwrap() = Some(LaneDeliveryNack::Readonly);
    *f.runtime.shared.lanes.delivery.host.lock().unwrap() = None;
    let (tx, mut steps) = tokio::sync::mpsc::unbounded_channel();
    *f.runtime.shared.lanes.delivery.worker_steps.lock().unwrap() = Some(tx);
    let worker = f.runtime.bind_lane_delivery_host(host.clone()).unwrap();
    let startup = steps.recv().await.unwrap();
    let start = startup.scheduled;
    startup.release.send(()).unwrap();
    startup.completed.await.unwrap();
    assert_eq!(f.receipts(), 0);
    *host.deny.lock().unwrap() = None;
    tokio::time::advance(Duration::from_secs(30)).await;
    let retry = steps.recv().await.unwrap();
    assert_eq!(retry.scheduled - start, Duration::from_secs(30));
    retry.release.send(()).unwrap();
    retry.completed.await.unwrap();
    assert_eq!(f.receipts(), 1);
    let held = steps.recv().await.unwrap();
    tokio::time::advance(Duration::from_secs(301)).await;
    let release_at = tokio::time::Instant::now();
    held.release.send(()).unwrap();
    held.completed.await.unwrap();
    let missed = steps.recv().await.unwrap();
    missed.release.send(()).unwrap();
    missed.completed.await.unwrap();
    let skipped = steps.recv().await.unwrap();
    assert!(
        skipped.scheduled > release_at,
        "must Skip backlog, not burst through missed deadlines"
    );
    f.runtime.stop(lead, crate::StopScope::Subtree).unwrap();
    f.runtime.wait(lead).await.unwrap();
    f.runtime
        .stop(lane.root_run_id, crate::StopScope::Subtree)
        .unwrap();
    f.runtime.wait(lane.root_run_id).await.unwrap();
    let weak = f.runtime.downgrade();
    drop(f);
    assert!(weak.upgrade().is_none());
    skipped.release.send(()).unwrap();
    worker.await.unwrap();
    assert!(steps.recv().await.is_none());
}

#[tokio::test(start_paused = true)]
async fn waiting_question_pause_summary_and_reported_terminal_do_not_gain_nudges() {
    for kind in [
        "question",
        "pause",
        "summary",
        "ready",
        "stop",
        "provider",
        "compaction",
        "approval",
        "child",
    ] {
        let mut f = Fixture::new();
        let (lead, _) = f.lead().await;
        let lane = if kind == "summary" {
            let lane = f
                .runtime
                .open_lane(
                    lead,
                    kind,
                    event_bus::LaneBrief {
                        verification_requirements: Default::default(),
                        objective: kind.into(),
                        inputs: vec![],
                        deliverables: vec!["artifact".into()],
                        acceptance_criteria: vec!["review".into()],
                        constraints: vec![],
                        workspace: Default::default(),
                        report_policy: event_bus::LanePolicy::Summary,
                    },
                )
                .unwrap();
            f.phase(lane.root_run_id, AgentRunPhase::Waiting).await;
            lane
        } else {
            f.lane(lead, kind).await
        };
        let (_owner, host) = owned(&f, &lane);
        let base = host.now_ns();
        f.runtime.monitor_lanes().unwrap();
        let mut child = None;
        match kind {
            "compaction" | "approval" => {
                f.runtime
                    .shared
                    .bus
                    .emit(Event::new(event_bus::LifecycleEvent::RunProgress {
                        run_id: lane.root_run_id.to_string(),
                        activity: if kind == "compaction" {
                            event_bus::RunActivity::Compaction
                        } else {
                            event_bus::RunActivity::Tools
                        },
                        compaction_operation_id: None,
                        context: None,
                    }));
            }
            "child" => {
                f.model.block_next.store(true, Ordering::SeqCst);
                child = Some(
                    f.runtime
                        .delegate_background_as_child(
                            lane.root_run_id,
                            Role::Worker,
                            "healthy long provider",
                            RunConfig::default(),
                        )
                        .unwrap(),
                );
                f.model.blocked.notified().await;
            }
            "question" => f.question(&lane, 0),
            "pause" => {
                let goal = f
                    .runtime
                    .create_thread_goal(
                        &lane.thread_id,
                        lane.root_run_id,
                        "goal".into(),
                        vec!["review".into()],
                    )
                    .unwrap();
                f.runtime
                    .set_goal_checks_paused(&lane.thread_id, &goal.goal_id, true)
                    .unwrap();
            }
            "ready" => f.report(&lane, None, LaneStatus::Ready),
            "stop" => {
                f.runtime
                    .stop(lane.root_run_id, crate::StopScope::SelfOnly)
                    .unwrap();
                f.runtime.wait(lane.root_run_id).await.unwrap();
            }
            "provider" => {
                f.runtime
                    .shared
                    .bus
                    .emit(Event::new(event_bus::ProviderEvent::RequestStarted {
                        request_id: "long-provider".into(),
                        provider: "mock".into(),
                        profile: None,
                        protocol: "mock".into(),
                        model: "test".into(),
                        streaming: false,
                        run_id: Some(lane.root_run_id.to_string()),
                        reasoning_effort: None,
                        purpose: None,
                    }));
            }
            _ => {}
        }
        host.now.store(base + 30 * MINUTE, Ordering::SeqCst);
        f.runtime.monitor_lanes().unwrap();
        if kind == "question" {
            let e = episodes(&f);
            assert_eq!(e[0].classification, Class::WaitingQuestion);
            assert_eq!(e[0].nudge_state, LaneNudgeState::NotRequested);
        } else {
            assert!(episodes(&f).is_empty(), "unexpected {kind} stall");
        }
        assert_eq!(host.restores.load(Ordering::SeqCst), 0);
        assert_eq!(f.receipts(), 0);
        if let Some(child) = child {
            f.runtime.cancel(child).unwrap();
            f.runtime.wait(child).await.unwrap();
        }
    }
}
#[tokio::test(start_paused = true)]
async fn permanent_restore_failure_retires_nudge_and_restart_cannot_wake_it_again() {
    let mut f = Fixture::new();
    let (lead, _) = f.lead().await;
    let lane = f.lane(lead, "failure").await;
    let (_owner, host) = owned(&f, &lane);
    f.finish(lane.root_run_id).await;
    f.runtime.monitor_lanes().unwrap();
    host.fail.store(true, Ordering::SeqCst);
    assert_eq!(
        f.runtime
            .drive_lane_deliveries(&lane.thread_id)
            .await
            .unwrap(),
        LaneDeliveryOutcome::Nacked(LaneDeliveryNack::RestoreFailed)
    );
    let failed = episodes(&f)[0].clone();
    assert_eq!(failed.nudge_state, LaneNudgeState::Failed);
    assert_eq!(failed.classification, Class::RestoreUnavailable);
    let last_nack: String = rusqlite::Connection::open(&f.config.db_path)
        .unwrap()
        .query_row(
            "SELECT last_nack FROM lane_delivery_destinations WHERE thread_id=?1",
            [&lane.thread_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<LaneDeliveryNack>(&last_nack).unwrap(),
        LaneDeliveryNack::RestoreFailed
    );
    assert_eq!(f.receipts(), 0);
    // New runtime and reopened RunStore: not merely a fresh in-memory timer.
    let runtime = AgentRuntime::new(
        f.runtime.shared.bus.clone(),
        Arc::new(tools::ToolExecutor::new(f.runtime.shared.bus.clone())),
        f.model.clone(),
    )
    .with_run_store(crate::RunStore::open(&f.config, f.storage.handle()).unwrap());
    runtime
        .bind_thread_root(&lane.thread_id, lane.root_run_id)
        .unwrap();
    *runtime.shared.lanes.delivery.host.lock().unwrap() = Some(host.clone());
    runtime.start_lane_monitor();
    runtime.monitor_lanes().unwrap();
    assert_eq!(
        runtime
            .drive_lane_deliveries(&lane.thread_id)
            .await
            .unwrap(),
        LaneDeliveryOutcome::Empty
    );
    assert_eq!(host.restores.load(Ordering::SeqCst), 1);
    assert_eq!(episodes(&f).len(), 1);
}

#[tokio::test(start_paused = true)]
async fn accepted_but_unpresented_nudge_error_is_attention_not_automatic_restore() {
    let mut f = Fixture::new();
    let (lead, _) = f.lead().await;
    let lane = f.lane(lead, "unpresented-error").await;
    let (_owner, host) = owned(&f, &lane);
    f.runtime.monitor_lanes().unwrap();
    host.now.fetch_add(30 * MINUTE, Ordering::SeqCst);
    f.runtime.monitor_lanes().unwrap();
    f.model.fail_next.store(true, Ordering::SeqCst);
    let gate = Arc::new(tokio::sync::Barrier::new(2));
    let drive = AFTER_RECEIPT_BARRIER.scope(
        gate.clone(),
        f.runtime.drive_lane_deliveries(&lane.thread_id),
    );
    let fail = async {
        gate.wait().await;
        f.runtime.wait(lane.root_run_id).await.unwrap();
        gate.wait().await;
    };
    let (outcome, ()) = tokio::join!(drive, fail);
    assert_eq!(
        outcome.unwrap(),
        LaneDeliveryOutcome::Delivered { notifications: 1 }
    );
    assert_eq!(
        f.runtime.inspect_agent(lane.root_run_id).unwrap().phase,
        AgentRunPhase::Error
    );
    assert!(
        !f.credits(lane.root_run_id).lane_presentations[0]
            .nudge_episode_ids
            .is_empty()
    );
    f.runtime.monitor_lanes().unwrap();
    assert!(
        episodes(&f)
            .iter()
            .any(|e| e.classification == Class::RestoreUnavailable && e.resolved_at_ns.is_none())
    );
    assert_eq!(
        f.runtime
            .drive_lane_deliveries(&lane.thread_id)
            .await
            .unwrap(),
        LaneDeliveryOutcome::Empty
    );
    assert_eq!(host.restores.load(Ordering::SeqCst), 0);
    assert_eq!(f.receipts(), 1);
}

#[tokio::test(start_paused = true)]
async fn resolved_claim_is_obsolete_not_a_synthetic_restore_failure() {
    let mut f = Fixture::new();
    let (lead, _) = f.lead().await;
    let lane = f.lane(lead, "resolved-claim").await;
    let (_owner, host) = owned(&f, &lane);
    f.runtime.monitor_lanes().unwrap();
    host.now.fetch_add(30 * MINUTE, Ordering::SeqCst);
    f.runtime.monitor_lanes().unwrap();
    let destination = f
        .storage
        .handle()
        .lane_delivery_destination(lane.thread_id.clone())
        .unwrap()
        .unwrap();
    let claimed = f
        .storage
        .handle()
        .claim_lane_deliveries(storage::lane_delivery::LaneClaimRequest {
            destination_thread_id: lane.thread_id.clone(),
            expected_destination_epoch: destination.epoch,
            owner: host.target.owner(),
            token: "resolved-nudge-claim".into(),
            now_ns: host.now_ns(),
            lease_until_ns: host.now_ns() + MINUTE,
            allow_non_waking: true,
        })
        .unwrap();
    let storage::lane_delivery::LaneClaimResult::Claimed(claim) = claimed else {
        panic!("expected nudge claim")
    };
    f.report(&lane, None, LaneStatus::Ready);
    assert_eq!(
        f.runtime.check_nudge_target(&host.target, &claim, false),
        Err(LaneDeliveryNack::Stopped)
    );
    assert_eq!(
        f.runtime.check_nudge_target(&host.target, &claim, true),
        Err(LaneDeliveryNack::Stopped)
    );
    assert!(episodes(&f)[0].resolved_at_ns.is_some());
    assert_eq!(
        f.storage
            .handle()
            .lane_reports(&lane.lane_id, 0, 20)
            .unwrap()
            .last()
            .unwrap()
            .status,
        LaneStatus::Ready
    );
    assert_eq!(f.receipts(), 0);
    assert_eq!(host.restores.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn provider_exit_before_startup_scan_still_grants_exact_five_minute_grace() {
    let mut f = Fixture::new();
    let (lead, _) = f.lead().await;
    let lane = f.lane(lead, "provider-exit").await;
    let (_owner, host) = owned(&f, &lane);
    let base = host.now_ns();
    host.now.store(base + 120 * MINUTE, Ordering::SeqCst);
    // The monitor bound after RequestStarted. Completion is still an explicit
    // exemption-exit signal; lack of the start event is not 120 minutes of idle.
    f.runtime
        .shared
        .bus
        .emit(Event::new(event_bus::ProviderEvent::RequestCompleted {
            request_id: "pre-bind-request".into(),
            provider: "test".into(),
            profile: None,
            protocol: "test".into(),
            model: "test".into(),
            streaming: false,
            duration_ms: 0,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            finish_reason: "stop".into(),
            run_id: Some(lane.root_run_id.to_string()),
            reasoning_tokens: None,
            purpose: None,
            service_tier: None,
            usage_reported: true,
        }));
    f.runtime.monitor_lanes().unwrap();
    assert!(episodes(&f).is_empty());
    host.now.store(base + 125 * MINUTE - 1, Ordering::SeqCst);
    f.runtime.monitor_lanes().unwrap();
    assert!(episodes(&f).is_empty());
    host.now.store(base + 125 * MINUTE, Ordering::SeqCst);
    f.runtime.monitor_lanes().unwrap();
    assert_eq!(episodes(&f)[0].classification, Class::NoReport);
}

#[tokio::test(start_paused = true)]
async fn monitor_change_hints_requery_committed_state_without_timer_heartbeats() {
    let mut f = Fixture::new();
    let (lead, _) = f.lead().await;
    let lane = f.lane(lead, "attention-hint").await;
    let (_owner, host) = owned(&f, &lane);
    let mut events = f.runtime.shared.bus.subscribe();
    let hints = |events: &mut event_bus::EventReceiver| {
        events
            .drain_pending_snapshot()
            .into_iter()
            .filter_map(|e| match e.kind {
                EventKind::Diagnostic(d)
                    if d.source == "lane_monitor" && d.code == "lane_stall_changed" =>
                {
                    Some(d)
                }
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    f.runtime.monitor_lanes().unwrap();
    assert!(hints(&mut events).is_empty());
    host.now.fetch_add(30 * MINUTE, Ordering::SeqCst);
    f.runtime.monitor_lanes().unwrap();
    let first = hints(&mut events);
    assert_eq!(first.len(), 1);
    assert_eq!(
        first[0].run_id.as_deref(),
        Some(lane.root_run_id.to_string().as_str())
    );
    assert_eq!(first[0].thread_id.as_deref(), Some(lane.thread_id.as_str()));
    assert_eq!(episodes(&f)[0].classification, Class::NoReport);
    assert_eq!(episodes(&f)[0].nudge_state, LaneNudgeState::Pending);
    let revision = episodes(&f)[0].revision;
    f.runtime.monitor_lanes().unwrap();
    assert!(hints(&mut events).is_empty());
    assert_eq!(episodes(&f)[0].revision, revision);
    host.now.fetch_add(30 * MINUTE, Ordering::SeqCst);
    f.runtime.monitor_lanes().unwrap();
    assert_eq!(hints(&mut events).len(), 1);
    assert_eq!(episodes(&f)[0].classification, Class::WorkStalled);
    f.runtime.monitor_lanes().unwrap();
    assert!(hints(&mut events).is_empty());
    assert_eq!(
        f.runtime
            .drive_lane_deliveries(&lane.thread_id)
            .await
            .unwrap(),
        LaneDeliveryOutcome::Delivered { notifications: 1 }
    );
    let accepted: Vec<_> = events
        .drain_pending_snapshot()
        .into_iter()
        .filter_map(|e| match e.kind {
            EventKind::Diagnostic(d)
                if d.source == "lane_delivery" && d.code == "lane_nudge_accepted" =>
            {
                Some(d)
            }
            _ => None,
        })
        .collect();
    assert_eq!(accepted.len(), 1);
    assert_eq!(
        accepted[0].thread_id.as_deref(),
        Some(lane.thread_id.as_str())
    );
    assert_eq!(episodes(&f)[0].nudge_state, LaneNudgeState::Accepted);
    assert_eq!(
        f.runtime
            .drive_lane_deliveries(&lane.thread_id)
            .await
            .unwrap(),
        LaneDeliveryOutcome::Empty
    );
    assert!(
        !events
            .drain_pending_snapshot()
            .into_iter()
            .any(|e| matches!(e.kind, EventKind::Diagnostic(d) if d.code == "lane_nudge_accepted"))
    );
    f.report(&lane, None, LaneStatus::Ready);
    f.runtime.monitor_lanes().unwrap();
    assert_eq!(
        hints(&mut events).len(),
        1,
        "resolution also re-queries attention"
    );
    assert!(episodes(&f)[0].resolved_at_ns.is_some());
    assert_eq!(episodes(&f)[0].nudge_state, LaneNudgeState::Accepted);
    f.runtime.monitor_lanes().unwrap();
    assert!(hints(&mut events).is_empty());
}

#[tokio::test(start_paused = true)]
async fn ready_and_done_after_the_last_nudge_fence_do_not_append_failed() {
    let mut f = Fixture::new();
    let (lead, _) = f.lead().await;
    let lane = f.lane(lead, "late-ready").await;
    let (_owner, host) = owned(&f, &lane);
    f.runtime.monitor_lanes().unwrap();
    host.now.fetch_add(30 * MINUTE, Ordering::SeqCst);
    f.runtime.monitor_lanes().unwrap();
    let gate = Arc::new(tokio::sync::Barrier::new(2));
    let drive = BEFORE_QUEUE.scope(
        gate.clone(),
        f.runtime.drive_lane_deliveries(&lane.thread_id),
    );
    let finish = async {
        gate.wait().await; // the whole driver has passed its last nudge fence
        f.report(&lane, None, LaneStatus::Ready);
        f.finish(lane.root_run_id).await;
        gate.wait().await;
    };
    let (outcome, ()) = tokio::join!(drive, finish);
    assert_eq!(
        outcome.unwrap(),
        LaneDeliveryOutcome::Nacked(LaneDeliveryNack::Stopped)
    );
    let reports = f
        .storage
        .handle()
        .lane_reports(&lane.lane_id, 0, 20)
        .unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].status, LaneStatus::Ready);
    assert_eq!(f.receipts(), 0);
    assert!(episodes(&f)[0].resolved_at_ns.is_some());
    f.runtime.cancel(lead).unwrap();
    f.runtime.wait(lead).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn lag_and_a_parallel_tool_completion_remain_unobservable_until_a_full_boundary() {
    let mut f = Fixture::new();
    let (lead, _) = f.lead().await;
    let lane = f.lane(lead, "lag-parallel").await;
    let (_owner, host) = owned(&f, &lane);
    f.runtime.monitor_lanes().unwrap();
    f.model.block_next.store(true, Ordering::SeqCst);
    f.runtime
        .send_message(lane.root_run_id, "parallel work".into())
        .unwrap();
    f.model.blocked.notified().await;
    // Overflow the real receiver, losing both parallel starts while the
    // provider stays live until explicit cancel. Completion-only suffixes
    // cannot re-establish a complete execution observation.
    let bus = &f.runtime.shared.bus;
    for call_id in ["still-running", "completed"] {
        bus.emit(Event::new(ToolEvent::ToolStarted {
            tool_name: "shell".into(),
            call_id: call_id.into(),
            input: None,
            run_id: Some(lane.root_run_id.to_string()),
            parent_call_id: None,
        }));
    }
    for _ in 0..16_384 {
        let call_id = "completed";
        bus.emit(Event::new(ToolEvent::ToolCompleted {
            tool_name: "shell".into(),
            call_id: call_id.into(),
            is_error: false,
            output: None,
            detail: None,
            run_id: Some(lane.root_run_id.to_string()),
            parent_call_id: None,
        }));
    }
    host.now.fetch_add(120 * MINUTE, Ordering::SeqCst);
    f.runtime.monitor_lanes().unwrap();
    host.now.fetch_add(120 * MINUTE, Ordering::SeqCst);
    f.runtime.monitor_lanes().unwrap();
    assert!(
        episodes(&f).is_empty(),
        "one completion cannot account for the lost parallel start"
    );
    bus.emit(Event::new(LifecycleEvent::RunProgress {
        run_id: lane.root_run_id.to_string(),
        activity: event_bus::RunActivity::Idle,
        compaction_operation_id: None,
        context: None,
    }));
    f.runtime.monitor_lanes().unwrap();
    host.now.fetch_add(5 * MINUTE - 1, Ordering::SeqCst);
    f.runtime.monitor_lanes().unwrap();
    assert!(episodes(&f).is_empty());
    host.now.fetch_add(1, Ordering::SeqCst);
    f.runtime.monitor_lanes().unwrap();
    assert_eq!(episodes(&f)[0].classification, Class::WorkStalled);
    for run in [lane.root_run_id, lead] {
        f.runtime.cancel(run).unwrap();
        f.runtime.wait(run).await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn lag_with_a_child_completion_suffix_keeps_the_child_wait_healthy_until_a_full_boundary() {
    let mut f = Fixture::new();
    let (lead, _) = f.lead().await;
    let lane = f.lane(lead, "lag-child").await;
    let (_owner, host) = owned(&f, &lane);
    f.runtime.monitor_lanes().unwrap();
    f.model.block_next.store(true, Ordering::SeqCst);
    let child = f
        .runtime
        .delegate_background_as_child(
            lane.root_run_id,
            Role::Worker,
            "long provider child",
            RunConfig::default(),
        )
        .unwrap();
    f.model.blocked.notified().await;
    // Overflow the real receiver so the child's start/progress events are lost
    // while its provider call stays live. The surviving completion-only suffix
    // is not a whole-run observation and must not end the healthy child wait.
    let bus = &f.runtime.shared.bus;
    for _ in 0..16_384 {
        bus.emit(Event::new(ToolEvent::ToolCompleted {
            tool_name: "shell".into(),
            call_id: "completed".into(),
            is_error: false,
            output: None,
            detail: None,
            run_id: Some(child.to_string()),
            parent_call_id: None,
        }));
    }
    host.now.fetch_add(120 * MINUTE, Ordering::SeqCst);
    f.runtime.monitor_lanes().unwrap();
    host.now.fetch_add(120 * MINUTE, Ordering::SeqCst);
    f.runtime.monitor_lanes().unwrap();
    assert!(
        episodes(&f).is_empty(),
        "an unobserved child activity cannot prove the child wait ended"
    );
    // A whole-run activity boundary re-establishes observability.
    bus.emit(Event::new(LifecycleEvent::RunProgress {
        run_id: child.to_string(),
        activity: event_bus::RunActivity::Idle,
        compaction_operation_id: None,
        context: None,
    }));
    f.runtime.monitor_lanes().unwrap();
    host.now.fetch_add(5 * MINUTE, Ordering::SeqCst);
    f.runtime.monitor_lanes().unwrap();
    assert_eq!(episodes(&f)[0].classification, Class::WorkStalled);
    for run in [child, lane.root_run_id, lead] {
        f.runtime.cancel(run).unwrap();
        f.runtime.wait(run).await.unwrap();
    }
}
