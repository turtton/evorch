use workbench_host::admin::{GraphOwnership, is_undisposed};
#[test]
fn only_verified_matching_live_owner_and_generation_count() {
    let g = GraphOwnership {
        verified_live_adoption: true,
        owner: "live",
        generation: 2,
        resume_committed: false,
        disposal_complete: false,
    };
    assert!(is_undisposed(&g, "live", 2));
    assert!(!is_undisposed(&g, "other", 2));
    assert!(!is_undisposed(&g, "live", 3));
    for bad in [
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
            ..g
        },
    ] {
        assert!(!is_undisposed(&bad, "live", 2));
    }
}
