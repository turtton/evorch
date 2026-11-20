//! I-6: both real recovery_authority claim callsites, not copied logic.
#[cfg(target_os = "linux")]
#[allow(dead_code)]
#[path = "../../runtime/shared-test-support/physical_ownership.rs"]
mod fixture;
#[cfg(target_os = "linux")]
fn main() {
    fixture::run(
        &["planned-owner-threads", "current-authority-resolver"],
        check,
    );
}
#[cfg(not(target_os = "linux"))]
fn main() {
    println!("Q2 disposal retention: Linux-only");
}
#[cfg(target_os = "linux")]
fn check(f: &fixture::Fixture, case: &str) {
    use std::collections::{BTreeMap, BTreeSet};
    use workbench_host::{
        bootstrap::recovery::authority::{CurrentAuthorityProvider, claim_planned_owner_threads},
        lane::{BindingTable, ThreadBinding},
    };
    for thread in fixture::THREADS {
        f.denied(thread);
    }
    match case {
        "planned-owner-threads" => claim_planned_owner_threads(&f.runtime, &f.owner).unwrap(),
        "current-authority-resolver" => {
            let bindings = BindingTable::default();
            bindings.replace(BTreeMap::from_iter(fixture::THREADS.map(|thread| {
                (
                    thread.into(),
                    ThreadBinding {
                        root: fixture::run_id(thread),
                        project_root: Some(f.root.join("project")),
                        goal_project: None,
                        model_preference: None,
                    },
                )
            })));
            let records = fixture::THREADS.map(|thread| fixture::recovery_record(&f.root, thread));
            let resume = BTreeSet::from_iter(records.iter().map(|r| r.run_id));
            let writer = f.storage.handle();
            let provider = CurrentAuthorityProvider {
                runtime: &f.runtime,
                owner: &f.owner,
                bindings: &bindings,
                storage: &f.config,
                writer: &writer,
                config_dir: &f.root.join("user-config"),
            };
            let authority = provider.resolve(&records, &resume).unwrap();
            assert_eq!(authority.members.len(), 2);
        }
        _ => panic!("unknown host I-6 case {case}"),
    }
    for thread in fixture::THREADS {
        let actual = f.owner.attach(thread).unwrap();
        assert_eq!(
            actual.lease.owner_id,
            f.owner.owner_id(),
            "real claim did not execute"
        );
        assert!(actual.handoff_hold.is_none());
        assert!(actual.planned_claim.is_some());
        f.denied(thread);
    }
    f.retain();
    for thread in fixture::THREADS {
        f.allowed(thread);
    }
}
