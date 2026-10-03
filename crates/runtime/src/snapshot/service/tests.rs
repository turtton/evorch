use super::*;
use event_bus::{EventKind, EventReceiver};

fn requester(run: &str) -> WorkspaceLockHolder {
    WorkspaceLockHolder {
        run_id: run.into(),
        call_id: format!("{run}-call"),
        tool_name: "shell".into(),
        command: Some(format!("echo {run}")),
    }
}

fn fixture() -> (tempfile::TempDir, SnapshotService, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    let other = temp.path().join("other");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&other).unwrap();
    let service = SnapshotService::new(&root, &temp.path().join("snapshots")).unwrap();
    (temp, service, other)
}

async fn wait_event(receiver: &mut EventReceiver, run: &str) -> Option<WorkspaceWait> {
    let EventKind::Lifecycle(LifecycleEvent::WorkspaceWaitChanged {
        run_id,
        call_id,
        waiting,
    }) = receiver.recv().await.unwrap().kind
    else {
        panic!("expected workspace wait event");
    };
    assert_eq!(run_id, run);
    assert_eq!(call_id, format!("{run}-call"));
    waiting
}

#[tokio::test]
async fn only_contended_workspace_reports_a_wait() {
    let (_temp, service, other) = fixture();
    let bus = Arc::new(EventBus::new(32));
    let mut events = bus.subscribe();
    let owner = service
        .lock_observed(None, requester("owner"), bus.clone())
        .await
        .unwrap();
    let independent = service
        .lock_observed(Some(&other), requester("independent"), bus.clone())
        .await
        .unwrap();
    assert!(events.drain_pending_snapshot().is_empty());

    let mut same = Box::pin(service.lock_observed(None, requester("same"), bus));
    assert!(futures_util::poll!(same.as_mut()).is_pending());
    let waiting = wait_event(&mut events, "same").await.unwrap();
    assert_eq!(waiting.workspace_root, owner.root());
    assert_eq!(waiting.tool_name, "shell");
    assert_eq!(waiting.command, requester("same").command);
    assert_eq!(waiting.holder, Some(requester("owner")));
    drop(owner);
    drop(same.await.unwrap());
    assert!(wait_event(&mut events, "same").await.is_none());
    drop(independent);
    assert!(events.drain_pending_snapshot().is_empty());
}

#[tokio::test]
async fn uncontended_acquisitions_do_not_report_cooperative_yields_as_workspace_waits() {
    let (_temp, service, _) = fixture();
    let bus = Arc::new(EventBus::new(32));
    let mut events = bus.subscribe();
    // Cross Tokio's cooperative budget repeatedly while the workspace remains
    // free. A scheduler yield is not evidence of another workspace owner.
    for _ in 0..256 {
        drop(
            service
                .lock_observed(None, requester("uncontended"), bus.clone())
                .await
                .unwrap(),
        );
        tokio::task::consume_budget().await;
    }
    assert!(events.drain_pending_snapshot().is_empty());
}

#[tokio::test]
async fn holder_handoff_preserves_fifo_even_when_later_waiter_observes_updates_first() {
    let (_temp, service, _) = fixture();
    let bus = Arc::new(EventBus::new(32));
    let mut events = bus.subscribe();
    let first = service
        .lock_observed(None, requester("first"), bus.clone())
        .await
        .unwrap();
    let mut second = Box::pin(service.lock_observed(None, requester("second"), bus.clone()));
    let mut third = Box::pin(service.lock_observed(None, requester("third"), bus));
    assert!(futures_util::poll!(second.as_mut()).is_pending());
    assert_eq!(
        wait_event(&mut events, "second").await.unwrap().holder,
        Some(requester("first"))
    );
    assert!(futures_util::poll!(third.as_mut()).is_pending());
    assert_eq!(
        wait_event(&mut events, "third").await.unwrap().holder,
        Some(requester("first"))
    );

    drop(first);
    // Poll the later waiter first: observing an owner update must not move the
    // earlier acquisition to the back of the mutex queue.
    assert!(futures_util::poll!(third.as_mut()).is_pending());
    assert!(
        wait_event(&mut events, "third")
            .await
            .unwrap()
            .holder
            .is_none()
    );
    let second = second.await.unwrap();
    assert!(wait_event(&mut events, "second").await.is_none());
    assert!(futures_util::poll!(third.as_mut()).is_pending());
    assert_eq!(
        wait_event(&mut events, "third").await.unwrap().holder,
        Some(requester("second"))
    );
    drop(second);
    let third = third.await.unwrap();
    assert!(wait_event(&mut events, "third").await.is_none());
    drop(third);

    assert!(events.drain_pending_snapshot().is_empty());
}

#[tokio::test]
async fn cancelling_an_unknown_owner_wait_clears_it_and_removes_it_from_the_queue() {
    let (_temp, service, _) = fixture();
    let bus = Arc::new(EventBus::new(32));
    let mut events = bus.subscribe();
    // A prior observed lease must leave no stale owner for a general caller.
    drop(
        service
            .lock_observed(None, requester("past"), bus.clone())
            .await
            .unwrap(),
    );
    let owner = service.lock(None).await.unwrap();
    let mut cancelled = Box::pin(service.lock_observed(None, requester("cancelled"), bus.clone()));
    assert!(futures_util::poll!(cancelled.as_mut()).is_pending());
    assert!(
        wait_event(&mut events, "cancelled")
            .await
            .unwrap()
            .holder
            .is_none()
    );
    drop(cancelled);
    assert!(wait_event(&mut events, "cancelled").await.is_none());

    let mut next = Box::pin(service.lock_observed(None, requester("next"), bus));
    assert!(futures_util::poll!(next.as_mut()).is_pending());
    assert!(
        wait_event(&mut events, "next")
            .await
            .unwrap()
            .holder
            .is_none()
    );
    drop(owner);
    drop(next.await.unwrap());
    assert!(wait_event(&mut events, "next").await.is_none());
    assert!(events.drain_pending_snapshot().is_empty());
}

#[tokio::test]
async fn failed_workspace_open_does_not_publish_a_wait_or_owner() {
    let (_temp, service, other) = fixture();
    let bus = Arc::new(EventBus::new(8));
    let mut events = bus.subscribe();
    assert!(
        service
            .lock_observed(Some(&other.join("missing")), requester("failed"), bus)
            .await
            .is_err()
    );
    assert!(events.drain_pending_snapshot().is_empty());
}
