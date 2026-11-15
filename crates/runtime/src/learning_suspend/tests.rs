use super::*;

#[tokio::test(start_paused = true)]
async fn learning_cohort_deadline_does_not_drop_owned_cleanup_or_restart_per_entry() {
    let registry = Arc::new(LearningExecutions::default());
    let changes = watch::channel(0).0;
    let first = registry.register("first", changes.clone()).unwrap();
    let last = registry.register("last", changes).unwrap();
    let (_fence, fenced) = watch::channel(Some(Ok(())));
    let suspension = LearningSuspension {
        id: "request".into(),
        entries: vec![first.clone(), last.clone()],
        recovery_taken: AtomicBool::new(false),
        fenced,
    };
    let (release, returned) = tokio::sync::oneshot::channel();
    let owner = registry.clone();
    let owned = last.clone();
    registry.own(tokio::spawn(async move {
        returned.await.unwrap();
        owner.returned(&owned);
    }));
    let wait =
        suspension.wait_until(tokio::time::Instant::now() + std::time::Duration::from_secs(5));
    tokio::pin!(wait);
    assert!(futures_util::poll!(&mut wait).is_pending());
    tokio::time::advance(std::time::Duration::from_secs(4)).await;
    registry.returned(&first);
    assert!(futures_util::poll!(&mut wait).is_pending());
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    assert_eq!(wait.await.unwrap(), LearningSuspendWait::DeadlineExceeded);
    assert!(last.returned.borrow().is_none());
    assert!(registry.entries.lock().unwrap().contains_key("last"));
    release.send(()).unwrap();
    suspension.wait().await.unwrap();
    assert!(registry.entries.lock().unwrap().is_empty());
}

// Cleanup callbacks can arrive twice (the return-time reconcile, the automatic retry and an
// explicit suspend reconcile all report the same entry). A late one for attempt 1 must not
// release the registration of attempt 2 that reuses the task id, or the next suspend would
// seal over a job that is still Started.
#[test]
fn a_late_callback_for_an_older_attempt_never_drops_the_newer_registration() {
    let registry = LearningExecutions::default();
    let changes = watch::channel(0).0;
    let first = registry.register("task", changes.clone()).unwrap();
    registry.returned(&first);
    assert!(registry.entries.lock().unwrap().is_empty());
    let second = registry.register("task", changes).unwrap();

    registry.returned(&first);

    let cohort = registry.cohort();
    assert_eq!(cohort.len(), 1);
    assert!(Arc::ptr_eq(&cohort[0], &second));
    assert!(second.returned.borrow().is_none());
    registry.returned(&second);
    assert!(registry.entries.lock().unwrap().is_empty());
}

// One slot per task: an entry whose write outcome is unknown keeps the slot, so a registration
// that never starts (a refused dispatch, a scheduler race) can neither displace it nor make its
// cleanup look finished.
#[test]
fn an_unresolved_entry_keeps_its_slot_so_no_attempt_can_register_over_it() {
    let registry = LearningExecutions::default();
    let changes = watch::channel(0).0;
    let older = registry.register("task", changes.clone()).unwrap();
    older.state.lock().unwrap().unsafe_result = Some("unconfirmed".into());
    registry.returned(&older);
    assert!(matches!(*older.returned.borrow(), Some(Err(_))));

    assert!(registry.register("task", changes.clone()).is_none());

    assert!(Arc::ptr_eq(&registry.cohort()[0], &older));
    assert!(Arc::ptr_eq(&registry.unresolved()[0], &older));
    older.state.lock().unwrap().unsafe_result = None;
    registry.returned(&older);
    assert!(registry.cohort().is_empty() && registry.unresolved().is_empty());
    assert!(registry.register("task", changes).is_some());
}

// An Interrupted job is reserved for its cancel or recovery write. Its entry stays registered
// (blocking a new attempt) but is neither waited on by a suspend nor retried, until released.
#[test]
fn an_interrupted_entry_holds_its_slot_until_it_is_released() {
    let registry = LearningExecutions::default();
    let changes = watch::channel(0).0;
    let entry = registry.register("task", changes.clone()).unwrap();
    entry.state.lock().unwrap().key = Some(LearningAttemptKey {
        task_id: "task".into(),
        owner: "owner".into(),
        attempt: 1,
        generation: 1,
        revision: 3,
        state: LearningJobState::Interrupted,
    });
    registry.returned(&entry);

    assert!(matches!(*entry.returned.borrow(), Some(Ok(()))));
    assert!(registry.register("task", changes.clone()).is_none());
    assert!(registry.cohort().is_empty());
    assert!(registry.unresolved().is_empty());

    registry.release(&entry);
    assert!(registry.register("task", changes).is_some());
}
