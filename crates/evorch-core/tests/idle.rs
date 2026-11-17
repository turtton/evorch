use evorch_core::lifecycle::IdleGrace;
use std::time::Duration;
use tokio::time::Instant;

#[tokio::test(start_paused = true)]
async fn injected_clock_preserves_deadline_until_activity_starts_a_full_new_grace() {
    let start = Instant::now();
    let grace = Duration::from_secs(60);
    let mut idle = IdleGrace::new(grace);
    assert_eq!(idle.update_at(start, false, 0, false), Some(start + grace));
    assert_eq!(
        idle.update_at(start + grace - Duration::from_nanos(1), false, 0, false),
        Some(start + grace)
    );
    assert_eq!(
        idle.update_at(start + grace, false, 0, false),
        Some(start + grace)
    );
    // The untracked-listener case is the legacy API only; production supplies
    // actual Web counts together with Unix counts. All retention resets grace.
    for (offset, (untracked_web, clients, work)) in [
        (false, 1, false),
        (false, 2, false),
        (false, 0, true),
        (true, 0, false),
    ]
    .into_iter()
    .enumerate()
    {
        let at = start + Duration::from_secs((offset as u64 + 1) * 1000);
        assert!(idle.update_at(at, untracked_web, clients, work).is_none());
        assert!(
            idle.update_at(at + grace * 2, untracked_web, clients, work)
                .is_none()
        );
        let disconnected = at + grace * 3;
        assert_eq!(
            idle.update_at(disconnected, false, 0, false),
            Some(disconnected + grace)
        );
    }
}

#[tokio::test(start_paused = true)]
async fn zero_grace_is_immediate_but_overflow_never_exits_early() {
    let now = Instant::now();
    let mut immediate = IdleGrace::new(Duration::ZERO);
    assert_eq!(immediate.update_at(now, false, 0, false), Some(now));
    assert_eq!(immediate.update_at(now, false, 1, false), None);
    assert_eq!(immediate.update_at(now, false, 0, true), None);
    let mut indefinite = IdleGrace::new(Duration::from_secs(u64::MAX));
    assert_eq!(indefinite.update_at(now, false, 0, false), None);
    assert_eq!(
        indefinite.update_at(now + Duration::from_secs(3600), false, 0, false),
        None
    );
}
