use super::*;
use event_bus::{Event, EventKind, ToolEvent};
use std::future::{Future, poll_fn};
use std::task::Poll;

fn request<'a>(run: &'a str, call: &'a str) -> ApprovalRequest<'a> {
    ApprovalRequest {
        run_id: run,
        thread_id: Some("thread-1"),
        tool_name: "shell",
        call_id: call,
        input: Some(serde_json::json!({"command":"echo hello"})),
    }
}
fn register(registry: &Arc<ApprovalRegistry>) -> PendingApproval {
    registry.register(request("run-1", "same-call"), Duration::from_secs(60))
}
fn decide(
    registry: &ApprovalRegistry,
    id: &ApprovalId,
    decision: ApprovalDecision,
) -> Result<ApprovalSnapshot, ApprovalError> {
    registry.resolve(id, 1, decision, ApprovalProvenance::DESKTOP)
}

#[tokio::test]
async fn decision_before_wait_is_retained_and_only_first_decision_wins() {
    let bus = Arc::new(EventBus::new(16));
    let registry = ApprovalRegistry::for_bus(&bus);
    assert!(Arc::ptr_eq(&registry, &ApprovalRegistry::for_bus(&bus)));
    for decision in [ApprovalDecision::Approve, ApprovalDecision::Deny] {
        let pending = register(&registry);
        let snapshot = decide(&registry, &pending.id, decision).unwrap();
        assert_eq!(snapshot.revision, 2);
        assert_eq!(snapshot.decided_by, Some(ApprovalProvenance::DESKTOP));
        assert_eq!(
            decide(&registry, &pending.id, decision).unwrap_err(),
            ApprovalError::AlreadyDecided(snapshot.state)
        );
        assert_eq!(pending.wait().await, snapshot.state);
    }
    assert!(registry.list_pending(None, PAGE_MAX).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simultaneous_decisions_have_exactly_one_winner() {
    let bus = Arc::new(EventBus::new(16));
    let registry = ApprovalRegistry::for_bus(&bus);
    let pending = register(&registry);
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let mut tasks = tokio::task::JoinSet::new();
    for decision in [ApprovalDecision::Approve, ApprovalDecision::Deny] {
        let (registry, id, barrier) = (registry.clone(), pending.id.clone(), barrier.clone());
        tasks.spawn(async move {
            barrier.wait().await;
            decide(&registry, &id, decision)
        });
    }
    let mut winners = Vec::new();
    while let Some(result) = tasks.join_next().await {
        match result.unwrap() {
            Ok(s) => winners.push(s.state),
            Err(e) => assert!(matches!(e, ApprovalError::AlreadyDecided(_))),
        }
    }
    assert_eq!(winners.len(), 1);
    assert_eq!(pending.wait().await, winners[0]);
}

#[tokio::test(start_paused = true)]
async fn deadline_is_inclusive_and_checked_by_cas_not_waiter_scheduling() {
    let bus = Arc::new(EventBus::new(16));
    let registry = ApprovalRegistry::for_bus(&bus);
    let pending = register(&registry);
    tokio::time::advance(Duration::from_secs(60)).await;
    assert_eq!(
        decide(&registry, &pending.id, ApprovalDecision::Approve).unwrap_err(),
        ApprovalError::Expired
    );
    assert_eq!(pending.wait().await, ApprovalState::Expired);
    // A decision immediately before the boundary wins even if wait starts later.
    let pending = register(&registry);
    tokio::time::advance(Duration::from_secs(59)).await;
    decide(&registry, &pending.id, ApprovalDecision::Approve).unwrap();
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(pending.wait().await, ApprovalState::Approved);
}

#[tokio::test(start_paused = true)]
async fn gate_timeout_uses_virtual_time() {
    let bus = Arc::new(EventBus::new(16));
    let gate = ApprovalGate::new(bus.clone(), Duration::from_secs(60));
    let mut events = bus.subscribe();
    let task = tokio::spawn(async move { gate.request("shell", "call").await });
    events.recv().await.unwrap();
    tokio::time::advance(Duration::from_secs(60)).await;
    assert_eq!(task.await.unwrap(), ApprovalOutcome::TimedOut);
    assert!(
        ApprovalRegistry::for_bus(&bus)
            .list_pending(None, 10)
            .is_empty()
    );
}

#[tokio::test]
async fn owner_close_drop_and_rebind_fail_closed() {
    let bus = Arc::new(EventBus::new(32));
    let registry = ApprovalRegistry::for_bus(&bus);
    for drop_owner in [false, true] {
        let owner = Arc::new(ApprovalOwner::default());
        registry.bind_run("run-1", Some("canonical-thread".into()), &owner);
        let pending = register(&registry);
        let id = pending.id.clone();
        assert_eq!(
            registry.get(&id).unwrap().thread_id.as_deref(),
            Some("canonical-thread")
        );
        if !drop_owner {
            owner.close();
        }
        drop(owner);
        assert_eq!(
            decide(&registry, &id, ApprovalDecision::Approve).unwrap_err(),
            ApprovalError::Cancelled
        );
        assert_eq!(pending.wait().await, ApprovalState::Cancelled);
        assert_eq!(register(&registry).wait().await, ApprovalState::Cancelled);
    }
    let owner = Arc::new(ApprovalOwner::default());
    registry.bind_run("run-1", None, &owner);
    let old = register(&registry);
    let old_attempt = registry.get(old.id()).unwrap().execution_attempt;
    let resumed_owner = Arc::new(ApprovalOwner::default());
    registry.bind_run("run-1", None, &resumed_owner);
    let resumed = register(&registry);
    assert_ne!(
        registry.get(resumed.id()).unwrap().execution_attempt,
        old_attempt
    );
    assert_ne!(old.id(), resumed.id());
    assert_eq!(old.wait().await, ApprovalState::Cancelled);
    decide(&registry, resumed.id(), ApprovalDecision::Approve).unwrap();
    assert_eq!(resumed.wait().await, ApprovalState::Approved);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_cancellation_racing_a_decision_has_one_terminal_result() {
    let bus = Arc::new(EventBus::new(16));
    let registry = ApprovalRegistry::for_bus(&bus);
    let owner = Arc::new(ApprovalOwner::default());
    registry.bind_run("run-1", None, &owner);
    let pending = register(&registry);
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let (other, other_barrier) = (owner.clone(), barrier.clone());
    let closer = tokio::spawn(async move {
        other_barrier.wait().await;
        other.close();
    });
    barrier.wait().await;
    let result = decide(&registry, &pending.id, ApprovalDecision::Approve);
    closer.await.unwrap();
    let state = pending.wait().await;
    match result {
        Ok(s) => assert_eq!(state, s.state),
        Err(e) => {
            assert_eq!(e, ApprovalError::Cancelled);
            assert_eq!(state, ApprovalState::Cancelled);
        }
    }
    assert!(registry.list_pending(None, 1).is_empty());
}

#[tokio::test]
async fn future_drop_cleans_up_without_detached_tasks() {
    let bus = Arc::new(EventBus::new(16));
    let registry = ApprovalRegistry::for_bus(&bus);
    let gate = ApprovalGate::new(bus.clone(), Duration::from_secs(60));
    let mut future = Box::pin(gate.request_for(request("run-1", "call")));
    poll_fn(|cx| {
        assert!(future.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    let snapshot = registry.list_pending(None, 1).pop().unwrap();
    drop(future);
    assert_eq!(
        decide(&registry, &snapshot.id, ApprovalDecision::Approve).unwrap_err(),
        ApprovalError::Cancelled
    );
    assert!(registry.list_pending(None, 1).is_empty());
}

#[tokio::test]
async fn forged_resolution_and_replayed_request_cannot_authorize_or_resurrect() {
    let bus = Arc::new(EventBus::new(16));
    let registry = ApprovalRegistry::for_bus(&bus);
    let mut events = bus.subscribe();
    let pending = register(&registry);
    let event = events.recv().await.unwrap();
    assert!(
        matches!(&event.kind, EventKind::Tool(ToolEvent::ApprovalRequested { call_id, .. }) if call_id == &pending.id.value)
    );
    bus.emit(Event::new(ToolEvent::ApprovalResolved {
        call_id: pending.id.value.clone(),
        approved: true,
    }));
    assert_eq!(
        registry.get(&pending.id).unwrap().state,
        ApprovalState::Pending
    );
    let id = pending.id.clone();
    drop(pending);
    bus.emit(event);
    let newer = register(&registry);
    assert_ne!(newer.id, id);
    assert_eq!(
        decide(&registry, &id, ApprovalDecision::Approve).unwrap_err(),
        ApprovalError::Cancelled
    );
    assert_eq!(registry.list_pending(None, 10).len(), 1);
    decide(&registry, &newer.id, ApprovalDecision::Deny).unwrap();
    assert_eq!(newer.wait().await, ApprovalState::Denied);
}

#[tokio::test]
async fn revision_epoch_and_restart_are_distinct_from_terminal_errors() {
    let bus = Arc::new(EventBus::new(16));
    let registry = ApprovalRegistry::for_bus(&bus);
    let pending = register(&registry);
    assert_eq!(
        registry
            .resolve(
                &pending.id,
                9,
                ApprovalDecision::Approve,
                ApprovalProvenance::DESKTOP
            )
            .unwrap_err(),
        ApprovalError::RevisionMismatch { current: 1 }
    );
    let other_bus = Arc::new(EventBus::new(16));
    let restarted = ApprovalRegistry::for_bus(&other_bus);
    assert_eq!(
        decide(&restarted, &pending.id, ApprovalDecision::Approve).unwrap_err(),
        ApprovalError::WrongEpoch
    );
    assert!(restarted.list_pending(None, 10).is_empty());
    let new = register(&restarted);
    assert_ne!(new.id.value, pending.id.value);
}

#[tokio::test]
async fn paged_snapshots_and_change_stream_preserve_bounded_metadata_and_provenance() {
    let bus = Arc::new(EventBus::new(512));
    let registry = ApprovalRegistry::for_bus(&bus);
    let mut changes = registry.subscribe();
    let pending = registry.register(
        ApprovalRequest {
            input: Some(serde_json::json!({"command":"あ".repeat(SUMMARY_MAX_BYTES)})),
            ..request("run-1", "call")
        },
        Duration::from_secs(60),
    );
    let first = changes.recv().await.unwrap();
    assert!(first.summary.len() <= SUMMARY_MAX_BYTES);
    assert_eq!(first.id, pending.id);
    assert_eq!(first.state, ApprovalState::Pending);
    let another = register(&registry);
    let page = registry.list_pending(None, 1);
    assert_eq!(page.len(), 1);
    assert_eq!(
        registry.list_pending(Some(page[0].attempt), 1)[0].id,
        another.id
    );
    assert!(registry.list_pending(None, 0).is_empty());
    let provenance = ApprovalProvenance {
        actor: ApprovalActor::Human,
        surface: ApprovalSurface::Web,
    };
    registry
        .resolve(&pending.id, 1, ApprovalDecision::Approve, provenance)
        .unwrap();
    let _next_pending = changes.recv().await.unwrap();
    let terminal = changes.recv().await.unwrap();
    assert_eq!(terminal.revision, 2);
    assert_eq!(terminal.decided_by, Some(provenance));
    assert_eq!(pending.wait().await, ApprovalState::Approved);
}

#[tokio::test(start_paused = true)]
async fn expiry_and_cancellation_compete_without_reopening_terminal_rows() {
    let bus = Arc::new(EventBus::new(16));
    let registry = ApprovalRegistry::for_bus(&bus);
    let owner = Arc::new(ApprovalOwner::default());
    registry.bind_run("run-1", None, &owner);
    let pending = register(&registry);
    tokio::time::advance(Duration::from_secs(60)).await;
    owner.close();
    assert_eq!(
        decide(&registry, &pending.id, ApprovalDecision::Approve).unwrap_err(),
        ApprovalError::Expired
    );
    assert_eq!(pending.wait().await, ApprovalState::Expired);
    let owner = Arc::new(ApprovalOwner::default());
    registry.bind_run("run-1", None, &owner);
    let pending = register(&registry);
    owner.close();
    tokio::time::advance(Duration::from_secs(60)).await;
    assert_eq!(
        decide(&registry, &pending.id, ApprovalDecision::Approve).unwrap_err(),
        ApprovalError::Cancelled
    );
    assert_eq!(pending.wait().await, ApprovalState::Cancelled);
}

#[tokio::test]
async fn page_cap_id_bounds_and_evicted_tombstones_remain_fail_closed() {
    let bus = Arc::new(EventBus::new(512));
    let registry = ApprovalRegistry::for_bus(&bus);
    let first = register(&registry);
    let evicted = first.id.clone();
    drop(first);
    for _ in 0..1024 {
        drop(register(&registry));
    }
    assert!(registry.get(&evicted).is_none());
    assert_eq!(
        decide(&registry, &evicted, ApprovalDecision::Approve).unwrap_err(),
        ApprovalError::Unknown
    );
    let handles: Vec<_> = (0..101).map(|_| register(&registry)).collect();
    assert_eq!(registry.list_pending(None, usize::MAX).len(), PAGE_MAX);
    let run = "あ".repeat(100);
    let call = "\n:".repeat(100);
    let pending = registry.register(request(&run, &call), Duration::from_secs(60));
    assert!(pending.id.value.len() <= 128);
    assert!(!pending.id.value.chars().any(char::is_control));
    assert_eq!(registry.get(&pending.id).unwrap().call_id, call);
    drop(handles);
}
