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
