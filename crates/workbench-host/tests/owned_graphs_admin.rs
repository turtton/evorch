use workbench_host::admin::*;
fn facts(thread_count: usize) -> GraphDisposalFacts {
    GraphDisposalFacts {
        verified_live_adoption: true,
        thread_count,
        resume_committed: false,
        disposal_complete: false,
        revocation: RevocationObservation::NotApplied,
    }
}
#[test]
fn undisposed_covers_every_adopted_thread_and_includes_manual_and_over_limit() {
    for n in [1, 2, 64, 65] {
        let g = facts(n);
        assert!(g.is_undisposed()); // ManualPending/CleanupPending do not exclude.
        assert!(!g.is_incomplete_disposal());
        for changed in [
            GraphDisposalFacts {
                verified_live_adoption: false,
                ..g.clone()
            },
            GraphDisposalFacts {
                resume_committed: true,
                ..g.clone()
            },
            GraphDisposalFacts {
                disposal_complete: true,
                ..g.clone()
            },
        ] {
            assert!(!changed.is_undisposed());
        }
    }
    let mut g = facts(2);
    // An unknown revocation outcome still blocks: it may not have been applied.
    g.revocation = RevocationObservation::Unknown;
    assert!(g.is_undisposed());
    assert!(!g.is_incomplete_disposal());
    // A confirmed revocation leaves only physical cleanup: a warning, not a blocker.
    g.revocation = RevocationObservation::Confirmed;
    assert!(!g.is_undisposed());
    assert!(g.is_incomplete_disposal());
    g.disposal_complete = true;
    assert!(!g.is_undisposed());
    assert!(!g.is_incomplete_disposal());
    // A graph without a thread is never listed.
    let empty = facts(0);
    assert!(!empty.is_undisposed());
    assert!(
        !GraphDisposalFacts {
            revocation: RevocationObservation::Confirmed,
            ..empty
        }
        .is_incomplete_disposal()
    );
}
#[test]
fn opaque_graph_and_thread_refs_are_stable_domain_separated_and_path_free() {
    let mut refs = GraphReferences::default();
    let g = refs
        .graph_reference("tx", "cap", "/secret/manifest.json")
        .unwrap();
    assert_eq!(
        g,
        refs.graph_reference("tx", "cap", "/secret/manifest.json")
            .unwrap()
    );
    assert_ne!(
        g,
        refs.graph_reference("tx2", "cap", "/secret/manifest.json")
            .unwrap()
    );
    assert_ne!(
        g,
        refs.graph_reference("tx", "cap2", "/secret/manifest.json")
            .unwrap()
    );
    assert_ne!(
        g,
        refs.graph_reference("tx", "cap", "/secret/other.json")
            .unwrap()
    );
    let t = refs
        .thread_reference("tx", "cap", "/secret/manifest.json", "/raw/thread")
        .unwrap();
    assert_ne!(g, t);
    assert_ne!(
        t,
        refs.thread_reference("tx", "cap", "/secret/manifest.json", "/raw/thread2")
            .unwrap()
    );
    assert!(!serde_json::to_string(&[g, t]).unwrap().contains("secret"));
    assert_ne!(
        refs.graph_reference("ab", "c", "d").unwrap(),
        refs.graph_reference("a", "bc", "d").unwrap()
    );
}
