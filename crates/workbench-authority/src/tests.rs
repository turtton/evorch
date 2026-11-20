use super::*;
#[cfg(unix)]
#[tokio::test]
async fn verified_connection_epoch_is_socket_scoped_and_dies_with_its_lease() {
    let (a, _a_peer) = tokio::net::UnixStream::pair().unwrap();
    let (b, _b_peer) = tokio::net::UnixStream::pair().unwrap();
    let a = verify_unix_peer(&a).unwrap();
    let b = verify_unix_peer(&b).unwrap();
    let first = a.authority().connection().unwrap();
    let same = a.authority().connection().unwrap();
    let other = b.authority().connection().unwrap();
    assert!(first.matches(same.epoch()));
    assert!(
        !other.matches(first.epoch()),
        "another actual connection cannot reuse confirmation"
    );
    let epoch = first.epoch();
    drop(a);
    assert!(
        !first.matches(epoch),
        "saved context cannot revive a disconnected connection"
    );
    assert!(!same.matches(epoch));
    assert!(
        !other.matches(epoch),
        "reconnecting never restores the old epoch"
    );
    assert!(TransportAuthority::default().connection().is_none());
}
use workbench_protocol::{
    ids::{ClientId, RunId},
    namespaces::lifecycle::AdminAction,
};
fn human(surface: Surface) -> Provenance {
    Provenance::human(surface, ClientId::new("claimed-human").unwrap())
}
#[test]
fn every_unauthenticated_surface_actor_and_saved_provenance_denies_every_action() {
    for surface in [Surface::Desktop, Surface::Cli, Surface::Web, Surface::None] {
        for _operation in AdminAction::ALL {
            let forged = human(surface);
            let restored: Provenance =
                serde_json::from_str(&serde_json::to_string(&forged).unwrap()).unwrap();
            assert!(IngressContext::unverified(restored).authorize().is_err());
            let (proof, _lease) = test_support::local();
            assert!(
                IngressContext::new(Provenance::agent(RunId::new("agent").unwrap()), proof)
                    .authorize()
                    .is_err()
            );
            assert!(IngressContext::system(forged).authorize().is_err());
        }
    }
}
#[test]
fn local_proof_cannot_be_reused_for_web_or_after_disconnect() {
    let (proof, lease) = test_support::local();
    for surface in [Surface::Desktop, Surface::Cli] {
        assert!(
            IngressContext::new(human(surface), proof.clone())
                .authorize()
                .is_ok()
        );
    }
    assert_eq!(
        IngressContext::new(human(Surface::Web), proof.clone()).authorize(),
        Err(AuthorityDenial::SurfaceMismatch)
    );
    drop(lease);
    assert_eq!(
        IngressContext::new(human(Surface::Desktop), proof).authorize(),
        Err(AuthorityDenial::Expired)
    );
}
#[tokio::test(start_paused = true)]
async fn web_lease_revocation_and_absolute_expiry_cannot_be_reconnected_by_session_id() {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    let (principal, lease) = test_support::web("bounded", deadline);
    let proof = TransportAuthority::from_web_context(Some(&principal));
    assert!(
        IngressContext::new(human(Surface::Web), proof.clone())
            .authorize()
            .is_ok()
    );
    lease.revoke();
    proof.expired().await;
    assert!(
        IngressContext::new(human(Surface::Web), proof)
            .authorize()
            .is_err()
    );
    assert!(
        IngressContext::new(
            human(Surface::Web),
            TransportAuthority::from_web_context(None)
        )
        .authorize()
        .is_err()
    );
    let (principal, _lease) = test_support::web("bounded", deadline);
    let proof = TransportAuthority::from_web_context(Some(&principal));
    tokio::time::advance(std::time::Duration::from_secs(60)).await;
    proof.expired().await;
    assert_eq!(
        IngressContext::new(human(Surface::Web), proof).authorize(),
        Err(AuthorityDenial::Expired)
    );
}
