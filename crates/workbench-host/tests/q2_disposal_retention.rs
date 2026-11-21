//! I-6: both real recovery_authority claim callsites, not copied logic.
#[cfg(target_os = "linux")]
#[allow(dead_code)]
#[path = "../../runtime/shared-test-support/physical_ownership.rs"]
mod fixture;
#[cfg(target_os = "linux")]
use runtime::runtime::graph_observation::GraphObserver;
#[cfg(target_os = "linux")]
fn main() {
    fixture::run(
        &[
            "planned-owner-threads",
            "current-authority-resolver",
            "live-collector-effects",
            "live-collector-owner-generation",
            "live-collector-diagnostic-race",
            "diagnostic-64",
            "diagnostic-oversize",
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
    if case.starts_with("diagnostic-") {
        diagnostic_bounds(f, case);
        return;
    }
    for thread in fixture::THREADS {
        f.denied(thread);
    }
    match case {
        "planned-owner-threads"
        | "live-collector-effects"
        | "live-collector-owner-generation"
        | "live-collector-diagnostic-race" => {
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
        let pause = (case == "live-collector-diagnostic-race").then(|| {
            std::sync::Arc::new(PausedDiagnostic {
                collector: collector.clone(),
                entered: std::sync::Barrier::new(2),
                leave: std::sync::Barrier::new(2),
                paused: std::sync::atomic::AtomicBool::new(false),
            })
        });
        let observer: std::sync::Arc<dyn GraphObserver> = match &pause {
            Some(pause) => pause.clone(),
            None => collector.clone(),
        };
        f.runtime.install_graph_observer(observer).unwrap();
        (collector, pause)
    });
    f.retain();
    for thread in fixture::THREADS {
        f.allowed(thread);
    }
    if let Some((collector, pause)) = collector {
        live_collector(f, case, &collector, pause.as_deref());
    }
}

#[cfg(target_os = "linux")]
struct PausedDiagnostic {
    collector: std::sync::Arc<workbench_host::bootstrap::recovery_disposal::LiveGraphCollector>,
    entered: std::sync::Barrier,
    leave: std::sync::Barrier,
    paused: std::sync::atomic::AtomicBool,
}
#[cfg(target_os = "linux")]
impl GraphObserver for PausedDiagnostic {
    fn begin_change(&self, manifest: &str) -> Result<u64, runtime::RuntimeError> {
        self.collector.begin_change(manifest)
    }
    fn begin_automatic_change(&self, manifest: &str) -> Result<Option<u64>, runtime::RuntimeError> {
        self.collector.begin_automatic_change(manifest)
    }
    fn diagnostic_revision(&self, manifest: &str) -> Option<u64> {
        self.collector.diagnostic_revision(manifest)
    }
    fn publish_diagnostic(
        &self,
        expected: &storage::recovery::graph_control::GraphControlCas,
        revision: u64,
        diagnostic: &runtime::OwnedGraphOwnershipDiagnostic,
    ) -> Result<(), runtime::RuntimeError> {
        if !self.paused.swap(true, std::sync::atomic::Ordering::SeqCst) {
            self.entered.wait();
            self.leave.wait();
        }
        self.collector
            .publish_diagnostic(expected, revision, diagnostic)
    }
    fn cancel_unstarted(&self, manifest: &str, ticket: u64) -> Result<(), runtime::RuntimeError> {
        self.collector.cancel_unstarted(manifest, ticket)
    }
    fn readback(&self, manifest: &str, ticket: u64) -> Result<(), runtime::RuntimeError> {
        self.collector.readback(manifest, ticket)
    }
    fn rebuild(&self) -> Result<(), runtime::RuntimeError> {
        self.collector.rebuild()
    }
    fn mark_unavailable(&self) {
        self.collector.mark_unavailable()
    }
}
#[cfg(target_os = "linux")]
fn diagnostic_race(
    f: &fixture::Fixture,
    collector: &workbench_host::bootstrap::recovery_disposal::LiveGraphCollector,
    pause: &PausedDiagnostic,
) {
    struct Release<'a>(&'a std::sync::Barrier);
    impl Drop for Release<'_> {
        fn drop(&mut self) {
            self.0.wait();
        }
    }
    let cas = f.cas("a");
    std::thread::scope(|scope| {
        let runtime = f.runtime.clone();
        let worker = scope.spawn(move || runtime.owned_graph_ownership_diagnostic(&cas));
        pause.entered.wait();
        // Always release the worker even if the contention assertion fails.
        let release = Release(&pause.leave);
        let automatic = f.runtime.reserve_automatic_activation("graph-a");
        assert!(
            automatic.is_ok(),
            "a parked diagnostic must not turn an automatic retry into Contended"
        );
        let automatic: runtime::ActivationReservation = automatic.unwrap().into();
        assert!(
            f.runtime
                .reserved_owned_graph_ownership_diagnostic(&f.cas("b"), &automatic)
                .is_err(),
            "another manifest's reservation is not an internal readback scope"
        );
        assert_eq!(
            f.runtime
                .reserved_owned_graph_ownership_diagnostic(&f.cas("a"), &automatic)
                .unwrap()
                .state(),
            runtime::OwnedGraphOwnershipState::LiveSelf
        );
        automatic.confirm_graph_observation();
        drop(automatic);
        let before_owner_change = collector.snapshot().unwrap().snapshot_revision;
        let mut owner = f.owner.attach("a").unwrap();
        owner.lease.generation += 1;
        rusqlite::Connection::open(f.root.join("owners/owners.db"))
            .unwrap()
            .execute(
                "UPDATE thread_owners SET state=?1 WHERE thread_id='a'",
                [serde_json::to_string(&owner).unwrap()],
            )
            .unwrap();
        let current = f
            .runtime
            .owned_graph_ownership_diagnostic(&f.cas("a"))
            .unwrap();
        assert_eq!(
            current.state(),
            runtime::OwnedGraphOwnershipState::GenerationChanged
        );
        let confirmed = collector.snapshot().unwrap().snapshot_revision;
        assert!(confirmed > before_owner_change);
        drop(release);
        assert!(
            matches!(
                worker.join().unwrap(),
                Err(runtime::RuntimeError::RestartBusy)
            ),
            "a stale diagnostic must not return its old ownership response"
        );
        assert_eq!(
            collector.snapshot().unwrap().snapshot_revision,
            confirmed,
            "the old read must not publish facts over the newer observation"
        );
        f.runtime
            .owned_graph_ownership_diagnostic(&f.cas("a"))
            .unwrap();
        assert_eq!(collector.snapshot().unwrap().snapshot_revision, confirmed);
    });
}
#[cfg(target_os = "linux")]
fn diagnostic_bounds(f: &fixture::Fixture, case: &str) {
    use workbench_protocol::wire::{InstanceEpoch, Revision};
    workbench_host::bootstrap::recovery::authority::claim_planned_owner_threads(
        &f.runtime, &f.owner,
    )
    .unwrap();
    let collector = workbench_host::bootstrap::recovery_disposal::LiveGraphCollector::new(
        &f.runtime,
        f.config.clone(),
    );
    collector.bind_epoch(InstanceEpoch(2)).unwrap();
    f.runtime.install_graph_observer(collector.clone()).unwrap();
    f.retain();
    let cas = f.cas("a");
    let before = collector.snapshot().unwrap();
    let count = if case == "diagnostic-64" { 64 } else { 65 };
    for _ in 0..2 {
        let diagnostic = f.runtime.owned_graph_ownership_diagnostic(&cas).unwrap();
        assert_eq!(diagnostic.owner_count(), count);
        assert_eq!(diagnostic.retained_owner_count(), count);
        assert_eq!(diagnostic.references().len(), 64);
        assert_eq!(diagnostic.truncated(), count > 64);
        let snapshot = collector.snapshot().unwrap();
        assert_eq!(snapshot.count, Revision(1));
        assert_eq!(snapshot.over_limit_count, Revision(u64::from(count > 64)));
        assert_eq!(snapshot.snapshot_revision, before.snapshot_revision);
    }
    assert_eq!(
        f.cas("a"),
        cas,
        "diagnostics must not change durable graph facts"
    );
    assert!(f.runtime.list_agents().is_empty());
}
#[cfg(target_os = "linux")]
fn live_collector(
    f: &fixture::Fixture,
    case: &str,
    collector: &workbench_host::bootstrap::recovery_disposal::LiveGraphCollector,
    pause: Option<&PausedDiagnostic>,
) {
    use storage::recovery::graph_control::GraphEffectKey;
    use workbench_protocol::wire::{InstanceEpoch, Revision};
    let initial = collector.snapshot().unwrap();
    assert_eq!(
        initial.count,
        Revision(2),
        "seal builds the complete adopted inventory"
    );
    if let Some(pause) = pause {
        diagnostic_race(f, collector, pause);
        return;
    }
    for _ in 0..2 {
        f.runtime
            .owned_graph_ownership_diagnostic(&f.cas("a"))
            .unwrap();
        assert_eq!(
            collector.snapshot().unwrap().snapshot_revision,
            initial.snapshot_revision,
            "unchanged diagnostic reads must not invalidate admission"
        );
    }
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
