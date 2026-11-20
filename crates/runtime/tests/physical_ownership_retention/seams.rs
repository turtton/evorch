//! Central seam checks, all through the same real physical bootstrap fixture.
use super::fixture::{self, Fixture};
use runtime::{OwnedGraphOwnershipState, ownership::ThreadOwner};
use rusqlite::{Connection, params};
use std::collections::BTreeSet;
use storage::recovery::graph_control::GraphEffectKey;

fn save(conn: &Connection, owner: &ThreadOwner) {
    conn.execute(
        "UPDATE thread_owners SET state=?2 WHERE thread_id=?1",
        params![owner.thread_id, serde_json::to_string(owner).unwrap()],
    )
    .unwrap();
}
fn identity(owner: &ThreadOwner) -> (&str, u64, runtime::ownership::OwnerState) {
    (&owner.lease.owner_id, owner.lease.generation, owner.state)
}

pub(super) fn diagnostic(f: &Fixture) {
    let graph = f.cas("a");
    assert_eq!(graph.membership.len(), 65);
    let cold = f.runtime.owned_graph_ownership_diagnostic(&graph).unwrap();
    assert_eq!(cold.state(), OwnedGraphOwnershipState::Cold);
    assert!(!cold.verified_adoption());
    f.retain();
    let before = f.tx();
    let view = f.runtime.owned_graph_ownership_diagnostic(&graph).unwrap();
    assert_eq!(view.state(), OwnedGraphOwnershipState::LiveSelf);
    assert!(view.verified_adoption());
    assert_eq!(view.retained_owner_count(), 65);
    assert_eq!(view.owner_count(), 65);
    assert_eq!(view.references().len(), 64);
    assert!(view.truncated());
    assert!(
        view.references()
            .iter()
            .all(|r| r.len() == 64 && r.bytes().all(|b| b.is_ascii_hexdigit()))
    );
    let owners = graph
        .membership
        .iter()
        .flat_map(|m| f.owners(m.thread_id.as_deref().unwrap()))
        .collect::<Vec<_>>();
    assert!(
        f.runtime
            .disposal_authority(graph.clone(), owners.clone())
            .is_err()
    );
    assert!(
        f.runtime
            .disposal_authority(graph.clone(), owners[..64].to_vec())
            .is_err()
    );
    // The 65th member is outside the displayed prefix, but never outside CAS.
    let conn = Connection::open(f.root.join("owners/owners.db")).unwrap();
    let last = f.owner.attach("t062").unwrap();
    let mut changed = last.clone();
    changed.lease.generation += 1;
    save(&conn, &changed);
    let mismatch = f.runtime.owned_graph_ownership_diagnostic(&graph).unwrap();
    assert_eq!(
        mismatch.state(),
        OwnedGraphOwnershipState::GenerationChanged
    );
    assert_eq!(mismatch.references(), view.references());
    save(&conn, &last);
    assert_eq!(f.tx(), before);
    assert!(f.db().recovery_graph_control("graph-a").unwrap().is_none());
    assert!(f.runtime.list_agents().is_empty());
    assert!(!f.runtime.has_paused_recovery());
}

pub(super) fn terminal(f: &Fixture, case: &str) {
    f.retain();
    let phase = case.strip_prefix("terminal-").unwrap();
    Connection::open(&f.config.db_path)
        .unwrap()
        .execute(
            "UPDATE restart_transactions SET phase=?1 WHERE restart_id=?2",
            params![phase, fixture::TX],
        )
        .unwrap();
    let graph = f.cas("a");
    let before_component = graph.component.clone();
    let conn = Connection::open(f.root.join("owners/owners.db")).unwrap();
    let original = f.owner.attach("a").unwrap();
    let sibling = f.owner.attach("b").unwrap();
    let owners = f.owners("a");
    let mut changed = original.clone();
    changed.lease.generation += 1;
    save(&conn, &changed);
    assert!(
        f.runtime
            .disposal_authority(graph.clone(), owners.clone())
            .is_err()
    );
    assert!(f.db().recovery_graph_control("graph-a").unwrap().is_none());
    save(&conn, &original);
    let authority = f.runtime.disposal_authority(graph, owners).unwrap();
    let effect = GraphEffectKey {
        operation_id: case.into(),
        attempt_nonce: "terminal-attempt".into(),
    };
    let marker = f
        .runtime
        .release_owned_graph_eligibility(&authority, effect.clone())
        .unwrap();
    assert!(marker.revoked);
    assert_eq!(marker.origin, "manual_release_without_claim");
    assert_eq!(f.cas("a").component, before_component);
    assert!(f.db().recovery_claims().unwrap().is_empty());
    // Revocation is not rolled back when a later owner CAS fails.
    save(&conn, &changed);
    assert!(
        f.runtime
            .cleanup_owned_graph_receipts(&authority, effect.clone(), &BTreeSet::new())
            .is_err()
    );
    assert!(f.owner.disposal_cleanup_receipt(case).unwrap().is_none());
    assert!(
        f.db()
            .recovery_graph_control("graph-a")
            .unwrap()
            .unwrap()
            .revoked
    );
    save(&conn, &original);
    let receipt = f
        .runtime
        .cleanup_owned_graph_receipts(&authority, effect.clone(), &BTreeSet::new())
        .unwrap();
    assert_eq!(receipt.cleaned_threads, ["a"]);
    assert_eq!(
        f.owner.disposal_cleanup_receipt(case).unwrap(),
        Some(receipt)
    );
    let current = f.owner.attach("a").unwrap();
    assert_eq!(identity(&current), identity(&original));
    assert!(current.planned_claim.is_none());
    assert_eq!(identity(&f.owner.attach("b").unwrap()), identity(&sibling));
    assert!(f.owner.attach("b").unwrap().planned_claim.is_some());
    assert!(f.runtime.list_agents().is_empty());
    assert!(!f.runtime.has_paused_recovery());
    f.storage
        .handle()
        .finish_recovery_graph_control(marker)
        .unwrap();
}
