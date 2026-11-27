use super::*;
use event_bus::{Event, EventBus, LifecycleEvent};
use providers::{ChatResponse, Message, ToolSpec};
use runtime::{AgentInvocationContext, AgentModel, Role, RuntimeError};
use std::sync::Barrier;
use workbench_protocol::namespaces::{LifecycleEvent as WireLifecycleEvent, ProjectionEvent};
use workbench_protocol::queries::ReadBody;

struct NoModel;
#[async_trait::async_trait]
impl AgentModel for NoModel {
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        std::future::pending().await
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "unused".into()
    }
}
fn make(
    handle: &tokio::runtime::Handle,
    capacity: usize,
    limits: SubscriptionLimits,
) -> (Arc<Projection>, Arc<EventBus>) {
    let bus = Arc::new(EventBus::new(capacity));
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus.clone())),
        Arc::new(NoModel),
    );
    (Projection::new(runtime, handle, limits), bus)
}
fn all(after: Option<EventCursor>) -> Subscribe {
    Subscribe {
        scope: SubscriptionScope::All,
        after,
    }
}
fn event(n: u32) -> UiEvent {
    UiEvent::Lifecycle(WireLifecycleEvent::CoreStatusChanged {
        status: workbench_protocol::handshake::CoreStatus {
            state: workbench_protocol::handshake::CoreState::Ready,
            retention_reasons: vec![],
            running: workbench_protocol::handshake::ActivityCounts {
                turns: n,
                ..Default::default()
            },
        },
    })
}
#[tokio::test]
async fn replay_in_range_repeats_exactly_the_suffix_and_ids_never_recur() {
    let (p, _) = make(&tokio::runtime::Handle::current(), 64, Default::default());
    let initial = p.subscribe_with_snapshot(&all(None)).unwrap();
    let after = initial.started.high_water;
    {
        let mut state = p.state.lock().unwrap();
        state.publish(event(1), p.limits);
        state.publish(event(2), p.limits);
    }
    let mut resumed = p.subscribe_with_snapshot(&all(Some(after))).unwrap();
    assert_eq!(resumed.started.high_water, after);
    assert!(resumed.started.initial_page.is_none());
    let first = resumed.recv().await.unwrap();
    let second = resumed.recv().await.unwrap();
    assert_eq!(first.event, event(1));
    assert_eq!(second.event, event(2));
    assert_eq!(first.cursor.sequence.get(), after.sequence.get() + 1);
    assert_eq!(second.cursor.sequence.get(), after.sequence.get() + 2);
    assert_ne!(
        initial.started.subscription_id,
        resumed.started.subscription_id
    );
    p.unsubscribe(&resumed.started.subscription_id);
    assert!(matches!(
        resumed.recv().await.unwrap().event,
        UiEvent::Projection(ProjectionEvent::SubscriptionClosed {
            reason: CloseReason::Unsubscribed
        })
    ));
    assert!(resumed.recv().await.is_none());
    let next = p.subscribe_with_snapshot(&all(None)).unwrap();
    assert_ne!(
        next.started.subscription_id,
        resumed.started.subscription_id
    );
}
#[tokio::test]
async fn replay_eviction_epoch_mismatch_and_future_cursor_require_resync() {
    let limits = SubscriptionLimits {
        replay_events: 1,
        ..Default::default()
    };
    let (p, _) = make(&tokio::runtime::Handle::current(), 64, limits);
    let initial = p.subscribe_with_snapshot(&all(None)).unwrap();
    let after = initial.started.high_water;
    {
        let mut s = p.state.lock().unwrap();
        s.publish(event(1), limits);
        s.publish(event(2), limits);
    }
    for (cursor, reason) in [
        (after, ResyncReason::CursorTooOld),
        (
            EventCursor {
                core_epoch: InstanceEpoch(after.core_epoch.get() + 1),
                ..after
            },
            ResyncReason::EpochChanged,
        ),
        (
            EventCursor {
                sequence: Sequence(999),
                ..after
            },
            ResyncReason::CursorTooOld,
        ),
    ] {
        let mut sub = p.subscribe_with_snapshot(&all(Some(cursor))).unwrap();
        assert!(sub.started.snapshot_id.is_some());
        let terminal = sub.recv().await.unwrap();
        assert!(terminal.cursor.sequence > sub.started.high_water.sequence);
        assert_eq!(
            terminal.event,
            UiEvent::Projection(ProjectionEvent::ResyncRequired { reason })
        );
        assert!(sub.recv().await.is_none());
    }
}
#[tokio::test]
async fn a_fixed_epoch_is_the_hello_epoch_and_the_cursor_epoch_and_rejects_the_previous_one() {
    let bus = Arc::new(EventBus::new(64));
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus.clone())),
        Arc::new(NoModel),
    );
    let predecessor = 0x1234_5678_9abc_u64;
    let successor = predecessor + 1;
    let p = Projection::with_epoch(
        runtime,
        &tokio::runtime::Handle::current(),
        Default::default(),
        Some(successor),
    );
    assert_eq!(p.instance_epoch(), InstanceEpoch(successor));
    let first = p.subscribe_with_snapshot(&all(None)).unwrap();
    assert_eq!(
        first.started.high_water.core_epoch,
        InstanceEpoch(successor)
    );
    // A cursor of the same epoch resumes; the predecessor's cursor, even at a
    // sequence the successor has, is refused with a fresh snapshot.
    let resumed = p
        .subscribe_with_snapshot(&all(Some(first.started.high_water)))
        .unwrap();
    assert!(resumed.started.snapshot_id.is_none());
    let mut refused = p
        .subscribe_with_snapshot(&all(Some(EventCursor {
            core_epoch: InstanceEpoch(predecessor),
            sequence: first.started.high_water.sequence,
        })))
        .unwrap();
    assert!(refused.started.snapshot_id.is_some());
    assert_eq!(
        refused.recv().await.unwrap().event,
        UiEvent::Projection(ProjectionEvent::ResyncRequired {
            reason: ResyncReason::EpochChanged
        })
    );
}
#[tokio::test]
async fn slow_consumer_receives_terminal_resync_for_both_count_and_byte_budgets() {
    for limits in [
        SubscriptionLimits {
            queue_events: 1,
            ..Default::default()
        },
        SubscriptionLimits {
            queue_bytes: 1,
            ..Default::default()
        },
    ] {
        let (p, _) = make(&tokio::runtime::Handle::current(), 64, limits);
        let mut sub = p.subscribe_with_snapshot(&all(None)).unwrap();
        {
            let mut s = p.state.lock().unwrap();
            s.publish(event(1), limits);
            s.publish(event(2), limits);
        }
        assert_eq!(
            sub.recv().await.unwrap().event,
            UiEvent::Projection(ProjectionEvent::ResyncRequired {
                reason: ResyncReason::SlowSubscriber
            })
        );
        assert!(sub.recv().await.is_none());
    }
}
#[tokio::test]
async fn lag_on_the_host_bus_invalidates_the_epoch_instead_of_silently_continuing() {
    let (p, bus) = make(&tokio::runtime::Handle::current(), 1, Default::default());
    let mut sub = p.subscribe_with_snapshot(&all(None)).unwrap();
    for _ in 0..4 {
        bus.emit(Event::new(LifecycleEvent::BackgroundTaskCompleted {
            task_id: "task".into(),
        }));
    }
    // The current-thread executor cannot run the projection until this await;
    // the broadcast ring has deterministically overflowed already.
    let terminal = sub.recv().await.unwrap();
    assert_eq!(
        terminal.event,
        UiEvent::Projection(ProjectionEvent::ResyncRequired {
            reason: ResyncReason::EpochChanged
        })
    );
    assert_ne!(
        terminal.cursor.core_epoch,
        sub.started.high_water.core_epoch
    );
}
fn wakeup(n: usize) -> Event {
    Event::new(LifecycleEvent::BackgroundTaskCompleted {
        task_id: n.to_string(),
    })
}

#[tokio::test]
async fn refresh_wakeups_coalesce_a_ready_burst_but_leave_a_bounded_suffix() {
    let bus = EventBus::new(MAX_REFRESH_WAKEUPS * 2);
    let mut events = bus.subscribe();
    for n in 0..=MAX_REFRESH_WAKEUPS {
        bus.emit(wakeup(n));
    }
    let first = events.recv().await;
    let batch = RefreshWakeups::drain(&mut events, first).await;
    assert_eq!(
        batch,
        RefreshWakeups {
            dirty: true,
            ..Default::default()
        }
    );
    // Do not consume the next batch's first event, or wait to fill a partial batch.
    let last = events.recv().await.unwrap();
    assert_eq!(last.kind, wakeup(MAX_REFRESH_WAKEUPS).kind);
    assert_eq!(RefreshWakeups::drain(&mut events, Ok(last)).await, batch);
}

#[tokio::test(start_paused = true)]
async fn refresh_wakeups_coalesce_spaced_bursts_and_keep_the_trailing_dirty() {
    let (p, bus) = make(&tokio::runtime::Handle::current(), 128, Default::default());
    let mut completed = p.model_refreshes.subscribe();
    bus.emit(wakeup(0));
    completed.changed().await.unwrap();
    assert_eq!(*completed.borrow_and_update(), 1);
    // Six separately scheduled bursts used to cause six additional collects.
    for burst in 1..=6 {
        tokio::time::advance(std::time::Duration::from_millis(50)).await;
        for n in 0..8 {
            bus.emit(wakeup(burst * 8 + n));
        }
        tokio::task::yield_now().await;
        assert_eq!(*completed.borrow(), 1);
    }
    tokio::time::advance(std::time::Duration::from_millis(199)).await;
    bus.emit(wakeup(99)); // The last dirty, just before the minimum interval.
    tokio::task::yield_now().await;
    assert_eq!(*completed.borrow(), 1);
    tokio::time::advance(std::time::Duration::from_millis(1)).await;
    completed.changed().await.unwrap();
    assert_eq!(*completed.borrow_and_update(), 2);
    tokio::time::advance(MIN_REFRESH_INTERVAL * 2).await;
    tokio::task::yield_now().await;
    assert_eq!(*completed.borrow(), 2); // No periodic collect while clean.
}

#[tokio::test(start_paused = true)]
async fn refresh_interval_starts_at_completion_not_at_collect_start() {
    let (p, bus) = make(&tokio::runtime::Handle::current(), 128, Default::default());
    let mut completed = p.model_refreshes.subscribe();
    let (entered_tx, entered) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let entered_tx = Mutex::new(Some(entered_tx));
    let released = Mutex::new(released);
    p.state.lock().unwrap().refresh_hook = Some(Arc::new(move || {
        entered_tx.lock().unwrap().take().unwrap().send(()).unwrap();
        released.lock().unwrap().recv().unwrap();
    }));
    bus.emit(wakeup(0));
    entered.await.unwrap();
    tokio::time::advance(MIN_REFRESH_INTERVAL * 10).await;
    bus.emit(wakeup(1)); // A change during collect must not disappear.
    release.send(()).unwrap();
    completed.changed().await.unwrap();
    assert_eq!(*completed.borrow_and_update(), 1);
    tokio::time::advance(MIN_REFRESH_INTERVAL - std::time::Duration::from_millis(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(*completed.borrow(), 1);
    tokio::time::advance(std::time::Duration::from_millis(1)).await;
    completed.changed().await.unwrap();
    assert_eq!(*completed.borrow_and_update(), 2);
}

#[tokio::test(start_paused = true)]
async fn refresh_cooldown_keeps_draining_fragments_without_lag_or_extra_collects() {
    let (p, bus) = make(&tokio::runtime::Handle::current(), 8, Default::default());
    let epoch = p.instance_epoch();
    let mut completed = p.model_refreshes.subscribe();
    let mut drained = p.model_event_batches.subscribe();
    bus.emit(wakeup(0));
    completed.changed().await.unwrap();
    assert_eq!(*completed.borrow_and_update(), 1);
    drained.borrow_and_update();
    let cooldown_started = tokio::time::Instant::now();
    // Pace batches on receiver progress, not a clock or scheduler guess. The
    // total exceeds the bus capacity while refresh remains in its cooldown.
    for _ in 0..16 {
        for _ in 0..8 {
            bus.emit(Event::new(event_bus::MessageEvent::ReasoningDelta {
                run_id: None,
                delta: "fragment".into(),
            }));
        }
        drained.changed().await.unwrap();
        drained.borrow_and_update();
        assert_eq!(tokio::time::Instant::now(), cooldown_started);
        assert_eq!(p.instance_epoch(), epoch);
        assert_eq!(*completed.borrow(), 1);
    }
    bus.emit(wakeup(1));
    drained.changed().await.unwrap();
    tokio::time::advance(MIN_REFRESH_INTERVAL).await;
    completed.changed().await.unwrap();
    assert_eq!(*completed.borrow_and_update(), 2);
    assert_eq!(p.instance_epoch(), epoch);
}

#[tokio::test]
async fn fragments_do_not_hide_a_state_change_or_lag_in_the_same_refresh_batch() {
    use event_bus::MessageEvent;
    let bus = EventBus::new(8);
    let mut events = bus.subscribe();
    let fragments = [
        MessageEvent::MessageDelta {
            run_id: None,
            delta: "draft".into(),
        },
        MessageEvent::ReasoningDelta {
            run_id: None,
            delta: "draft".into(),
        },
        MessageEvent::ToolCallDelta {
            run_id: None,
            index: 0,
            call_id: None,
            tool_name: None,
            arguments_delta: "draft".into(),
        },
    ];
    for fragment in &fragments {
        bus.emit(Event::new(fragment.clone()));
    }
    let first = events.recv().await;
    assert_eq!(
        RefreshWakeups::drain(&mut events, first).await,
        RefreshWakeups::default()
    );
    for fragment in fragments {
        bus.emit(Event::new(fragment));
    }
    bus.emit(wakeup(0));
    let first = events.recv().await;
    assert_eq!(
        RefreshWakeups::drain(&mut events, first).await,
        RefreshWakeups {
            dirty: true,
            ..Default::default()
        }
    );

    // Loss discovered while draining (not just the initial recv) must invalidate cursors.
    let first = Ok(wakeup(1));
    for n in 0..16 {
        bus.emit(wakeup(n));
    }
    assert_eq!(
        RefreshWakeups::drain(&mut events, first).await,
        RefreshWakeups {
            dirty: true,
            lagged: true,
            closed: false,
            ..Default::default()
        }
    );
}

fn stored() -> (
    tempfile::TempDir,
    storage::Storage,
    Arc<EventBus>,
    AgentRuntime,
) {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("host.db"),
        ..Default::default()
    };
    let store = storage::Storage::open(config.clone()).unwrap();
    let bus = Arc::new(EventBus::new(128));
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus.clone())),
        Arc::new(NoModel),
    )
    .with_run_store(runtime::RunStore::open(&config, store.handle()).unwrap());
    (dir, store, bus, runtime)
}

/// Diagnostic only: wall time is reported, never a correctness assertion.
/// 250 threads / 5,000 total runs mirrors the profiling report, without opening
/// the operator's database. Run with --ignored --nocapture in a release build.
#[test]
#[ignore = "synthetic collect measurement"]
fn measure_collect_synthetic() {
    let (dir, _store, _bus, runtime) = stored();
    let mut conn = rusqlite::Connection::open(dir.path().join("host.db")).unwrap();
    let tx = conn.transaction().unwrap();
    for thread in 0..250 {
        let id = format!("thread-{thread}");
        tx.execute(
            "INSERT INTO threads(thread_id,project_id,project_root,title,created_at_ns,updated_at_ns,source)
             VALUES(?1,'project',?2,?1,0,0,'host')",
            rusqlite::params![id, dir.path().to_string_lossy()],
        ).unwrap();
        for run in 0..20 {
            let run_id = format!("run-{thread}-{run}");
            tx.execute(
                "INSERT INTO run_contexts(run_id,role,name,config_json,checkpoints_json,terminal_phase,restorable,updated_at_ns)
                 VALUES(?1,'worker',?1,'{}','[]','done',0,0)",
                [&run_id],
            ).unwrap();
            tx.execute(
                "INSERT INTO thread_runs(run_id,thread_id,root_run_id,depth) VALUES(?1,?2,?1,0)",
                rusqlite::params![run_id, id],
            )
            .unwrap();
        }
    }
    tx.commit().unwrap();
    let projects = [(ProjectId::new("project").unwrap(), dir.path().to_path_buf())];
    let reader = runtime.open_storage_reader().unwrap().unwrap();
    for sample in 0..6 {
        let start = std::time::Instant::now();
        let models =
            queries::collect(&runtime, Some(&reader), &projects, &Default::default()).unwrap();
        let elapsed = start.elapsed();
        assert_eq!(models.threads.len(), 250);
        assert_eq!(models.runs.len(), 5000);
        println!(
            "collect sample={sample} elapsed_ms={:.3}",
            elapsed.as_secs_f64() * 1000.0
        );
    }
}

#[test]
fn a_change_during_refresh_reaches_the_subscriber_without_an_extra_wakeup() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (dir, store, bus, runtime) = stored();
    let project = ProjectId::new("project").unwrap();
    let title = |value: &str| {
        store
            .handle()
            .upsert_threads_host(&[storage::threads::ThreadUpsert {
                thread_id: "thread".into(),
                project_id: Some(project.as_str().into()),
                project_root: Some(dir.path().to_string_lossy().into_owned()),
                title: Some(value.into()),
                ..Default::default()
            }])
            .unwrap();
    };
    title("before");
    let p = Projection::new(runtime, rt.handle(), Default::default());
    p.configure_projects(
        vec![(project.clone(), dir.path().to_path_buf())],
        Default::default(),
    )
    .unwrap();
    let mut sub = p.subscribe_with_snapshot(&all(None)).unwrap();
    assert!(
        sub.started
            .initial_page
            .as_ref()
            .unwrap()
            .items
            .iter()
            .any(|item| matches!(item, SnapshotItem::Thread(thread) if thread.title == "before"))
    );

    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    p.state.lock().unwrap().refresh_hook = Some({
        let entered = entered.clone();
        let release = release.clone();
        Arc::new(move || {
            entered.wait();
            release.wait();
        })
    });
    bus.emit(wakeup(0));
    entered.wait(); // The in-flight refresh has already read the "before" state.
    title("after");
    bus.emit(wakeup(1)); // The change's own notification; no subsequent tick or refresh.
    release.wait();

    let changed = rt.block_on(async {
        loop {
            let event = sub.recv().await.expect("open subscription").event;
            match event {
                UiEvent::Workspace(WorkspaceEvent::ThreadChanged { thread })
                    if thread.title == "after" =>
                {
                    break thread;
                }
                UiEvent::Projection(ProjectionEvent::ResyncRequired { reason }) => {
                    panic!("unexpected resync: {reason:?}")
                }
                _ => {}
            }
        }
    });
    assert_eq!(changed.id.as_str(), "thread");
}

#[test]
fn hello_epoch_and_status_do_not_wait_for_a_refresh() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (p, _) = make(rt.handle(), 64, Default::default());
    p.refresh().unwrap();
    let status = p.cached_status();
    let mut epochs = p.epoch_changes();
    p.invalidate();
    rt.block_on(epochs.changed()).unwrap();
    let epoch = InstanceEpoch(*epochs.borrow_and_update());
    assert_eq!(p.instance_epoch(), epoch);

    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    p.state.lock().unwrap().refresh_hook = Some({
        let entered = entered.clone();
        let release = release.clone();
        Arc::new(move || {
            entered.wait();
            release.wait();
        })
    });
    let refreshing = {
        let p = p.clone();
        std::thread::spawn(move || p.refresh())
    };
    entered.wait();
    assert_eq!(p.instance_epoch(), epoch);
    assert_eq!(p.cached_status(), status);
    release.wait();
    refreshing.join().unwrap().unwrap();
}

#[test]
fn collect_has_its_own_reader_and_leaves_history_and_snapshot_cuts_available() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (dir, store, _bus, runtime) = stored();
    let project = ProjectId::new("project").unwrap();
    let projects = vec![(project.clone(), dir.path().to_path_buf())];
    store
        .handle()
        .upsert_threads_host(&[storage::threads::ThreadUpsert {
            thread_id: "thread".into(),
            project_id: Some(project.as_str().into()),
            project_root: Some(dir.path().to_string_lossy().into_owned()),
            title: Some("before".into()),
            ..Default::default()
        }])
        .unwrap();
    let p = Projection::new(runtime.clone(), rt.handle(), Default::default());
    p.configure_projects(projects.clone(), Default::default())
        .unwrap();
    let initial = p.subscribe_with_snapshot(&all(None)).unwrap();
    let snapshot_id = initial.started.snapshot_id.clone().unwrap();
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    p.state.lock().unwrap().refresh_hook = Some({
        let (entered, release) = (entered.clone(), release.clone());
        Arc::new(move || {
            entered.wait();
            release.wait();
        })
    });
    // Hold the shared runtime reader until collect has completed on its own
    // connection. This would deadlock if collect fell back to that reader.
    let refreshing = runtime
        .with_storage_reader(|_| {
            let p = p.clone();
            let refreshing = std::thread::spawn(move || p.refresh_models());
            entered.wait();
            Ok(refreshing)
        })
        .unwrap()
        .unwrap();
    assert!(matches!(
        p.model_refresh.try_lock(),
        Err(std::sync::TryLockError::WouldBlock)
    ));
    assert!(p.state.try_lock().is_ok());
    let history = p
        .query(&Query::Conversation(
            ConversationQuery::ReadCommittedHistory(
                workbench_protocol::queries::ReadCommittedHistory {
                    thread: ThreadId::new("thread").unwrap(),
                    run: None,
                    direction: Default::default(),
                    page: Default::default(),
                },
            ),
        ))
        .unwrap();
    assert!(
        matches!(history, QueryReply::Conversation(ConversationReply::CommittedHistory(page)) if page.items.is_empty())
    );
    let mut sub = p.subscribe_with_snapshot(&all(None)).unwrap();
    let page = p
        .read_snapshot(&ReadSnapshot {
            snapshot_id,
            page: Default::default(),
        })
        .unwrap();
    assert_eq!(page.high_water, initial.started.high_water);
    // The dedicated connection must not retain a read transaction between
    // queries (or during diff/publish), even after a concurrent write.
    store
        .handle()
        .upsert_threads_host(&[storage::threads::ThreadUpsert {
            thread_id: "thread".into(),
            title: Some("after".into()),
            ..Default::default()
        }])
        .unwrap();
    let conn = rusqlite::Connection::open(dir.path().join("host.db")).unwrap();
    let (busy, log, checkpointed): (i64, i64, i64) = conn
        .query_row("PRAGMA wal_checkpoint(FULL)", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap();
    assert_eq!(busy, 0);
    assert_eq!(log, checkpointed);
    let reconfigured = {
        let p = p.clone();
        std::thread::spawn(move || {
            p.configure_projects(
                projects,
                BTreeMap::from([("project".into(), "renamed".into())]),
            )
        })
    };
    release.wait();
    refreshing.join().unwrap().unwrap();
    reconfigured.join().unwrap().unwrap();
    let changed = rt.block_on(sub.recv()).unwrap();
    assert!(changed.cursor.sequence > page.high_water.sequence);
    assert!(
        matches!(changed.event, UiEvent::Workspace(WorkspaceEvent::ThreadChanged { thread }) if thread.title == "after")
    );
    let state = p.state.lock().unwrap();
    assert_eq!(state.models.threads["thread"].summary.title, "after");
    assert_eq!(state.models.projects["project"].display_name, "renamed");
}

#[test]
fn hello_and_lane_reads_do_not_wait_for_a_bodies_advance() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (dir, store, _bus, runtime) = stored();
    let project = ProjectId::new("project").unwrap();
    store
        .handle()
        .upsert_threads_host(&[storage::threads::ThreadUpsert {
            thread_id: "thread".into(),
            project_id: Some(project.as_str().into()),
            project_root: Some(dir.path().to_string_lossy().into_owned()),
            title: Some("title".into()),
            ..Default::default()
        }])
        .unwrap();
    let p = Projection::new(runtime.clone(), rt.handle(), Default::default());
    p.configure_projects(
        vec![(project.clone(), dir.path().to_path_buf())],
        Default::default(),
    )
    .unwrap();
    let status = p.cached_status();
    let epoch = p.instance_epoch();

    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    p.body_cursor.lock().unwrap().advance_hook = Some({
        let entered = entered.clone();
        let release = release.clone();
        Arc::new(move || {
            entered.wait();
            release.wait();
        })
    });
    let advancing = {
        let p = p.clone();
        std::thread::spawn(move || p.refresh())
    };
    entered.wait();
    assert_eq!(p.instance_epoch(), epoch);
    assert_eq!(p.cached_status(), status);
    let QueryReply::Workspace(WorkspaceReply::Threads(threads)) = p
        .query(&Query::Workspace(WorkspaceQuery::ListThreads(
            workbench_protocol::queries::ListThreads {
                project,
                page: PageRequest::default(),
            },
        )))
        .unwrap()
    else {
        panic!("expected threads")
    };
    assert_eq!(threads.items.len(), 1);
    assert!(
        runtime
            .with_storage_reader(|db| db.thread("thread"))
            .unwrap()
            .flatten()
            .is_some()
    );
    release.wait();
    advancing.join().unwrap().unwrap();
}

#[test]
fn update_during_snapshot_cannot_cross_registration_or_high_water() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (p, _) = make(rt.handle(), 64, Default::default());
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    p.state.lock().unwrap().snapshot_hook = Some({
        let entered = entered.clone();
        let release = release.clone();
        Arc::new(move || {
            entered.wait();
            release.wait();
        })
    });
    let snap = {
        let p = p.clone();
        std::thread::spawn(move || p.subscribe_with_snapshot(&all(None)).unwrap())
    };
    entered.wait();
    let updating = Arc::new(Barrier::new(2));
    let update = {
        let p = p.clone();
        let updating = updating.clone();
        std::thread::spawn(move || {
            updating.wait();
            p.state.lock().unwrap().publish(event(1), p.limits);
        })
    };
    updating.wait();
    release.wait();
    let mut sub = snap.join().unwrap();
    update.join().unwrap();
    let change = rt.block_on(sub.recv()).unwrap();
    assert_eq!(change.event, event(1));
    assert_eq!(
        change.cursor.sequence.get(),
        sub.started.high_water.sequence.get() + 1
    );
    assert!(
        sub.started.initial_page.as_ref().unwrap().items.iter().any(
            |item| matches!(item, SnapshotItem::CoreStatus(status) if status.running.turns == 0)
        )
    );
}

#[tokio::test]
async fn replay_byte_eviction_and_shutdown_have_observable_terminal_events() {
    let (p, _) = make(
        &tokio::runtime::Handle::current(),
        64,
        SubscriptionLimits {
            replay_bytes: 1,
            ..Default::default()
        },
    );
    let mut active = p.subscribe_with_snapshot(&all(None)).unwrap();
    let after = active.started.high_water;
    p.state.lock().unwrap().publish(event(1), p.limits);
    assert_eq!(active.recv().await.unwrap().event, event(1));
    let mut stale = p.subscribe_with_snapshot(&all(Some(after))).unwrap();
    assert!(matches!(
        stale.recv().await.unwrap().event,
        UiEvent::Projection(ProjectionEvent::ResyncRequired {
            reason: ResyncReason::CursorTooOld
        })
    ));
    drop(p);
    let closed = active.recv().await.unwrap();
    assert!(closed.cursor.sequence > after.sequence);
    assert_eq!(
        closed.event,
        UiEvent::Projection(ProjectionEvent::SubscriptionClosed {
            reason: CloseReason::CoreShuttingDown
        })
    );
    assert!(active.recv().await.is_none());
}

#[test]
fn page_budget_counts_json_escaping_as_well_as_items() {
    let pager = Pager::default();
    let values = vec!["\\".repeat(100_000); 10];
    let mut page = PageRequest {
        limit: 200,
        cursor: None,
    };
    let mut seen = 0;
    loop {
        let result = pager.page("escaped", &values, &page, Revision(1)).unwrap();
        assert!(!result.items.is_empty());
        assert!(
            serde_json::to_vec(&result).unwrap().len()
                < workbench_protocol::limits::DEFAULT_FRAME_BYTES as usize
        );
        seen += result.items.len();
        page.cursor = result.next_cursor;
        if page.cursor.is_none() {
            break;
        }
    }
    assert_eq!(seen, values.len());
    let first = pager
        .page(
            "shrunk",
            &[1, 2, 3],
            &PageRequest {
                limit: 2,
                cursor: None,
            },
            Revision(1),
        )
        .unwrap();
    assert_eq!(
        pager
            .page(
                "shrunk",
                &[1],
                &PageRequest {
                    limit: 2,
                    cursor: first.next_cursor
                },
                Revision(1)
            )
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
}

#[test]
fn control_rich_question_preview_fits_the_wire_budget_without_breaking_utf8() {
    let preview = queries::text(&"\0".repeat(256 * 1024), 256 * 1024);
    assert!(!preview.is_empty());
    assert!(serde_json::to_vec(&preview).unwrap().len() <= 512 * 1024);
    assert_eq!(queries::text("日本語\nformat?", 1024), "日本語\nformat?");
}

#[tokio::test(start_paused = true)]
async fn snapshot_age_and_epoch_expiry_require_resync_without_wall_clock_waits() {
    let (p, _) = make(&tokio::runtime::Handle::current(), 64, Default::default());
    let sub = p.subscribe_with_snapshot(&all(None)).unwrap();
    let q = ReadSnapshot {
        snapshot_id: sub.started.snapshot_id.unwrap(),
        page: PageRequest::default(),
    };
    tokio::time::advance(SNAPSHOT_TTL - std::time::Duration::from_nanos(1)).await;
    assert_eq!(
        p.read_snapshot(&q).unwrap().high_water,
        sub.started.high_water
    );
    tokio::time::advance(std::time::Duration::from_nanos(1)).await;
    assert_eq!(
        p.read_snapshot(&q).unwrap_err().code,
        ErrorCode::ResyncRequired
    );
    let next = p.subscribe_with_snapshot(&all(None)).unwrap();
    let q = ReadSnapshot {
        snapshot_id: next.started.snapshot_id.unwrap(),
        page: PageRequest::default(),
    };
    p.state.lock().unwrap().invalidate(p.limits);
    assert_eq!(
        p.read_snapshot(&q).unwrap_err().code,
        ErrorCode::ResyncRequired
    );
}

#[test]
fn backward_pages_respect_encoded_byte_and_item_limits_without_gaps() {
    let pager = Pager::default();
    let values: Vec<_> = (0..201)
        .map(|i| format!("{i}:{}", "\0".repeat(8192)))
        .collect();
    let mut request = PageRequest {
        limit: 200,
        cursor: None,
    };
    let mut pages = vec![];
    loop {
        let page = pager
            .page_backward("backward-budget", &values, &request, Revision(7))
            .unwrap();
        assert!(!page.items.is_empty());
        assert!(page.items.len() <= 200);
        assert!(serde_json::to_vec(&page).unwrap().len() <= queries::PAGE_BYTES);
        request.cursor = page.next_cursor;
        pages.push(page.items);
        if request.cursor.is_none() {
            break;
        }
    }
    let all: Vec<_> = pages.into_iter().rev().flatten().collect();
    assert_eq!(all, values);
    let empty = pager
        .page_backward::<String>("empty", &[], &PageRequest::default(), Revision(1))
        .unwrap();
    assert!(empty.items.is_empty());
    assert!(empty.next_cursor.is_none());
}

#[tokio::test]
async fn an_area_source_publishes_only_its_own_namespace_through_the_one_boundary() {
    let (p, _) = make(&tokio::runtime::Handle::current(), 64, Default::default());
    let mut subscriber = p.subscribe_with_snapshot(&all(None)).unwrap();
    let after = subscriber.started.high_water;

    // The projection's own control channel and names that are no namespace
    // have no source.
    for refused in ["projection", "no_such_namespace"] {
        assert_eq!(
            p.source(refused).err().map(|e| e.code),
            Some(ErrorCode::InvalidRequest),
            "{refused}"
        );
    }

    // An event of another namespace is refused and never published.
    let workspace = p.source("workspace").unwrap();
    assert_eq!(
        workspace.publish(event(1)).unwrap_err().code,
        ErrorCode::InvalidRequest
    );

    // An event of its own namespace gets the next cursor and reaches the
    // subscriber like any other event.
    let lifecycle = p.source("lifecycle").unwrap();
    lifecycle.publish(event(2)).unwrap();
    let delivered = subscriber.recv().await.unwrap();
    assert_eq!(delivered.event, event(2));
    assert_eq!(delivered.cursor.sequence.get(), after.sequence.get() + 1);

    // A source keeps nothing alive: once the projection is gone it reports so.
    drop(subscriber);
    drop(p);
    assert_eq!(
        lifecycle.publish(event(3)).unwrap_err().code,
        ErrorCode::Draining
    );
}

// Activity fixtures stamp both event clocks and inject a manual monotonic "now".
fn activity_fixture() -> (
    tokio::runtime::Runtime,
    tempfile::TempDir,
    storage::Storage,
    Arc<Projection>,
    Arc<std::sync::atomic::AtomicU64>,
    Arc<EventBus>,
) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (dir, store, bus, runtime) = stored();
    for (thread, run) in [("thread", "run"), ("other-thread", "other-run")] {
        store
            .handle()
            .upsert_threads_host(&[storage::threads::ThreadUpsert {
                thread_id: thread.into(),
                project_id: Some("project".into()),
                project_root: Some(dir.path().to_string_lossy().into_owned()),
                root_run_id: Some(run.into()),
                ..Default::default()
            }])
            .unwrap();
        store
            .handle()
            .upsert_run_context(&storage::RunContextRecord {
                run_id: run.into(),
                role: "worker".into(),
                name: "activity".into(),
                parent_run_id: None,
                config_json: "{}".into(),
                messages_json: "[]".into(),
                checkpoints_json: "[]".into(),
                terminal_phase: "running".into(),
                restorable: false,
                updated_at_ns: 1,
                host_inputs_json: "[]".into(),
            })
            .unwrap();
    }
    let now = Arc::new(std::sync::atomic::AtomicU64::new(2000));
    let clock = now.clone();
    let p = Projection::with_activity_clock(
        runtime,
        rt.handle(),
        Default::default(),
        None,
        Arc::new(move || std::time::Duration::from_millis(clock.load(Ordering::Relaxed))),
    );
    p.configure_projects(
        vec![(ProjectId::new("project").unwrap(), dir.path().to_path_buf())],
        Default::default(),
    )
    .unwrap();
    (rt, dir, store, p, now, bus)
}
fn activity_start(call: &str, name: &str, input: serde_json::Value) -> Event {
    activity_at(
        event_bus::ToolEvent::ToolStarted {
            run_id: Some("run".into()),
            call_id: call.into(),
            parent_call_id: Some("parent".into()),
            tool_name: name.into(),
            input: Some(input),
        },
        1000,
    )
}
fn activity_at(kind: impl Into<EventKind>, millis: u64) -> Event {
    let mut event = Event::new(kind);
    event.meta.monotonic = std::time::Duration::from_millis(millis);
    event.meta.wall_clock = std::time::UNIX_EPOCH + std::time::Duration::from_millis(millis);
    event
}
fn activity_done(call: &str, millis: u64, failed: bool) -> Event {
    activity_at(
        event_bus::ToolEvent::ToolCompleted {
            run_id: Some("run".into()),
            call_id: call.into(),
            parent_call_id: Some("parent".into()),
            tool_name: "shell".into(),
            output: None,
            detail: None,
            is_error: failed,
        },
        millis,
    )
}
fn activity_snapshot(
    p: &Projection,
) -> Vec<workbench_protocol::namespaces::execution::ToolActivityView> {
    let sub = p.subscribe_with_snapshot(&all(None)).unwrap();
    sub.started
        .initial_page
        .unwrap()
        .items
        .into_iter()
        .filter_map(|v| match v {
            SnapshotItem::ToolActivity(tool) => Some(tool),
            _ => None,
        })
        .collect()
}
#[test]
fn activity_bus_order_scopes_reconnect_and_duration_use_the_event_clocks() {
    use workbench_protocol::namespaces::execution::ToolActivityStatus;
    let (rt, _dir, _store, p, now, bus) = activity_fixture();
    let mut sub = p
        .subscribe_with_snapshot(&Subscribe {
            scope: SubscriptionScope::Thread {
                thread: ThreadId::new("thread").unwrap(),
            },
            after: None,
        })
        .unwrap();
    let mut other = p
        .subscribe_with_snapshot(&Subscribe {
            scope: SubscriptionScope::Thread {
                thread: ThreadId::new("other-thread").unwrap(),
            },
            after: None,
        })
        .unwrap();
    bus.emit(activity_start(
        "shell",
        "shell",
        serde_json::json!({"command":"true"}),
    ));
    let first = rt.block_on(sub.recv()).unwrap();
    let UiEvent::Execution(ExecutionEvent::ToolStarted { tool }) = first.event else {
        panic!("start")
    };
    assert_eq!(tool.elapsed_ms, 1000);
    now.store(3500, Ordering::Relaxed);
    assert_eq!(activity_snapshot(&p)[0].elapsed_ms, 2500);
    assert_eq!(activity_snapshot(&p)[0].elapsed_ms, 2500);
    assert_eq!(p.activity_formats.load(Ordering::Relaxed), 1);
    bus.emit(activity_done("shell", 2240, true));
    let end = rt.block_on(sub.recv()).unwrap();
    assert!(matches!(
        end.event,
        UiEvent::Execution(ExecutionEvent::ToolFinished {
            status: ToolActivityStatus::Failed,
            duration_ms: Some(1240),
            ..
        })
    ));
    assert!(end.cursor.sequence > first.cursor.sequence);
    assert!(activity_snapshot(&p).is_empty());
    // An explicit close is the first event in the other thread's queue.
    p.unsubscribe(&other.started.subscription_id);
    assert!(matches!(
        rt.block_on(other.recv()).unwrap().event,
        UiEvent::Projection(ProjectionEvent::SubscriptionClosed { .. })
    ));
    p.observe_activity(activity_done("missing", 900, false));
    assert!(matches!(
        rt.block_on(sub.recv()).unwrap().event,
        UiEvent::Execution(ExecutionEvent::ToolFinished {
            duration_ms: None,
            ..
        })
    ));
    p.observe_activity(activity_start("backwards", "wait", serde_json::json!({})));
    let _ = rt.block_on(sub.recv());
    p.observe_activity(activity_done("backwards", 999, false));
    assert!(matches!(
        rt.block_on(sub.recv()).unwrap().event,
        UiEvent::Execution(ExecutionEvent::ToolFinished {
            duration_ms: None,
            status: ToolActivityStatus::Succeeded,
            ..
        })
    ));
}
#[test]
fn activity_masks_before_formatting_and_reuses_e2_inputs_and_bodies() {
    let (_rt, dir, store, p, _now, _bus) = activity_fixture();
    // Synthetic test value; no credential is loaded from the environment.
    let secret = format!("sk-{}", "a".repeat(32));
    for (call, name, input) in [
        (
            "shell",
            "shell",
            serde_json::json!({"command":format!("echo {secret}"), "cwd":"/work"}),
        ),
        (
            "js",
            "run_js",
            serde_json::json!({"code":format!("const token = '{secret}';\nconsole.log(token);"), "ignored":true}),
        ),
        (
            "large",
            "run_js",
            serde_json::json!({"code":format!("// {secret}\n{}", "x".repeat(20 * 1024))}),
        ),
    ] {
        let start = activity_start(call, name, input);
        store.handle().append_event(None, &start).unwrap();
        p.observe_activity(start);
        let db = storage::Database::open(&storage::StorageConfig {
            db_path: dir.path().join("host.db"),
            ..Default::default()
        })
        .unwrap();
        let input = db.tool_start_input(db.last_event_id().unwrap()).unwrap();
        let expected = p
            .bodies
            .tool(
                &ThreadId::new("thread").unwrap(),
                &RunId::new("run").unwrap(),
                storage::history::ToolRow {
                    tool_name: name.into(),
                    call_id: call.into(),
                    parent_call_id: Some("parent".into()),
                    is_error: false,
                    input,
                    detail: None,
                    duration_ms: None,
                },
            )
            .unwrap();
        let tools = activity_snapshot(&p);
        let actual = tools.iter().find(|t| t.call_id == call).unwrap();
        if call != "large" {
            assert_eq!(actual.input_summary, expected.input_summary);
        }
        let read = |input: &Option<MessageContent>| {
            input.as_ref().map(|input| match input {
                MessageContent::Inline { text } => text.clone(),
                MessageContent::Body(body) => {
                    let mut text = String::new();
                    let mut offset = ByteOffset(0);
                    loop {
                        let chunk = p
                            .bodies
                            .read(
                                &p.runtime,
                                &ReadBody {
                                    body_id: body.body_id.clone(),
                                    offset,
                                    max_bytes: 65536,
                                },
                            )
                            .unwrap();
                        text.push_str(&chunk.text);
                        match chunk.next_offset {
                            Some(next) => offset = next,
                            None => break,
                        }
                    }
                    text
                }
            })
        };
        let shown = read(&actual.input).unwrap();
        assert_eq!(Some(shown.clone()), read(&expected.input));
        assert!(!shown.contains(&secret));
        assert!(shown.contains("[REDACTED:openai-style-key]"));
        if name == "run_js" {
            assert!(!shown.contains("ignored"));
        }
        assert!(!serde_json::to_string(actual).unwrap().contains(&secret));
    }
    assert_eq!(p.activity_formats.load(Ordering::Relaxed), 3);
    assert!(p.retained_bodies() >= 1);
}
#[test]
fn activity_capacity_terminal_and_lag_snapshots_are_bounded_and_empty_after_loss() {
    use workbench_protocol::namespaces::execution::ToolActivityStatus;
    let (rt, _dir, _store, p, _now, bus) = activity_fixture();
    let mut sub = p.subscribe_with_snapshot(&all(None)).unwrap();
    for n in 0..activity::MAX_LIVE_TOOLS + 10 {
        p.observe_activity(activity_start(
            &format!("call-{n}"),
            "run_js",
            serde_json::json!({"code":"x".repeat(16 * 1024)}),
        ));
    }
    assert_eq!(activity_snapshot(&p).len(), activity::MAX_LIVE_TOOLS);
    assert_eq!(
        p.activity_formats.load(Ordering::Relaxed),
        activity::MAX_LIVE_TOOLS
    );
    for _ in 0..activity::MAX_LIVE_TOOLS {
        let _ = rt.block_on(sub.recv()).unwrap();
    }
    p.observe_activity(activity_at(
        LifecycleEvent::AgentRunStateChanged {
            run_id: "run".into(),
            from: event_bus::AgentRunPhase::Running,
            to: event_bus::AgentRunPhase::Error,
            reason: None,
            waiting: None,
        },
        3000,
    ));
    for _ in 0..activity::MAX_LIVE_TOOLS {
        assert!(matches!(
            rt.block_on(sub.recv()).unwrap().event,
            UiEvent::Execution(ExecutionEvent::ToolFinished {
                status: ToolActivityStatus::Interrupted,
                ..
            })
        ));
    }
    assert!(activity_snapshot(&p).is_empty());
    p.observe_activity(activity_start("lost", "wait", serde_json::json!({})));
    let mut epoch = p.epoch_changes();
    // The current-thread executor has not polled the bus task; force a real bus lag.
    for n in 0..256 {
        bus.emit(wakeup(n));
    }
    rt.block_on(epoch.changed()).unwrap();
    assert!(activity_snapshot(&p).is_empty());
}

#[test]
fn activity_snapshot_cut_keeps_starts_and_finishes_on_one_side_of_high_water() {
    for finishing in [false, true] {
        let (rt, _dir, _store, p, _now, _bus) = activity_fixture();
        if finishing {
            p.observe_activity(activity_start(
                "cut",
                "shell",
                serde_json::json!({"command":"true"}),
            ));
        }
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        p.state.lock().unwrap().snapshot_hook = Some({
            let entered = entered.clone();
            let release = release.clone();
            Arc::new(move || {
                entered.wait();
                release.wait();
            })
        });
        let snap = {
            let p = p.clone();
            std::thread::spawn(move || p.subscribe_with_snapshot(&all(None)).unwrap())
        };
        entered.wait();
        let updating = Arc::new(Barrier::new(2));
        let update = {
            let p = p.clone();
            let updating = updating.clone();
            std::thread::spawn(move || {
                updating.wait();
                p.observe_activity(if finishing {
                    activity_done("cut", 2240, false)
                } else {
                    activity_start("cut", "shell", serde_json::json!({"command":"true"}))
                });
            })
        };
        updating.wait();
        release.wait();
        let mut sub = snap.join().unwrap();
        update.join().unwrap();
        let initial = sub
            .started
            .initial_page
            .as_ref()
            .unwrap()
            .items
            .iter()
            .filter(|item| matches!(item, SnapshotItem::ToolActivity(_)))
            .count();
        assert_eq!(initial, usize::from(finishing));
        let change = rt.block_on(sub.recv()).unwrap();
        assert_eq!(
            change.cursor.sequence.get(),
            sub.started.high_water.sequence.get() + 1
        );
        assert_eq!(
            matches!(
                change.event,
                UiEvent::Execution(ExecutionEvent::ToolFinished { .. })
            ),
            finishing
        );
    }
}

#[test]
fn a_new_projection_does_not_reconstruct_activity_from_saved_starts() {
    let (rt, dir, store, p, _now, _bus) = activity_fixture();
    let start = activity_start("saved", "wait", serde_json::json!({}));
    store.handle().append_event(None, &start).unwrap();
    p.observe_activity(start);
    assert_eq!(activity_snapshot(&p).len(), 1);
    let fresh = Projection::new(p.runtime.clone(), rt.handle(), Default::default());
    fresh
        .configure_projects(
            vec![(ProjectId::new("project").unwrap(), dir.path().to_path_buf())],
            Default::default(),
        )
        .unwrap();
    assert!(activity_snapshot(&fresh).is_empty());
}

#[test]
fn activity_elapsed_uses_monotonic_time_but_finishes_use_wall_clock_stamps() {
    let (rt, _dir, _store, p, now, _bus) = activity_fixture();
    let mut sub = p.subscribe_with_snapshot(&all(None)).unwrap();
    let mut start = activity_start("clock", "wait", serde_json::json!({}));
    start.meta.wall_clock = std::time::UNIX_EPOCH + std::time::Duration::from_millis(500_000);
    p.observe_activity(start);
    let _ = rt.block_on(sub.recv()).unwrap();
    // The manual "now" is 2000ms and the start's monotonic stamp is 1000ms,
    // despite the unrelated wall clock (which can jump while a call runs).
    assert_eq!(activity_snapshot(&p)[0].elapsed_ms, 1000);
    now.store(3500, Ordering::Relaxed);
    assert_eq!(activity_snapshot(&p)[0].elapsed_ms, 2500);
    let mut done = activity_done("clock", 2240, false);
    done.meta.wall_clock = std::time::UNIX_EPOCH + std::time::Duration::from_millis(500_125);
    p.observe_activity(done);
    assert!(matches!(
        rt.block_on(sub.recv()).unwrap().event,
        UiEvent::Execution(ExecutionEvent::ToolFinished {
            duration_ms: Some(125),
            ..
        })
    ));
}

#[test]
fn optional_activity_cannot_overflow_an_almost_full_workspace_snapshot() {
    let (_rt, dir, store, p, _now, _bus) = activity_fixture();
    let mut template = p.state.lock().unwrap().models.threads["thread"]
        .summary
        .clone();
    template.id = ThreadId::new("bulk-00000").unwrap();
    template.title = "x".repeat(workbench_protocol::limits::MAX_TITLE_BYTES);
    let item_bytes = serde_json::to_vec(&SnapshotItem::Thread(template.clone()))
        .unwrap()
        .len()
        + 1;
    // Leave less than the 16 inline activities, but enough for per-row revision
    // digit differences and the fixture's other canonical snapshot items.
    let count = (MAX_SNAPSHOT_BYTES - 128 * 1024) / item_bytes;
    let threads: Vec<_> = (0..count)
        .map(|n| storage::threads::ThreadUpsert {
            thread_id: format!("bulk-{n:05}"),
            project_id: Some("project".into()),
            project_root: Some(dir.path().to_string_lossy().into_owned()),
            title: Some(template.title.clone()),
            ..Default::default()
        })
        .collect();
    store.handle().upsert_threads_host(&threads).unwrap();
    for n in 0..activity::MAX_LIVE_TOOLS {
        p.observe_activity(activity_start(
            &format!("call-{n}"),
            "run_js",
            serde_json::json!({"code":"x".repeat(16 * 1024)}),
        ));
    }
    p.subscribe_with_snapshot(&all(None)).unwrap();
    let state = p.state.lock().unwrap();
    let items = &state.snapshots.back().unwrap().items;
    let shown = items
        .iter()
        .filter(|item| matches!(item, SnapshotItem::ToolActivity(_)))
        .count();
    assert!(shown < activity::MAX_LIVE_TOOLS);
    assert_eq!(state.activities.len(), activity::MAX_LIVE_TOOLS);
    assert!(serde_json::to_vec(items).unwrap().len() <= MAX_SNAPSHOT_BYTES);
}
