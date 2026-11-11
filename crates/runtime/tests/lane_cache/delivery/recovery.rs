//! Failure/suppression/Resume is not a provider-cache reset boundary.
use super::*;
pub(super) struct FailingHost {
    pub(super) inner: Arc<Host>,
    pub(super) fail: AtomicBool,
    pub(super) attempts: AtomicUsize,
}
impl LaneDeliveryHost for FailingHost {
    fn resolve(
        &self,
        thread: &str,
    ) -> Result<LaneDeliveryTarget, storage::lane_delivery::LaneDeliveryNack> {
        self.inner.resolve(thread)
    }
    fn now_ns(&self) -> i64 {
        self.inner.now_ns()
    }
    fn restore_root(
        &self,
        runtime: &AgentRuntime,
        target: &LaneDeliveryTarget,
    ) -> Result<(), storage::lane_delivery::LaneDeliveryNack> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            Err(storage::lane_delivery::LaneDeliveryNack::RestoreFailed)
        } else {
            self.inner.restore_root(runtime, target)
        }
    }
}
#[tokio::test]
async fn restore_failure_suppression_and_resume_reuse_the_sent_wire_prefix() {
    let mut f = Fixture::new().await;
    f.finish().await;
    let host = Arc::new(FailingHost {
        inner: f.host.clone(),
        fail: AtomicBool::new(true),
        attempts: AtomicUsize::new(0),
    });
    if let Some(task) = f.h.runtime.bind_lane_delivery_host(host.clone()) {
        task.abort();
    }
    f.report(0, event_bus::LaneStatus::Ready, 7);
    f.report(1, event_bus::LaneStatus::Ready, 8);
    let sent = f.requests();
    for _ in 0..6 {
        assert_eq!(
            f.h.runtime
                .drive_lane_deliveries("cache-delivery-lead")
                .await
                .unwrap(),
            LaneDeliveryOutcome::Nacked(storage::lane_delivery::LaneDeliveryNack::RestoreFailed)
        );
    }
    assert_eq!(host.attempts.load(Ordering::SeqCst), 3);
    assert_eq!(
        f.requests(),
        sent,
        "failed/suppressed restores send nothing and rewrite no provider input"
    );
    let conn = rusqlite::Connection::open(&f.config.db_path).unwrap();
    let (id, attempts): (String, u32) = conn
        .query_row(
            "SELECT episode_id,attempts FROM lane_recovery_episodes WHERE state='suppressed'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(attempts, 3);
    host.fail.store(false, Ordering::SeqCst);
    f.h.mock.enqueue(vec![text_response("one recovered batch")]);
    f.h.runtime
        .resume_lane_deliveries("cache-delivery-lead", f.lead, None)
        .unwrap();
    // Explicit Resume schedules the real driver. Its provider event is the
    // completion signal; we do not race a second manual driver against it.
    f.delivered_request().await;
    assert_eq!(
        f.store
            .handle()
            .lane_recovery_episode(id)
            .unwrap()
            .unwrap()
            .state,
        storage::lane_delivery::LaneRecoveryState::Resolved
    );
    assert_eq!(host.attempts.load(Ordering::SeqCst), 4);
    assert_eq!(
        f.h.runtime
            .drive_lane_deliveries("cache-delivery-lead")
            .await
            .unwrap(),
        LaneDeliveryOutcome::Empty
    );
    assert_eq!(f.requests().len(), sent.len() + 1);
    f.followup("ordinary turn after recovery suppression and Resume")
        .await;
    f.verify(0);
    let requests = f.requests();
    for pair in requests.windows(2) {
        assert_append_only(CacheProtocol::OpenAi, &pair[0], &pair[1]).unwrap();
    }
    f.h.runtime.cancel(f.lead).unwrap();
    f.h.runtime.wait(f.lead).await.unwrap();
}
