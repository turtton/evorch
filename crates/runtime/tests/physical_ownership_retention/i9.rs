//! I-9 uses the same sealed physical bootstrap, with two members in one graph.
use super::fixture::{self, Fixture};
use runtime::{OwnedThreadCas, ownership::ThreadOwner};
use rusqlite::{Connection, params};
use std::collections::BTreeSet;
use storage::recovery::graph_control::GraphEffectKey;

fn effect(operation: &str) -> GraphEffectKey {
    GraphEffectKey {
        operation_id: operation.into(),
        attempt_nonce: format!("{operation}-attempt"),
    }
}
fn save(conn: &Connection, owner: &ThreadOwner) {
    conn.execute(
        "INSERT OR REPLACE INTO thread_owners(thread_id,state) VALUES(?1,?2)",
        params![owner.thread_id, serde_json::to_string(owner).unwrap()],
    )
    .unwrap();
}
fn assert_receipts_empty(conn: &Connection) {
    for table in [
        "disposal_cleanup_receipts",
        "disposal_cleanup_receipt_members",
    ] {
        assert_eq!(
            conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| {
                r.get::<_, u64>(0)
            })
            .unwrap(),
            0,
            "{table}: a rejected batch must not leave any receipt"
        );
    }
}
pub(super) fn check(f: &Fixture) {
    f.retain();
    let graph = f.cas("a");
    assert_eq!(graph.membership.len(), 2);
    let owners: Vec<_> = fixture::THREADS
        .into_iter()
        .flat_map(|t| f.owners(t))
        .collect();
    let conn = Connection::open(f.root.join("owners/owners.db")).unwrap();
    let original = fixture::THREADS.map(|t| f.owner.attach(t).unwrap());
    let before_main = f.tx();
    let absent = vec![
        owners[0].clone(),
        OwnedThreadCas {
            thread_id: "b".into(),
            owner_identity: None,
            owner_generation: None,
        },
    ];
    // Complete membership, strict ordering, identity, generation and absence
    // are all expectations, never wildcard authority.
    for bad in [
        owners[..1].to_vec(),
        owners.iter().rev().cloned().collect(),
        vec![owners[0].clone(), owners[0].clone()],
        absent.clone(),
        vec![
            owners[0].clone(),
            OwnedThreadCas {
                owner_identity: Some("foreign".into()),
                ..owners[1].clone()
            },
        ],
        vec![
            owners[0].clone(),
            OwnedThreadCas {
                owner_generation: Some(owners[1].owner_generation.unwrap() + 1),
                ..owners[1].clone()
            },
        ],
    ] {
        assert!(f.runtime.disposal_authority(graph.clone(), bad).is_err());
    }
    conn.execute("DELETE FROM thread_owners WHERE thread_id='b'", [])
        .unwrap();
    let absence_authority = f
        .runtime
        .disposal_authority(graph.clone(), absent.clone())
        .unwrap();
    save(&conn, &original[1]);
    assert!(
        f.runtime
            .release_owned_graph_eligibility(&absence_authority, effect("absence"))
            .is_err(),
        "an owner appearing after authority creation must be rechecked"
    );
    let authority = f
        .runtime
        .disposal_authority(graph.clone(), owners.clone())
        .unwrap();
    let mut changed = original[1].clone();
    changed.lease.generation += 1;
    save(&conn, &changed);
    assert!(
        f.runtime
            .release_owned_graph_eligibility(&authority, effect("mismatch"))
            .is_err()
    );
    assert_eq!(f.tx(), before_main);
    assert!(
        f.db()
            .recovery_graph_control(&graph.manifest.manifest_id)
            .unwrap()
            .is_none()
    );
    for id in ["absence", "mismatch"] {
        assert!(f.db().recovery_graph_effect(id).unwrap().is_none());
    }
    assert_receipts_empty(&conn);
    save(&conn, &original[1]);
    let revoked = f
        .runtime
        .release_owned_graph_eligibility(&authority, effect("release"))
        .unwrap();
    assert!(revoked.revoked);
    assert_eq!(
        f.db().recovery_graph_effect("release").unwrap(),
        Some(revoked)
    );

    // A late mismatch in member b must not clear member a's planned receipt.
    save(&conn, &changed);
    assert!(
        f.runtime
            .cleanup_owned_graph_receipts(&authority, effect("batch"), &BTreeSet::new())
            .is_err()
    );
    assert_receipts_empty(&conn);
    assert!(f.owner.attach("a").unwrap().planned_claim.is_some());
    assert_eq!(
        f.owner.attach("b").unwrap().lease.owner_id,
        changed.lease.owner_id
    );
    assert_eq!(
        f.owner.attach("b").unwrap().lease.generation,
        changed.lease.generation
    );
    save(&conn, &original[1]);
    // A write failure on the second member rolls back the first member too.
    conn.execute_batch(
        "CREATE TRIGGER fail_batch BEFORE UPDATE ON thread_owners
         WHEN OLD.thread_id='b' BEGIN SELECT RAISE(ABORT,'I-9 second member'); END;",
    )
    .unwrap();
    assert!(
        f.runtime
            .cleanup_owned_graph_receipts(&authority, effect("batch"), &BTreeSet::new())
            .is_err()
    );
    assert_receipts_empty(&conn);
    assert!(f.owner.attach("a").unwrap().planned_claim.is_some());
    assert!(f.owner.attach("b").unwrap().planned_claim.is_some());
    conn.execute_batch("DROP TRIGGER fail_batch").unwrap();

    let shared = BTreeSet::from(["b".into()]);
    let receipt = f
        .runtime
        .cleanup_owned_graph_receipts(&authority, effect("batch"), &shared)
        .unwrap();
    assert_eq!(receipt.cleaned_threads, ["a"]);
    assert_eq!(receipt.shared_threads, ["b"]);
    assert_eq!(receipt.owner_generations.len(), 2);
    assert!(f.owner.attach("a").unwrap().planned_claim.is_none());
    assert!(f.owner.attach("b").unwrap().planned_claim.is_some());
    assert_eq!(
        f.owner.disposal_cleanup_receipt("batch").unwrap(),
        Some(receipt.clone())
    );
    assert!(
        f.owner
            .disposal_cleanup_receipt("unknown")
            .unwrap()
            .is_none()
    );
    assert_eq!(
        f.runtime
            .cleanup_owned_graph_receipts(&authority, effect("batch"), &shared)
            .unwrap(),
        receipt
    );
    // Same operation with changed effect/scope must not masquerade as readback.
    let mut wrong = effect("batch");
    wrong.attempt_nonce = "different".into();
    assert!(
        f.runtime
            .cleanup_owned_graph_receipts(&authority, wrong, &shared)
            .is_err()
    );
    assert!(
        f.runtime
            .cleanup_owned_graph_receipts(&authority, effect("batch"), &BTreeSet::new())
            .is_err()
    );
    let finish = f
        .runtime
        .cleanup_owned_graph_receipts(&authority, effect("remaining"), &BTreeSet::new())
        .unwrap();
    assert_eq!(finish.cleaned_threads, ["a", "b"]);
    for (thread, old) in fixture::THREADS.into_iter().zip(&original) {
        let now = f.owner.attach(thread).unwrap();
        assert_eq!(now.state, runtime::ownership::OwnerState::Running);
        assert_eq!(now.lease.owner_id, old.lease.owner_id);
        assert_eq!(now.lease.generation, old.lease.generation);
        assert_eq!(now.active_runs, old.active_runs);
        assert!(now.planned_claim.is_none());
        // No start/claim/release is needed to continue ordinary ownership.
        f.owner.owned_permit(thread).unwrap().begin_turn().unwrap();
    }
    // Receipt readback is diagnostic, not a reason to touch a new generation.
    save(&conn, &changed);
    assert_eq!(
        f.owner.disposal_cleanup_receipt("batch").unwrap(),
        Some(receipt)
    );
    assert!(
        f.runtime
            .cleanup_owned_graph_receipts(&authority, effect("new-generation"), &BTreeSet::new())
            .is_err()
    );
    assert_eq!(
        f.owner.attach("b").unwrap().lease.owner_id,
        changed.lease.owner_id
    );
    assert_eq!(
        f.owner.attach("b").unwrap().lease.generation,
        changed.lease.generation
    );

    // Confirmed absence contributes no owner write or receipt member.
    conn.execute("DELETE FROM thread_owners WHERE thread_id='b'", [])
        .unwrap();
    let authority = f.runtime.disposal_authority(f.cas("a"), absent).unwrap();
    let receipt = f
        .runtime
        .cleanup_owned_graph_receipts(&authority, effect("absent-cleanup"), &BTreeSet::new())
        .unwrap();
    assert_eq!(receipt.cleaned_threads, ["a"]);
    assert_eq!(receipt.owner_generations.len(), 1);
    assert!(f.owner.attach("b").is_err());
}
