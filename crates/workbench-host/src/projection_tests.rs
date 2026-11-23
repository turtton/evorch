use super::*;
use event_bus::{Event, EventBus, LifecycleEvent};
use providers::{ChatResponse, Message, ToolSpec};
use runtime::{AgentInvocationContext, AgentModel, Role, RuntimeError};
use std::sync::Barrier;
use workbench_protocol::namespaces::{LifecycleEvent as WireLifecycleEvent, ProjectionEvent};

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
            closed: false
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
