use super::*;
use crate::admin::failure;
use std::sync::{
    Barrier,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use workbench_protocol::namespaces::lifecycle::AdminId;
fn id(s: &str) -> AdminId {
    AdminId::new(s).unwrap()
}
fn fact(n: usize, undisposed: bool) -> GraphFact {
    GraphFact {
        reference: GraphReference {
            graph_id: id(&format!("graph-{n}")),
            restart_tx_id: id("tx"),
            capture_id: id("capture"),
            revision: Revision(1),
        },
        undisposed,
        incomplete: false,
        over_limit: false,
        ownership: (runtime::OwnedGraphOwnershipState::LiveSelf, 0, 0),
    }
}
fn runtime() -> AgentRuntime {
    struct Never;
    #[async_trait::async_trait]
    impl runtime::AgentModel for Never {
        fn selected_model(&self, _: runtime::Role, _: Option<&str>) -> String {
            "never".into()
        }
        async fn complete(
            &self,
            _: &runtime::AgentInvocationContext,
            _: runtime::Role,
            _: &[providers::Message],
            _: &[providers::ToolSpec],
        ) -> Result<providers::ChatResponse, RuntimeError> {
            panic!("observation must not request a provider")
        }
    }
    let bus = Arc::new(event_bus::EventBus::new(16));
    AgentRuntime::new(
        bus.clone(),
        Arc::new(::tools::ToolExecutor::new(bus)),
        Arc::new(Never),
    )
}
fn collector(rt: &AgentRuntime) -> Arc<LiveGraphCollector> {
    LiveGraphCollector::new(
        rt,
        StorageConfig {
            db_path: std::path::PathBuf::from("/no-collector-database/observation"),
            ..Default::default()
        },
    )
}
fn ready(c: &LiveGraphCollector) {
    c.bind_epoch(InstanceEpoch(7)).unwrap();
    let mut state = c.state.lock().unwrap();
    state.ready = true;
    state.graphs.insert("manifest".into(), fact(0, false));
}
fn reject<T>(_: &T) -> Arc<dyn Fn() -> bool + Send + Sync> {
    Arc::new(|| false)
}
#[test]
fn atomic_admission_runs_once_keeps_guard_and_excludes_effects_until_canonical_cancel() {
    let rt = runtime();
    let c = collector(&rt);
    ready(&c);
    let revision = c.snapshot().unwrap().snapshot_revision;
    let calls = AtomicUsize::new(0);
    let returned = Arc::new(AtomicBool::new(false));
    let predicate = returned.clone();
    let id = c
        .admit(
            InstanceEpoch(7),
            revision,
            &mut || {
                assert!(
                    c.state.try_lock().is_err(),
                    "the transition must hold the comparison guard"
                );
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(42)
            },
            move |_| Arc::new(move || predicate.load(Ordering::Acquire)),
        )
        .unwrap();
    assert_eq!(id, 42);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(c.begin_change("manifest").is_err());
    assert!(c.begin_automatic_change("manifest").is_err());
    returned.store(true, Ordering::Release);
    let ticket = c.begin_change("manifest").unwrap();
    assert!(c.snapshot().is_err());
    c.cancel_unstarted("manifest", ticket).unwrap();
    assert!(c.snapshot().unwrap().snapshot_revision > revision);
}
#[test]
fn stale_pending_membership_aba_and_epoch_refuse_before_transition() {
    let rt = runtime();
    let c = collector(&rt);
    ready(&c);
    let initial = c.snapshot().unwrap().snapshot_revision;
    assert_eq!(initial, c.snapshot().unwrap().snapshot_revision);
    let ticket = c.begin_change("manifest").unwrap();
    let mut transition =
        || -> Result<(), AdminFailure> { panic!("refusal must precede every gate/tx effect") };
    assert!(
        c.admit(InstanceEpoch(7), initial, &mut transition, reject)
            .is_err()
    );
    c.cancel_unstarted("manifest", ticket).unwrap();
    assert!(
        c.admit(InstanceEpoch(7), initial, &mut transition, reject)
            .is_err()
    );
    let revision = c.snapshot().unwrap().snapshot_revision;
    assert!(
        c.admit(InstanceEpoch(8), revision, &mut transition, reject)
            .is_err()
    );
    // Same count with different membership cannot reuse the old observation.
    let mut state = c.state.lock().unwrap();
    state.graphs.insert("manifest".into(), fact(1, true));
    state.advance().unwrap();
    drop(state);
    assert!(
        c.admit(InstanceEpoch(7), revision, &mut transition, reject)
            .is_err()
    );
    let revision = c.snapshot().unwrap().snapshot_revision;
    assert!(
        c.admit(InstanceEpoch(7), revision, &mut transition, reject)
            .is_err()
    );
}
#[test]
fn barrier_q2_cannot_overtake_atomic_gate_transition() {
    let rt = runtime();
    let c = collector(&rt);
    ready(&c);
    let revision = c.snapshot().unwrap().snapshot_revision;
    let entered = Arc::new(Barrier::new(2));
    let leave = Arc::new(Barrier::new(2));
    let worker = {
        let c = c.clone();
        let entered = entered.clone();
        let leave = leave.clone();
        std::thread::spawn(move || {
            c.admit(
                InstanceEpoch(7),
                revision,
                &mut || {
                    entered.wait();
                    leave.wait();
                    Ok(1)
                },
                reject,
            )
        })
    };
    entered.wait();
    assert!(c.state.try_lock().is_err());
    let (tx, rx) = std::sync::mpsc::channel();
    let q2 = {
        let c = c.clone();
        std::thread::spawn(move || {
            tx.send(()).unwrap();
            c.begin_change("manifest")
        })
    };
    rx.recv().unwrap();
    leave.wait();
    assert_eq!(worker.join().unwrap().unwrap(), 1);
    assert!(q2.join().unwrap().is_err());
}
#[test]
fn full_count_not_display_limit_and_warnings_are_not_blockers() {
    let rt = runtime();
    let c = collector(&rt);
    ready(&c);
    for n in [64, 65] {
        let mut state = c.state.lock().unwrap();
        state.graphs.clear();
        for i in 0..n {
            let mut f = fact(i, true);
            f.over_limit = true;
            state.graphs.insert(format!("m-{i}"), f);
        }
        state.advance().unwrap();
        drop(state);
        let snapshot = c.snapshot().unwrap();
        assert_eq!(snapshot.count, Revision(n as u64));
        assert_eq!(snapshot.references.len(), 64);
        assert_eq!(snapshot.truncated, n > 64);
        assert_eq!(snapshot.over_limit_count, Revision(n as u64));
        assert!(
            c.admit(
                InstanceEpoch(7),
                snapshot.snapshot_revision,
                &mut || Ok(()),
                reject
            )
            .is_err()
        );
    }
    let mut state = c.state.lock().unwrap();
    for f in state.graphs.values_mut() {
        f.undisposed = false;
        f.incomplete = true;
    }
    state.advance().unwrap();
    drop(state);
    let snapshot = c.snapshot().unwrap();
    assert_eq!(snapshot.count, Revision(0));
    assert_eq!(snapshot.incomplete_disposals.count, Revision(65));
    c.admit(
        InstanceEpoch(7),
        snapshot.snapshot_revision,
        &mut || Ok(()),
        reject,
    )
    .unwrap();
}
#[test]
fn unavailable_is_permanent_automatic_continues_manual_and_restart_are_closed() {
    let rt = runtime();
    let root = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: root.path().join("not-created.db"),
        ..Default::default()
    };
    let c = LiveGraphCollector::new(&rt, config.clone());
    c.bind_epoch(InstanceEpoch(7)).unwrap();
    rt.install_graph_observer(c.clone()).unwrap();
    c.rebuild().unwrap();
    assert!(c.snapshot().is_err());
    // Storage recovers later. Rebuild/readback must never reopen this process.
    let db = storage::Database::open(&config).unwrap();
    drop(db);
    c.rebuild().unwrap();
    let automatic = rt.reserve_automatic_activation("manifest").unwrap();
    drop(automatic);
    assert!(
        rt.reserve_manual_activation(
            "manifest",
            storage::recovery::graph_control::GraphEffectKey {
                operation_id: "op".into(),
                attempt_nonce: "nonce".into()
            },
            0
        )
        .is_err()
    );
    assert!(c.snapshot().is_err());
    assert!(
        c.admit(
            InstanceEpoch(7),
            Revision(2),
            &mut || -> Result<(), AdminFailure> { panic!("unavailable cannot transition") },
            reject
        )
        .is_err()
    );
    assert!(c.state.lock().unwrap().failed);
}
#[test]
fn uninitialized_poison_exhaustion_and_failed_transition_do_not_become_empty_success() {
    let rt = runtime();
    let c = collector(&rt);
    assert!(c.snapshot().is_err());
    ready(&c);
    let revision = c.snapshot().unwrap().snapshot_revision;
    assert!(
        c.admit(
            InstanceEpoch(7),
            revision,
            &mut || -> Result<(), AdminFailure> { Err(failure(ErrorCode::Conflict, "refused")) },
            reject
        )
        .is_err()
    );
    assert!(c.state.lock().unwrap().restart.is_none());
    c.state.lock().unwrap().revision = u64::MAX;
    assert!(c.begin_change("manifest").is_err());
    assert!(c.snapshot().is_err());
    let poisoned = collector(&rt);
    ready(&poisoned);
    let other = poisoned.clone();
    assert!(
        std::thread::spawn(move || {
            let _guard = other.state.lock().unwrap();
            panic!("poison");
        })
        .join()
        .is_err()
    );
    assert!(poisoned.snapshot().is_err());
    assert!(
        poisoned
            .begin_automatic_change("manifest")
            .unwrap()
            .is_none()
    );
    assert!(poisoned.begin_change("manifest").is_err());
}
#[test]
fn dropped_effect_ticket_remains_unknown_and_stale_readback_cannot_clear_successor() {
    let rt = runtime();
    let c = collector(&rt);
    ready(&c);
    rt.install_graph_observer(c.clone()).unwrap();
    let token = rt.reserve_automatic_activation("manifest").unwrap();
    drop(token);
    assert!(c.snapshot().is_err());
    assert!(c.begin_change("manifest").is_err());
    let ticket = *c.state.lock().unwrap().pending.get("manifest").unwrap();
    assert!(c.cancel_unstarted("manifest", ticket + 1).is_err());
    assert!(c.snapshot().is_err());
    // Observation loss cannot alter the existing automatic path, even when
    // the old unknown fence remains retained. Management never reopens.
    c.mark_unavailable();
    let next = rt.reserve_automatic_activation("manifest").unwrap();
    drop(next);
    assert!(c.snapshot().is_err());
    assert!(c.begin_change("manifest").is_err());
}
