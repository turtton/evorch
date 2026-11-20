//! I-6: every retaining/non-retaining path uses real physical bootstrap.
#[cfg(target_os = "linux")]
#[path = "../../shared-test-support/physical_ownership.rs"]
mod fixture;
#[cfg(target_os = "linux")]
fn main() {
    fixture::run(
        &[
            "sealed-positive",
            "forged-evidence",
            "db-hold",
            "owned-permit",
            "ordinary-start-claim",
            "interrupted-pass",
            "completed",
        ],
        check,
    );
}
#[cfg(not(target_os = "linux"))]
fn main() {
    println!("physical ownership retention: Linux-only");
}
#[cfg(target_os = "linux")]
fn check(f: &fixture::Fixture, case: &str) {
    use runtime::ownership::*;
    use storage::recovery::restart::*;
    struct Forged;
    impl AdoptedOwnershipEvidence for Forged {
        fn validate_hold(&self, _: &PlannedHold) -> Result<(), OwnershipError> {
            Ok(())
        }
    }
    match case {
        "sealed-positive" => {
            let mut first = 0;
            let evidence = f.runtime.adopted_restart_ownership_evidence().unwrap();
            let report = f
                .runtime
                .claim_physical_bootstrap_owners(&f.owner, evidence, || {
                    first += 1;
                    // A real first claim exists, but the pass is still provisional.
                    f.denied("a");
                })
                .unwrap();
            assert_eq!(first, 1);
            assert!(report.blocked_threads.is_empty());
            f.allowed("a");
            f.allowed("b");
            assert!(
                f.runtime
                    .claim_physical_bootstrap_owners(
                        &f.owner,
                        f.runtime.adopted_restart_ownership_evidence().unwrap(),
                        || panic!("second pass")
                    )
                    .is_err()
            );
        }
        "forged-evidence" => {
            f.owner.claim_planned(&f.hold("a"), &Forged).unwrap();
            f.denied("a");
            f.denied("b");
            f.retain();
            f.allowed("a");
            f.allowed("b");
        }
        "db-hold" => {
            let hold = f.hold("a");
            assert_eq!(hold.transaction_id, fixture::TX);
            assert_eq!(
                hold.active_runs,
                std::collections::BTreeSet::from([fixture::run_id("a").to_string()])
            );
            assert_eq!(f.tx().owner_progress.len(), 2);
            f.denied("a");
            f.denied("b");
            f.retain();
            f.allowed("a");
            f.allowed("b");
        }
        "owned-permit" => {
            let evidence = f.runtime.adopted_restart_ownership_evidence().unwrap();
            let claimed = f.owner.claim_planned(&f.hold("a"), &evidence).unwrap();
            assert_eq!(f.owner.owned_permit("a").unwrap().lease, claimed.lease);
            f.denied("a");
            f.denied("b");
            f.retain();
            f.allowed("a");
            f.allowed("b");
        }
        "ordinary-start-claim" => {
            let started = f.owner.start("ordinary").unwrap();
            f.denied("a");
            f.denied("b");
            let released = f
                .registry()
                .update("ordinary", |owner| {
                    owner.quiesce(&started.lease)?;
                    owner.release(&started.lease)
                })
                .unwrap();
            let claimed = f.owner.claim(&released).unwrap();
            assert_eq!(claimed.lease.generation, started.lease.generation + 1);
            f.denied("a");
            f.denied("b");
            f.retain();
            f.allowed("a");
            f.allowed("b");
        }
        "interrupted-pass" => {
            let mut first = 0;
            let result = f.runtime.claim_physical_bootstrap_owners(&f.owner,
                f.runtime.adopted_restart_ownership_evidence().unwrap(), || {
                first += 1;
                assert_eq!(f.owner.attach("a").unwrap().lease.owner_id, f.owner.owner_id());
                f.denied("a");
                rusqlite::Connection::open(f.root.join("owners/owners.db")).unwrap().execute_batch(
                    "CREATE TRIGGER i6_abort_second BEFORE UPDATE ON thread_owners WHEN OLD.thread_id='b' BEGIN SELECT RAISE(ABORT,'I-6 interrupted pass'); END;"
                ).unwrap();
            });
            assert_eq!(first, 1);
            assert!(result.is_err());
            f.denied("a");
            f.denied("b");
            rusqlite::Connection::open(f.root.join("owners/owners.db"))
                .unwrap()
                .execute_batch("DROP TRIGGER i6_abort_second")
                .unwrap();
            assert!(
                f.runtime
                    .claim_physical_bootstrap_owners(
                        &f.owner,
                        f.runtime.adopted_restart_ownership_evidence().unwrap(),
                        || panic!("retry")
                    )
                    .is_err()
            );
            f.denied("a");
            f.denied("b");
        }
        "completed" => {
            f.retain();
            f.allowed("a");
            f.allowed("b");
            f.step(RestartMutation::Ready { at_ms: 0 });
            for thread in fixture::THREADS {
                let component = f.cas(thread).component;
                f.step(RestartMutation::Dispose {
                    manifest_id: component.manifest_id,
                    expected_revision: component.revision,
                    disposition: RestartDisposition::Quarantined,
                    claim_id: None,
                    reason: Some("I-6 cleanup only".into()),
                });
            }
            assert_eq!(f.tx().phase, RestartPhase::Completed);
            assert!(
                f.runtime.adopted_restart_ownership_evidence().is_err(),
                "Completed must reject fresh evidence"
            );
            f.allowed("a");
            f.allowed("b");
        }
        _ => panic!("unknown I-6 case {case}"),
    }
}
