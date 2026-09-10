use runtime::ownership::{Lease, OwnerState};

#[test]
fn lease_is_suspect_before_grace_and_stale_after_grace() {
    // Given: a lease with an explicit expiry.
    let lease = Lease {
        owner_id: "owner-a".into(),
        generation: 1,
        expires_at: 100,
    };
    // When / Then: time advances through exact boundaries.
    assert_eq!(lease.observe(99, 50), OwnerState::Running);
    assert_eq!(lease.observe(100, 50), OwnerState::Suspect);
    assert_eq!(lease.observe(150, 50), OwnerState::Stale);
}
