//! Owned-graph management over a real adopted graph: the live collector, the
//! real disposal and the manual resume policy under the writer leases and the
//! thread binding. No process ownership exists to claim.
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
            "live-collector-effects",
            "live-collector-manual-resume",
            "cold-detail",
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

/// The one collector of this process, observing the runtime, bound to `epoch`.
#[cfg(target_os = "linux")]
fn collector(
    f: &fixture::Fixture,
    epoch: u64,
) -> std::sync::Arc<workbench_host::bootstrap::recovery_disposal::LiveGraphCollector> {
    let collector =
        workbench_host::bootstrap::recovery_disposal::LiveGraphCollector::new(f.config.clone());
    collector
        .bind_epoch(workbench_protocol::wire::InstanceEpoch(epoch))
        .unwrap();
    assert!(
        collector.snapshot().is_err(),
        "before the seal is not empty success"
    );
    f.runtime.install_graph_observer(collector.clone()).unwrap();
    f.runtime.rebuild_graph_observation().unwrap();
    collector
}

#[cfg(target_os = "linux")]
fn check(f: &fixture::Fixture, case: &str) {
    match case {
        "cold-detail" => cold_detail(f),
        "diagnostic-64" | "diagnostic-oversize" => thread_bounds(f, case),
        "live-collector-effects" | "live-collector-manual-resume" => {
            // The bindings that make disposal legitimate are current.
            for thread in fixture::THREADS {
                assert!(f.authority(thread), "{thread}: current binding");
            }
            let collector = collector(f, 2);
            if case == "live-collector-manual-resume" {
                manual_resume(f, &collector);
            } else {
                live_collector(f, &collector);
            }
        }
        _ => panic!("unknown host case {case}"),
    }
}

#[cfg(target_os = "linux")]
fn manual_resume(
    f: &fixture::Fixture,
    collector: &workbench_host::bootstrap::recovery_disposal::LiveGraphCollector,
) {
    use runtime::ActivationReservation;
    use storage::recovery::{
        RecoveryClaimRequest, RecoveryClaimState, graph_control::GraphEffectKey,
    };
    use workbench_protocol::wire::Revision;
    // Leave the same existing Prepared claim that an automatic attempt can
    // hand to manual Resume. The original attempt owns its observation fence.
    let automatic = f.runtime.reserve_automatic_activation("graph-a").unwrap();
    let claim = f
        .storage
        .handle()
        .claim_recovery_manifest(RecoveryClaimRequest {
            claim_id: "resume-prepared".into(),
            manifest_id: "graph-a".into(),
            incarnations: vec![(fixture::run_id("a").to_string(), "manual-new".into())],
        })
        .unwrap();
    automatic.confirm_graph_observation();
    drop(automatic);
    let cas = f.cas("a");
    assert_eq!(cas.claim.as_ref(), Some(&claim));
    let effect = GraphEffectKey {
        operation_id: "manual-resume".into(),
        attempt_nonce: "manual-resume-nonce".into(),
    };
    let reservation: ActivationReservation = f
        .runtime
        .reserve_manual_activation("graph-a", effect.clone(), cas.control_revision)
        .unwrap()
        .into();
    assert!(
        collector.snapshot().is_err(),
        "internal reads cannot settle pending"
    );
    // Exercise the real Resume policy writer, not only a read-only helper.
    let control = f
        .runtime
        .begin_owned_graph_resume(&cas, &reservation)
        .unwrap();
    assert_eq!(control.mode, "manual_running");
    assert!(!control.revoked);
    assert_eq!(control.revision, cas.control_revision + 1);
    assert_eq!(
        f.db().recovery_graph_effect(&effect.operation_id).unwrap(),
        Some(control.clone())
    );
    assert_eq!(
        f.cas("a").claim.unwrap().state,
        RecoveryClaimState::Prepared
    );
    assert!(collector.snapshot().is_err());
    f.storage
        .handle()
        .finish_recovery_graph_control(control)
        .unwrap();
    reservation.confirm_graph_observation();
    drop(reservation);
    assert_eq!(collector.snapshot().unwrap().count, Revision(2));
}

/// A core that did not adopt the graph (a later epoch) sees it as cold: it is
/// shown, never acted on, and never listed as a restart blocker.
#[cfg(target_os = "linux")]
fn cold_detail(f: &fixture::Fixture) {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use workbench_host::bootstrap::recovery_disposal::detail;
    use workbench_protocol::{
        namespaces::lifecycle::owned_graphs::{ActionAvailability, GraphEligibilityView},
        wire::{InstanceEpoch, Revision},
    };
    let collector = collector(f, 3);
    let before = collector.snapshot().unwrap();
    assert_eq!(before.count, Revision(0));
    let graph = f.db().owned_graph_snapshots().unwrap().remove(0);
    assert_eq!(graph.adopted_epoch, Some(graph.cas.successor_epoch));
    for _ in 0..2 {
        let view = detail(&graph, collector.epoch()).unwrap();
        assert_eq!(view.eligibility, GraphEligibilityView::ColdIsolated);
        assert!(matches!(view.resume, ActionAvailability::Blocked { .. }));
        assert!(matches!(view.release, ActionAvailability::Blocked { .. }));
        assert_eq!(collector.snapshot().unwrap(), before);
        assert!(
            collector
                .begin_change(&graph.cas.manifest.manifest_id)
                .is_err()
        );
    }
    let returned = Arc::new(AtomicBool::new(false));
    let predicate = returned.clone();
    collector
        .admit(
            InstanceEpoch(3),
            before.snapshot_revision,
            &mut || Ok(()),
            move |_| Arc::new(move || predicate.load(Ordering::Acquire)),
        )
        .unwrap();
    returned.store(true, Ordering::Release);
    assert_eq!(collector.snapshot().unwrap(), before);
    // A failed read is not "untracked success": it closes observation.
    collector.invalidate();
    assert!(collector.snapshot().is_err());
}

/// The thread inventory of one graph is complete, but only 64 are shown and
/// more than 64 is over the limit: still a blocker, never offered for effects.
#[cfg(target_os = "linux")]
fn thread_bounds(f: &fixture::Fixture, case: &str) {
    use workbench_host::bootstrap::recovery_disposal::detail;
    use workbench_protocol::{
        namespaces::lifecycle::owned_graphs::{ActionAvailability, OperationLimitView},
        wire::Revision,
    };
    let collector = collector(f, 2);
    let before = collector.snapshot().unwrap();
    let count: u64 = if case == "diagnostic-64" { 64 } else { 65 };
    let graph = f
        .db()
        .owned_graph_snapshots()
        .unwrap()
        .into_iter()
        .find(|g| g.cas.manifest.manifest_id == "graph-a")
        .unwrap();
    for _ in 0..2 {
        let view = detail(&graph, collector.epoch()).unwrap();
        assert_eq!(view.thread_count, Revision(count));
        assert_eq!(view.thread_refs.len(), 64);
        assert_eq!(view.thread_refs_truncated, count > 64);
        if count > 64 {
            assert_eq!(
                view.operation_limit,
                OperationLimitView::TooManyThreads { limit: 64 }
            );
            assert!(matches!(view.release, ActionAvailability::Blocked { .. }));
            assert!(matches!(view.resume, ActionAvailability::Blocked { .. }));
        } else {
            assert_eq!(view.operation_limit, OperationLimitView::WithinLimit);
        }
        let snapshot = collector.snapshot().unwrap();
        assert_eq!(snapshot.count, Revision(1));
        assert_eq!(snapshot.over_limit_count, Revision(u64::from(count > 64)));
        assert_eq!(snapshot.snapshot_revision, before.snapshot_revision);
    }
    assert_eq!(
        f.cas("a"),
        graph.cas,
        "observation must not change durable graph facts"
    );
    assert!(f.runtime.list_agents().is_empty());
}

#[cfg(target_os = "linux")]
fn live_collector(
    f: &fixture::Fixture,
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
    for thread in fixture::THREADS {
        let dir = f.scratch.thread_dir(thread);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("before"), "old scratch").unwrap();
    }
    // Real main revocation and scratch cleanup with a lost progress ACK. Query
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
        let authority = f.runtime.disposal_authority(cas).unwrap();
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
            // The durable revocation is confirmed; only cleanup is left. That is
            // a warning, not a restart blocker: the other graph still blocks.
            let partial = collector.snapshot().unwrap();
            assert_eq!(partial.count, Revision(1));
            assert_eq!(partial.incomplete_disposals.count, Revision(1));
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
            // The lost ACK came before the durable boundary, so nothing was
            // detached: the old scratch is intact. A new, explicitly requested
            // cleanup finishes the missing progress and resets exactly once.
            let dir = f.scratch.thread_dir(thread);
            assert!(dir.join("before").exists());
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
            let authority = f.runtime.disposal_authority(cas).unwrap();
            let done = f
                .runtime
                .dispose_owned_graph(&authority, next, &token)
                .unwrap();
            assert!(done.complete(), "{done:?}");
            token.confirm_graph_observation();
            drop(token);
            assert!(!dir.join("before").exists());
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
