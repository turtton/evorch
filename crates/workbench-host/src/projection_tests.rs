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
