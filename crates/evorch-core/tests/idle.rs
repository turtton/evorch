use evorch_core::lifecycle::IdleGrace;
use std::time::Duration;

#[tokio::test(start_paused = true)]
async fn idle_grace_resets_for_clients_work_and_web_serving() {
    let mut idle = IdleGrace::new(Duration::from_secs(60));
    let first = idle.update(false, 0, false).unwrap();
    tokio::time::advance(Duration::from_secs(59)).await;
    assert!(tokio::time::Instant::now() < first);
    assert_eq!(idle.update(false, 0, false), Some(first));
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(tokio::time::Instant::now(), first);
    for (web, clients, work) in [(false, 1, false), (false, 0, true), (true, 0, false)] {
        assert!(idle.update(web, clients, work).is_none());
        tokio::time::advance(Duration::from_secs(600)).await;
        assert!(idle.update(web, clients, work).is_none());
        assert_eq!(
            idle.update(false, 0, false),
            Some(tokio::time::Instant::now() + Duration::from_secs(60))
        );
    }
}
