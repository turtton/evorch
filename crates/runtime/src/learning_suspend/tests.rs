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
    assert!(registry.active.lock().unwrap().contains_key("last"));
    release.send(()).unwrap();
    suspension.wait().await.unwrap();
    assert!(registry.active.lock().unwrap().is_empty());
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
    assert!(registry.active.lock().unwrap().is_empty());
    let second = registry.register("task", changes).unwrap();

    registry.returned(&first);

    let cohort: Vec<_> = registry.active.lock().unwrap().values().cloned().collect();
    assert_eq!(cohort.len(), 1);
    assert!(Arc::ptr_eq(&cohort[0], &second));
    assert!(second.returned.borrow().is_none());
    registry.returned(&second);
    assert!(registry.active.lock().unwrap().is_empty());
}
