use super::*;
use crate::ownership::OwnerHost;

/// The owner's heartbeat moves the lease expiry in the registry, so a permit resolved
/// now differs from the permit the run started with in `expires_at` alone.
#[tokio::test]
async fn a_renewed_lease_is_still_the_owner_of_the_live_root() {
    let mut f = Fixture::new();
    let owner = OwnerHost::open(
        f._dir.path(),
        Default::default(),
        f.runtime.shared.bus.clone(),
    )
    .unwrap();
    let permit = owner.start("renewed-lead").unwrap();
    let authority = |permit| RunConfig {
        ownership: Some(permit),
        interactive: true,
        keep_alive: true,
        conversation: true,
        ..Default::default()
    };
    let lead = f
        .runtime
        .delegate_chat(
            "renewed-lead",
            Role::Orchestrator,
            "coordinate".into(),
            authority(permit.clone()),
        )
        .unwrap();
    f.phase(lead, AgentRunPhase::Waiting).await;
    let target = |permit| LaneDeliveryTarget {
        root_run_id: lead,
        generation: 1,
        authority: authority(permit),
    };

    let mut renewed = permit.clone();
    renewed.lease.expires_at += 5_000;
    assert_eq!(
        f.runtime
            .check_delivery_target("renewed-lead", &target(renewed)),
        Ok(())
    );

    let mut taken_over = permit;
    taken_over.lease.generation += 1;
    assert_eq!(
        f.runtime
            .check_delivery_target("renewed-lead", &target(taken_over)),
        Err(LaneDeliveryNack::OwnershipLost)
    );
}
