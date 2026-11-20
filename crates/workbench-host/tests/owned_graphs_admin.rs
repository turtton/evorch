use workbench_host::admin::*;
fn owner() -> RetainedOwner<'static> {
    RetainedOwner {
        retained_identity: "self",
        retained_generation: 7,
        current_identity: Some("self"),
        current_generation: Some(7),
    }
}
#[test]
fn undisposed_requires_all_retained_threads_and_includes_manual_and_over_limit() {
    for n in [1, 2, 64, 65] {
        let owners = vec![owner(); n];
        let g = GraphOwnership {
            verified_live_adoption: true,
            owners: &owners,
            resume_committed: false,
            disposal_complete: false,
            revocation: RevocationObservation::NotApplied,
        };
        assert!(is_undisposed(&g, "self")); // ManualPending/CleanupPending do not exclude.
        assert!(!is_undisposed(&g, "foreign"));
        assert!(!is_incomplete_disposal(&g, "self"));
        for changed in [
            GraphOwnership {
                verified_live_adoption: false,
                ..g.clone()
            },
            GraphOwnership {
                resume_committed: true,
                ..g.clone()
            },
            GraphOwnership {
                disposal_complete: true,
                ..g.clone()
            },
        ] {
            assert!(!is_undisposed(&changed, "self"));
        }
    }
    let mut owners = vec![owner(), owner()];
    owners[1].current_generation = Some(8);
    let mut g = GraphOwnership {
        verified_live_adoption: true,
        owners: &owners,
        resume_committed: false,
        disposal_complete: false,
        revocation: RevocationObservation::NotApplied,
    };
    assert!(!is_undisposed(&g, "self"));
    assert!(!is_incomplete_disposal(&g, "self"));
    g.revocation = RevocationObservation::Confirmed;
    assert!(!is_undisposed(&g, "self"));
    assert!(is_incomplete_disposal(&g, "self"));
    g.revocation = RevocationObservation::Unknown;
    assert!(is_undisposed(&g, "self"));
    assert!(!is_incomplete_disposal(&g, "self"));
    let mut foreign = owner();
    foreign.retained_identity = "foreign";
    let absent = RetainedOwner {
        current_identity: None,
        current_generation: None,
        ..owner()
    };
    for row in [foreign, absent] {
        let rows = [row];
        let g = GraphOwnership {
            owners: &rows,
            revocation: RevocationObservation::Confirmed,
            ..g.clone()
        };
        assert!(!is_undisposed(&g, "self"));
    }
    let empty = GraphOwnership { owners: &[], ..g };
    assert!(!is_undisposed(&empty, "self"));
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
