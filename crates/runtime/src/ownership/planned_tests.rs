use super::*;
use crate::ownership::{AdoptedOwnershipEvidence, ExpectedActiveRuns, OwnershipError, PlannedHold};
use std::collections::BTreeSet;

struct Fixture {
    host: OwnerHost,
    directory: test_socket_dir::SocketDir,
}
impl Fixture {
    fn new() -> Self {
        let directory = test_socket_dir::SocketDir::new();
        let host = OwnerHost::open(
            directory.path(),
            config::OwnershipConfig::default(),
            Arc::new(EventBus::new(64)),
        )
        .unwrap();
        host.stop_worker_for_exec().unwrap();
        Self { host, directory }
    }
    fn registry(&self) -> Registry {
        Registry::open_existing(&self.directory.path().join("owners.db")).unwrap()
    }
    fn marker(&self, thread: &str) -> PathBuf {
        let dir = self
            .host
            .scratch
            .prepare(&crate::scratch::ScratchKey::Thread(thread.into()))
            .unwrap();
        let file = dir.join("payload");
        std::fs::write(&file, b"keep\0bytes").unwrap();
        file
    }
    fn hold(&self) -> PlannedHold {
        self.host.start("idle").unwrap();
        self.registry()
            .update("idle", |owner| {
                owner.lease.expires_at = u64::MAX;
                Ok(())
            })
            .unwrap();
        self.host
            .hold_for_restart("tx", "capture", &ExpectedActiveRuns::new())
            .unwrap()
            .remove(0)
    }
    fn successor(&self) -> OwnerHost {
        OwnerHost::open(
            self.directory.path(),
            config::OwnershipConfig::default(),
            Arc::new(EventBus::new(64)),
        )
        .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.host.stop_worker_for_exec();
        let _ = std::fs::remove_dir_all(self.host.scratch.parent());
    }
}
// Test evidence stands in for 07b-1's Adopted receipt + inherited proof. No
// production constructor accepts a bool or deserialized owners.db as proof.
struct Evidence(Vec<PlannedHold>);
impl AdoptedOwnershipEvidence for Evidence {
    fn validate_hold(&self, hold: &PlannedHold) -> Result<(), OwnershipError> {
        if self.0.contains(hold) {
            Ok(())
        } else {
            Err(OwnershipError::PlannedMismatch)
        }
    }
}
fn kept(file: &Path) {
    assert_eq!(std::fs::read(file).unwrap(), b"keep\0bytes");
}

#[test]
fn start_cannot_reset_a_held_thread() {
    let f = Fixture::new();
    f.hold();
    let file = f.marker("idle");
    assert!(matches!(f.host.start("idle"), Err(RegistryError::Exists)));
    kept(&file);
}
#[test]
fn cold_claim_cannot_take_an_unexpired_hold() {
    let f = Fixture::new();
    let hold = f.hold();
    let file = f.marker("idle");
    let next = f.successor();
    assert!(matches!(
        next.claim(&f.host.attach("idle").unwrap()),
        Err(RegistryError::Ownership(OwnershipError::PlannedHeld))
    ));
    assert_eq!(
        f.host.attach("idle").unwrap().handoff_hold,
        Some(Box::new(hold))
    );
    kept(&file);
}
#[test]
fn handoff_cannot_release_or_reset_a_hold() {
    let f = Fixture::new();
    let permit = f.host.start("idle").unwrap();
    f.host
        .hold_for_restart("tx", "capture", &ExpectedActiveRuns::new())
        .unwrap();
    let file = f.marker("idle");
    let next = f.successor();
    assert!(matches!(
        f.host.handoff(&permit, &next),
        Err(RegistryError::Ownership(OwnershipError::PlannedHeld))
    ));
    kept(&file);
}
#[test]
fn release_ready_keeps_held_generation_and_scratch() {
    let f = Fixture::new();
    let hold = f.hold();
    let file = f.marker("idle");
    assert!(!f.host.release_ready().unwrap());
    assert_eq!(
        f.host.attach("idle").unwrap().handoff_hold,
        Some(Box::new(hold))
    );
    kept(&file);
}
#[test]
fn ipc_release_rejects_a_hold_before_cleanup() {
    let f = Fixture::new();
    let hold = f.hold();
    let file = f.marker("idle");
    f.host.restart_worker_failed_closed().unwrap();
    let response = ipc::request_without_timeout(
        &socket_path(f.directory.path(), f.host.owner_id()).unwrap(),
        &ipc::Request::Quiesce {
            thread_id: hold.thread_id.clone(),
            token: hold.lease(),
        },
    )
    .unwrap();
    assert!(matches!(response, ipc::Response::Rejected(_)));
    kept(&file);
}
#[test]
fn heartbeat_released_path_keeps_held_thread_and_scratch() {
    let f = Fixture::new();
    let hold = f.hold();
    let file = f.marker("idle");
    f.host.release_deferred.store(false, Ordering::SeqCst);
    heartbeat_tick(
        &mut f.registry(),
        &mut Default::default(),
        Worker {
            db: &f.host.db(),
            id: f.host.owner_id(),
            settings: &f.host.settings,
            bus: &f.host.bus,
            release_deferred: &f.host.release_deferred,
            scratch: &f.host.scratch,
        },
    );
    let owner = f.host.attach("idle").unwrap();
    assert_eq!(owner.state, OwnerState::Quiescing);
    assert_eq!(owner.handoff_hold, Some(Box::new(hold)));
    kept(&file);
}
#[test]
fn startup_sweep_keeps_holds_with_no_live_socket() {
    let f = Fixture::new();
    f.hold();
    let file = f.marker("idle");
    let _next = f.successor();
    kept(&file);
}
#[test]
fn limited_claim_clears_parked_active_runs_and_adopts_idle_threads_without_reset() {
    let f = Fixture::new();
    let old = f.host.start("parked").unwrap();
    f.host.start("idle").unwrap();
    f.registry()
        .update("parked", |owner| owner.begin_run(&old.lease, "run", 1))
        .unwrap();
    let expected = [(
        "parked".into(),
        crate::ownership::ExpectedThreadRuns {
            generation: old.lease.generation,
            active_runs: BTreeSet::from(["run".into()]),
        },
    )]
    .into();
    let holds = f.host.hold_for_restart("tx", "capture", &expected).unwrap();
    assert_eq!(holds.len(), 2);
    let parked = f.marker("parked");
    let idle = f.marker("idle");
    // Same process/PID, but the old endpoint has stopped: PID checks are wrong.
    let next = f.successor();
    for hold in &holds {
        let permit = next.claim_planned(hold, &Evidence(holds.clone())).unwrap();
        let owner = next.attach(&hold.thread_id).unwrap();
        assert_eq!(owner.lease.generation, hold.generation + 1);
        assert!(owner.active_runs.is_empty());
        assert!(!owner.active_turn);
        assert!(owner.handoff_hold.is_none());
        assert!(owner.lease.expires_at > 0);
        f.registry()
            .update(&hold.thread_id, |owner| {
                owner.begin_run(&permit.lease, "new-chat", 1)
            })
            .unwrap();
        // ACK-lost claim retry must not clear an already activated successor run.
        next.claim_planned(hold, &Evidence(holds.clone())).unwrap();
        assert!(
            next.attach(&hold.thread_id)
                .unwrap()
                .active_runs
                .contains("new-chat")
        );
        assert!(
            f.registry()
                .update(&hold.thread_id, |owner| owner
                    .begin_run(&old.lease, "old", 1))
                .is_err()
        );
    }
    kept(&parked);
    kept(&idle);
}
#[test]
fn limited_claim_requires_adopted_evidence_hold_and_exact_active_set() {
    let f = Fixture::new();
    let hold = f.hold();
    let file = f.marker("idle");
    let next = f.successor();
    assert!(next.claim_planned(&hold, &Evidence(vec![])).is_err());
    for changed in ["tx", "capture", "thread", "runs"] {
        let mut wrong = hold.clone();
        match changed {
            "tx" => wrong.transaction_id.push('x'),
            "capture" => wrong.capture_id.push('x'),
            "thread" => wrong.thread_id = "absent".into(),
            _ => {
                wrong.active_runs.insert("extra".into());
            }
        }
        assert!(
            next.claim_planned(&wrong, &Evidence(vec![wrong.clone()]))
                .is_err()
        );
    }
    f.registry()
        .update("idle", |owner| {
            owner.active_runs.insert("unexpected".into());
            owner.active_turn = true;
            Ok(())
        })
        .unwrap();
    assert!(
        next.claim_planned(&hold, &Evidence(vec![hold.clone()]))
            .is_err()
    );
    kept(&file);
}
#[test]
fn limited_claim_refuses_a_live_predecessor_endpoint() {
    let f = Fixture::new();
    let hold = f.hold();
    let file = f.marker("idle");
    f.host.restart_worker_failed_closed().unwrap();
    let next = f.successor();
    assert!(matches!(
        next.claim_planned(&hold, &Evidence(vec![hold.clone()])),
        Err(RegistryError::Ownership(OwnershipError::OwnerResponsive))
    ));
    kept(&file);
}
#[test]
fn stale_cold_claim_orphans_hold_and_preserves_scratch_across_sweep() {
    let f = Fixture::new();
    let hold = f.hold();
    let file = f.marker("idle");
    f.registry()
        .update("idle", |owner| {
            owner.lease.expires_at = 0;
            Ok(())
        })
        .unwrap();
    let next = f.successor();
    let permit = next.claim(&next.attach("idle").unwrap()).unwrap();
    let owner = next.attach("idle").unwrap();
    assert_eq!(owner.lease, permit.lease);
    let mut orphan = hold.clone();
    orphan.orphaned = true;
    assert_eq!(owner.handoff_hold, Some(Box::new(orphan)));
    kept(&file);
    let blocked = next
        .claim_planned(&hold, &Evidence(vec![hold.clone()]))
        .err()
        .unwrap();
    assert!(matches!(
        blocked,
        RegistryError::Ownership(OwnershipError::PlannedOwnerBlocked)
    ));
    assert!(blocked.to_string().contains("scratch保持は保証できない"));
    next.stop_worker_for_exec().unwrap();
    let _third = f.successor();
    kept(&file);
}
#[test]
fn failed_closed_removes_only_exact_hold_keeps_ownership_and_can_restore_worker() {
    let f = Fixture::new();
    let hold = f.hold();
    let file = f.marker("idle");
    let mut wrong = hold.clone();
    wrong.transaction_id.push('x');
    assert!(f.host.clear_planned_hold(&wrong).is_err());
    let cleared = f.host.clear_planned_hold(&hold).unwrap();
    assert_eq!(cleared.lease.owner_id, hold.owner_id);
    assert_eq!(cleared.lease.generation, hold.generation);
    assert_eq!(cleared.state, OwnerState::Quiescing);
    assert_eq!(f.host.clear_planned_hold(&hold).unwrap(), cleared);
    f.host.restart_worker_failed_closed().unwrap();
    let response = ipc::request_without_timeout(
        &socket_path(f.directory.path(), f.host.owner_id()).unwrap(),
        &ipc::Request::Attach {
            thread_id: "idle".into(),
        },
    )
    .unwrap();
    assert!(matches!(response, ipc::Response::Status(_)));
    f.host.stop_worker_for_exec().unwrap();
    f.host.stop_worker_for_exec().unwrap();
    assert!(
        !socket_path(f.directory.path(), f.host.owner_id())
            .unwrap()
            .exists()
    );
    kept(&file);
}
#[test]
fn all_owned_holds_are_atomic_and_do_not_take_another_owner() {
    let f = Fixture::new();
    f.host.start("a-idle").unwrap();
    let active = f.host.start("z-active").unwrap();
    f.registry()
        .update("z-active", |owner| owner.begin_run(&active.lease, "run", 1))
        .unwrap();
    assert!(
        f.host
            .hold_for_restart("tx", "capture", &ExpectedActiveRuns::new())
            .is_err()
    );
    assert!(f.host.attach("a-idle").unwrap().handoff_hold.is_none());
    let other = f.successor();
    other.start("other").unwrap();
    let expected = [(
        "z-active".into(),
        crate::ownership::ExpectedThreadRuns {
            generation: active.lease.generation,
            active_runs: BTreeSet::from(["run".into()]),
        },
    )]
    .into();
    let holds = f.host.hold_for_restart("tx", "capture", &expected).unwrap();
    assert_eq!(holds.len(), 2);
    assert!(other.attach("other").unwrap().handoff_hold.is_none());
    let alien = [(
        "other".into(),
        crate::ownership::ExpectedThreadRuns {
            generation: 1,
            active_runs: BTreeSet::new(),
        },
    )]
    .into();
    assert!(f.host.hold_for_restart("tx", "capture", &alien).is_err());
}
#[test]
fn generation_drift_never_clears_another_owners_hold() {
    let f = Fixture::new();
    let hold = f.hold();
    f.registry()
        .update("idle", |owner| {
            owner.lease.generation += 1;
            owner.lease.owner_id = "other".into();
            Ok(())
        })
        .unwrap();
    let before = f.host.attach("idle").unwrap();
    assert!(f.host.clear_planned_hold(&hold).is_err());
    assert!(!before.has_planned_hold());
    assert_eq!(f.host.attach("idle").unwrap(), before);
}
#[test]
fn unexplained_active_turn_and_generation_overflow_fail_closed() {
    let f = Fixture::new();
    let permit = f.host.start("idle").unwrap();
    f.registry()
        .update("idle", |owner| owner.begin_turn(&permit.lease, 1))
        .unwrap();
    assert!(
        f.host
            .hold_for_restart("tx", "capture", &ExpectedActiveRuns::new())
            .is_err()
    );
    f.registry()
        .update("idle", |owner| {
            owner.active_turn = false;
            owner.lease.generation = u64::MAX;
            Ok(())
        })
        .unwrap();
    let hold = f
        .host
        .hold_for_restart("tx", "capture", &ExpectedActiveRuns::new())
        .unwrap()
        .remove(0);
    let next = f.successor();
    assert!(matches!(
        next.claim_planned(&hold, &Evidence(vec![hold.clone()])),
        Err(RegistryError::Ownership(
            OwnershipError::GenerationExhausted
        ))
    ));
    assert_eq!(
        f.host.attach("idle").unwrap().handoff_hold,
        Some(Box::new(hold))
    );
}
#[test]
fn stop_worker_does_not_unlink_a_replaced_socket() {
    let f = Fixture::new();
    f.host.restart_worker_failed_closed().unwrap();
    let path = socket_path(f.directory.path(), f.host.owner_id()).unwrap();
    std::fs::remove_file(&path).unwrap();
    let _replacement = UnixListener::bind(&path).unwrap();
    assert!(f.host.stop_worker_for_exec().is_err());
    assert!(
        f.host.stop_worker_for_exec().is_err(),
        "stop failure is latched"
    );
    assert!(path.exists());
}

#[test]
fn captured_generation_drift_and_mid_write_failure_leave_no_partial_holds() {
    let f = Fixture::new();
    f.host.start("a").unwrap();
    f.host.start("z").unwrap();
    let wrong = [(
        "z".into(),
        crate::ownership::ExpectedThreadRuns {
            generation: 2,
            active_runs: BTreeSet::new(),
        },
    )]
    .into();
    assert!(matches!(
        f.host.hold_for_restart("tx", "capture", &wrong),
        Err(RegistryError::Ownership(
            OwnershipError::PlannedOwnerBlocked
        ))
    ));
    assert!(f.host.attach("a").unwrap().handoff_hold.is_none());
    // a is written before z fails. Atomicity is local to owners.db, never
    // assumed across the independent restart DB and this registry.
    f.registry().connection_for_tests().execute_batch("CREATE TRIGGER fail_hold BEFORE UPDATE ON thread_owners WHEN NEW.thread_id = 'z' BEGIN SELECT RAISE(ABORT, 'hold failure'); END;").unwrap();
    assert!(
        f.host
            .hold_for_restart("tx", "capture", &ExpectedActiveRuns::new())
            .is_err()
    );
    assert!(f.host.attach("a").unwrap().handoff_hold.is_none());
    assert!(f.host.attach("z").unwrap().handoff_hold.is_none());
}

#[test]
fn missing_hold_is_not_adoption_authority_and_failed_closed_keeps_parked_runs() {
    let f = Fixture::new();
    let old = f.host.start("parked").unwrap();
    f.registry()
        .update("parked", |owner| owner.begin_run(&old.lease, "run", 1))
        .unwrap();
    let expected = [(
        "parked".into(),
        crate::ownership::ExpectedThreadRuns {
            generation: 1,
            active_runs: BTreeSet::from(["run".into()]),
        },
    )]
    .into();
    let holds = f.host.hold_for_restart("tx", "capture", &expected).unwrap();
    assert_eq!(
        holds,
        f.host.hold_for_restart("tx", "capture", &expected).unwrap()
    );
    let hold = holds.into_iter().next().unwrap();
    let cleared = f.host.clear_planned_hold(&hold).unwrap();
    assert!(cleared.active_turn);
    assert_eq!(cleared.active_runs, hold.active_runs);
    let next = f.successor();
    assert!(matches!(
        next.claim_planned(&hold, &Evidence(vec![hold.clone()])),
        Err(RegistryError::Ownership(OwnershipError::PlannedMismatch))
    ));
    assert_eq!(next.attach("parked").unwrap(), cleared);
}
