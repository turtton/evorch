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
    assert!(
        registry
            .entries
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.task_id == "last")
    );
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

// An older attempt that returned with an unresolved write does not block a newer attempt of
// the same task, and it stays in the retry set although it left the suspend cohort.
#[test]
fn an_unresolved_older_attempt_coexists_with_the_newer_one_in_separate_sets() {
    let registry = LearningExecutions::default();
    let changes = watch::channel(0).0;
    let older = registry.register("task", changes.clone()).unwrap();
    older.state.lock().unwrap().unsafe_result = Some("unconfirmed".into());
    registry.returned(&older);
    assert!(matches!(*older.returned.borrow(), Some(Err(_))));

    let newer = registry.register("task", changes.clone()).unwrap();
    assert!(
        registry.register("task", changes).is_none(),
        "a running attempt still owns the task"
    );

    let cohort = registry.cohort();
    assert_eq!(cohort.len(), 1);
    assert!(Arc::ptr_eq(&cohort[0], &newer));
    let retry = registry.unresolved();
    assert_eq!(retry.len(), 1);
    assert!(Arc::ptr_eq(&retry[0], &older));

    older.state.lock().unwrap().unsafe_result = None;
    registry.returned(&older);
    assert!(registry.unresolved().is_empty());
    assert!(Arc::ptr_eq(&registry.cohort()[0], &newer));
}

// A suspend readmits an entry whose cleanup became unresolved again; it must neither replace
// nor be lost behind a newer attempt that already holds the task.
#[test]
fn readmitting_an_older_entry_keeps_both_and_never_replaces_the_newer() {
    let registry = LearningExecutions::default();
    let changes = watch::channel(0).0;
    let older = registry.register("task", changes.clone()).unwrap();
    registry.returned(&older);
    let newer = registry.register("task", changes).unwrap();
    older.state.lock().unwrap().unsafe_result = Some("unconfirmed".into());
    older.returned.send_replace(Some(Err("unconfirmed".into())));

    registry.readmit(&older);
    registry.readmit(&older);

    assert_eq!(registry.entries.lock().unwrap().len(), 2);
    assert!(Arc::ptr_eq(&registry.cohort()[0], &newer));
    assert_eq!(registry.cohort().len(), 1);
    assert!(Arc::ptr_eq(&registry.unresolved()[0], &older));
}
