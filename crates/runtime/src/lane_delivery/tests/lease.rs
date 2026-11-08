use super::*;

pub(super) struct Hosts {
    pub(super) targets: HashMap<String, LaneDeliveryTarget>,
    pub(super) now: AtomicI64,
}
impl LaneDeliveryHost for Hosts {
    fn resolve(&self, thread: &str) -> Result<LaneDeliveryTarget, LaneDeliveryNack> {
        self.targets
            .get(thread)
            .cloned()
            .ok_or(LaneDeliveryNack::UnknownThread)
    }
    fn now_ns(&self) -> i64 {
        self.now.load(Ordering::SeqCst)
    }
}

#[tokio::test(start_paused = true)]
async fn occupied_root_does_not_block_other_destinations_and_releases_expired_flight() {
    let mut f = Fixture::new();
    let (lead, first) = f.lead().await;
    let other = f
        .runtime
        .delegate_chat(
            "z-delivery-lead",
            Role::Orchestrator,
            "coordinate independently".into(),
            first.target.authority.clone(),
        )
        .unwrap();
    f.phase(other, AgentRunPhase::Waiting).await;
    let lane = f.lane(lead, "occupied-source").await;
    let other_lane = f.lane(other, "independent-source").await;
    let host = Arc::new(Hosts {
        targets: HashMap::from([
            ("delivery-lead".into(), first.target.clone()),
            (
                "z-delivery-lead".into(),
                LaneDeliveryTarget {
                    root_run_id: other,
                    ..first.target.clone()
                },
            ),
        ]),
        now: AtomicI64::new(100),
    });
    *f.runtime.shared.lanes.delivery.host.lock().unwrap() = Some(host.clone());
    f.model.block_next.store(true, Ordering::SeqCst);
    f.runtime
        .send_message(lead, "hold this provider request".into())
        .unwrap();
    f.model.blocked.notified().await;
    f.report(&lane, Some(1), LaneStatus::Ready);
    f.report(&other_lane, Some(1), LaneStatus::Ready);
    let runtime = f.runtime.clone();
    let scan = tokio::spawn(async move { runtime.drive_pending_lane_deliveries().await });
    // This terminal turn boundary must arrive while the first root is occupied.
    f.phase(other, AgentRunPhase::Waiting).await;
    assert_eq!(f.receipts(), 1);
    let old = f
        .runtime
        .shared
        .lanes
        .delivery
        .pending
        .lock()
        .unwrap()
        .get(&lead)
        .expect("occupied destination is waiting for receipt")
        .claim
        .lane()
        .expect("Lane source")
        .clone();
    host.now.store(old.lease_until_ns, Ordering::SeqCst);
    tokio::time::advance(Duration::from_nanos(LEASE_NS.cast_unsigned())).await;
    scan.await.unwrap().unwrap();
    assert!(
        !f.runtime
            .shared
            .lanes
            .delivery
            .pending
            .lock()
            .unwrap()
            .contains_key(&lead)
    );
    assert!(
        !f.runtime
            .shared
            .lanes
            .delivery
            .flying
            .lock()
            .unwrap()
            .contains("delivery-lead")
    );
    assert!(
        f.storage
            .handle()
            .ack_lane_delivery(old.clone(), old.lease_until_ns)
            .is_err()
    );
    let retry = f
        .storage
        .handle()
        .claim_lane_deliveries(LaneClaimRequest {
            destination_thread_id: old.destination_thread_id,
            expected_destination_epoch: old.destination_epoch,
            owner: old.owner,
            token: "reclaimed-after-local-expiry".into(),
            now_ns: old.lease_until_ns,
            lease_until_ns: old.lease_until_ns + LEASE_NS,
            allow_non_waking: true,
        })
        .unwrap();
    assert!(matches!(retry, LaneClaimResult::Claimed(_)));
    f.runtime.stop(lead, crate::StopScope::SelfOnly).unwrap();
    f.runtime.wait(lead).await.unwrap();
}
