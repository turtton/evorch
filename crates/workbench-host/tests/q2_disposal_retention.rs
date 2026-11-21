//! I-6: both real recovery_authority claim callsites, not copied logic.
#[cfg(target_os = "linux")]
#[allow(dead_code)]
#[path = "../../runtime/shared-test-support/physical_ownership.rs"]
mod fixture;
#[cfg(target_os = "linux")]
fn main() {
    fixture::run(
        &[
            "planned-owner-threads",
            "current-authority-resolver",
            "live-collector-effects",
            "live-collector-owner-generation",
        ],
        check,
    );
}
#[cfg(not(target_os = "linux"))]
fn main() {
    // No cases on platforms without validated physical bootstrap support.
    // An empty --list response is a valid custom-harness test listing.
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
        "planned-owner-threads" | "live-collector-effects" | "live-collector-owner-generation" => {
            claim_planned_owner_threads(&f.runtime, &f.owner).unwrap()
        }
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
    let collector = case.starts_with("live-collector-").then(|| {
        let collector = workbench_host::bootstrap::recovery_disposal::LiveGraphCollector::new(
            &f.runtime,
            f.config.clone(),
        );
        collector
            .bind_epoch(workbench_protocol::wire::InstanceEpoch(2))
            .unwrap();
        assert!(
            collector.snapshot().is_err(),
            "before physical seal is not empty success"
        );
        f.runtime.install_graph_observer(collector.clone()).unwrap();
        collector
    });
    f.retain();
    for thread in fixture::THREADS {
        f.allowed(thread);
    }
    if let Some(collector) = collector {
        live_collector(f, case, &collector);
    }
}

#[cfg(target_os = "linux")]
fn live_collector(
    f: &fixture::Fixture,
    case: &str,
    collector: &workbench_host::bootstrap::recovery_disposal::LiveGraphCollector,
) {
    use storage::recovery::graph_control::GraphEffectKey;
    use workbench_protocol::wire::{InstanceEpoch, Revision};
    let initial = collector.snapshot().unwrap();
    assert_eq!(
        initial.count,
        Revision(2),
        "seal builds the complete adopted inventory"
    );
    let conn = rusqlite::Connection::open(&f.config.db_path).unwrap();
    let roots = || {
        conn.prepare("SELECT thread_id,root_run_id FROM threads ORDER BY thread_id")
            .unwrap()
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    let before = roots();
    if case == "live-collector-owner-generation" {
        // Main revocation is confirmed but cleanup is incomplete: still a
        // blocker while this process retains every current owner.
        let cas = f.cas("a");
        let effect = GraphEffectKey {
            operation_id: "partial".into(),
            attempt_nonce: "partial".into(),
        };
        let token = f
            .runtime
            .reserve_owned_graph_disposal("graph-a", effect.clone(), cas.control_revision)
            .unwrap();
        let authority = f.runtime.disposal_authority(cas, f.owners("a")).unwrap();
        let result = f
            .runtime
            .dispose_owned_graph(&authority, effect, &token)
            .unwrap();
        assert!(!result.unknown);
        assert!(!result.complete());
        token.confirm_graph_observation();
        drop(token);
        assert_eq!(collector.snapshot().unwrap().count, Revision(2));
        let mut owner = f.owner.attach("a").unwrap();
        owner.lease.generation += 1;
        rusqlite::Connection::open(f.root.join("owners/owners.db"))
            .unwrap()
            .execute(
                "UPDATE thread_owners SET state=?1 WHERE thread_id='a'",
                [serde_json::to_string(&owner).unwrap()],
            )
            .unwrap();
        f.runtime
            .owned_graph_ownership_diagnostic(&f.cas("a"))
            .unwrap();
        let after = collector.snapshot().unwrap();
        assert!(after.snapshot_revision > initial.snapshot_revision);
        assert_eq!(after.count, Revision(1));
        assert_eq!(
            after.incomplete_disposals.count,
            Revision(1),
            "lost ownership after confirmed revocation is a separate warning"
        );
    } else {
        use sha2::Digest;
        use std::os::unix::ffi::OsStrExt;
        let scratch = f.owner.thread_scratch();
        f.runtime.set_thread_scratch(scratch.clone());
        let canonical = f.root.join("owners").canonicalize().unwrap();
        let digest = format!(
            "{:x}",
            sha2::Sha256::digest(canonical.as_os_str().as_bytes())
        );
        let disk = sandbox::ScratchRoot::fixed(&digest[..8], &digest).unwrap();
        for thread in fixture::THREADS {
            let dir = scratch.thread_dir(thread);
            disk.prepare(dir.file_name().unwrap().to_str().unwrap())
                .unwrap();
            std::fs::write(dir.join("before"), "old scratch").unwrap();
        }
        // Real main revocation/owner cleanup with a lost progress ACK. Query
        // cannot settle it; only this original supervisor can read it back.
        conn.execute_batch("CREATE TRIGGER fail_live_progress BEFORE INSERT ON recovery_graph_thread_progress BEGIN SELECT RAISE(FAIL,'lost progress ACK'); END;").unwrap();
        for (index, thread) in fixture::THREADS.iter().enumerate() {
            let cas = f.cas(thread);
            let effect = GraphEffectKey {
                operation_id: format!("live-{thread}"),
                attempt_nonce: format!("nonce-{thread}"),
            };
            let token = f
                .runtime
                .reserve_owned_graph_disposal(
                    &cas.manifest.manifest_id,
                    effect.clone(),
                    cas.control_revision,
                )
                .unwrap();
            assert!(
                collector.snapshot().is_err(),
                "fence is synchronous before every effect"
            );
            assert!(
                collector
                    .admit(
                        InstanceEpoch(2),
                        initial.snapshot_revision,
                        &mut || -> Result<(), workbench_host::admin::AdminFailure> {
                            panic!("pending cannot cross the restart gate")
                        },
                        |_| std::sync::Arc::new(|| false)
                    )
                    .is_err()
            );
            let authority = f.runtime.disposal_authority(cas, f.owners(thread)).unwrap();
            let result = f
                .runtime
                .dispose_owned_graph(&authority, effect.clone(), &token)
                .unwrap();
            assert!(
                collector.snapshot().is_err(),
                "FULL result alone is not supervisor publication"
            );
            if index == 0 {
                assert!(result.unknown);
                conn.execute_batch("DROP TRIGGER fail_live_progress")
                    .unwrap();
                assert!(
                    f.runtime
                        .reconcile_owned_graph_disposal(&effect, &token)
                        .unwrap()
                );
            } else {
                assert!(!result.unknown);
                assert!(result.complete());
            }
            token.confirm_graph_observation();
            drop(token);
            if index == 0 {
                let partial = collector.snapshot().unwrap();
                assert_eq!(
                    partial.count,
                    Revision(2),
                    "main revocation alone cannot remove the blocker"
                );
                assert!(
                    collector
                        .admit(
                            InstanceEpoch(2),
                            partial.snapshot_revision,
                            &mut || Ok(()),
                            |_| std::sync::Arc::new(|| false)
                        )
                        .is_err()
                );
                // A new, explicitly requested cleanup can finish missing durable
                // progress, but readback itself never repeats scratch reset.
                let dir = scratch.thread_dir(thread);
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(dir.join("after"), "new scratch").unwrap();
                let cas = f.cas(thread);
                let next = GraphEffectKey {
                    operation_id: "cleanup".into(),
                    attempt_nonce: "cleanup".into(),
                };
                let token = f
                    .runtime
                    .reserve_owned_graph_disposal(
                        &cas.manifest.manifest_id,
                        next.clone(),
                        cas.control_revision,
                    )
                    .unwrap();
                let authority = f.runtime.disposal_authority(cas, f.owners(thread)).unwrap();
                assert!(
                    f.runtime
                        .dispose_owned_graph(&authority, next, &token)
                        .unwrap()
                        .complete()
                );
                token.confirm_graph_observation();
                drop(token);
                assert_eq!(std::fs::read(dir.join("after")).unwrap(), b"new scratch");
            }
            let after = collector.snapshot().unwrap();
            assert_eq!(after.count, Revision(1 - index as u64));
            assert!(after.snapshot_revision > initial.snapshot_revision);
            assert_eq!(after.incomplete_disposals.count, Revision(0));
        }
        let after = collector.snapshot().unwrap();
        collector
            .admit(
                InstanceEpoch(2),
                after.snapshot_revision,
                &mut || Ok(()),
                |_| std::sync::Arc::new(|| false),
            )
            .unwrap();
        assert!(
            f.runtime.reserve_automatic_activation("graph-a").is_err(),
            "restart reservation excludes automatic effects too"
        );
        assert!(
            f.runtime
                .reserve_owned_graph_disposal(
                    "graph-a",
                    GraphEffectKey {
                        operation_id: "reserved".into(),
                        attempt_nonce: "reserved".into()
                    },
                    0
                )
                .is_err()
        );
    }
    assert_eq!(
        roots(),
        before,
        "no root binding publication by observation or Release"
    );
    assert!(
        f.runtime.list_agents().is_empty(),
        "no execution was registered"
    );
}
