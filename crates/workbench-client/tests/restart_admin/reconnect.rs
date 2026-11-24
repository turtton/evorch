use std::time::{Duration, Instant};
use workbench_client::restart::reconnect::*;
use workbench_protocol::{
    handshake::{ActivityCounts, CoreState, CoreStatus},
    ids::RestartId,
    namespaces::lifecycle::{
        RestartPhase,
        restart::{AdminAuditDiagnostics, RestartManagementStatus, RestartStatus},
    },
    wire::InstanceEpoch,
};

fn status(phase: RestartPhase, epoch: u64) -> RestartManagementStatus {
    RestartManagementStatus {
        audit: AdminAuditDiagnostics::default(),
        core_epoch: InstanceEpoch(epoch),
        status: RestartStatus {
            restart_id: Some(RestartId::new("r1").unwrap()),
            phase,
            revision: 3,
            blockers: vec![],
            blocker_total: 0,
            blocker_omitted: 0,
            active_producers: 0,
            cancel_reason: None,
        },
    }
}

fn core(state: CoreState) -> CoreStatus {
    CoreStatus {
        state,
        retention_reasons: vec![],
        running: ActivityCounts::default(),
    }
}

fn evidence(at: Instant) -> PlannedEvidence {
    let mut observer = EvidenceObserver::default();
    observer.observe_status(&status(RestartPhase::Finalizing, 7), at);
    observer
        .evidence()
        .cloned()
        .expect("finalizing is evidence")
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn ready(epoch: u64) -> Probe {
    Probe::Reachable {
        epoch: InstanceEpoch(epoch),
        state: CoreState::Ready,
    }
}

#[test]
fn a_closed_connection_alone_is_not_a_planned_restart() {
    assert!(on_connection_lost(None).is_none());
    assert!(on_connection_lost(EvidenceObserver::default().evidence()).is_none());
}

#[test]
fn only_a_restart_past_the_point_of_no_return_is_evidence() {
    let now = Instant::now();
    for phase in [
        RestartPhase::Running,
        RestartPhase::Requested,
        RestartPhase::PreparingDrains,
        RestartPhase::WaitingSafePoint,
        RestartPhase::Sealing,
        RestartPhase::Sealed,
        RestartPhase::Cancelling,
        RestartPhase::FailedClosed,
    ] {
        let mut observer = EvidenceObserver::default();
        observer.observe_status(&status(phase, 7), now);
        assert!(observer.evidence().is_none(), "{phase:?}");
    }
    for phase in [
        RestartPhase::Capturing,
        RestartPhase::Finalizing,
        RestartPhase::ShuttingDown,
    ] {
        let mut observer = EvidenceObserver::default();
        observer.observe_status(&status(phase, 7), now);
        assert_eq!(
            observer.evidence().unwrap().predecessor_epoch,
            InstanceEpoch(7)
        );
    }
}

#[test]
fn the_core_saying_it_is_restarting_is_evidence_and_ready_is_not() {
    let now = Instant::now();
    let mut observer = EvidenceObserver::default();
    observer.observe_core(&core(CoreState::Ready), InstanceEpoch(7), now);
    observer.observe_core(&core(CoreState::Draining), InstanceEpoch(7), now);
    assert!(observer.evidence().is_none());
    observer.observe_core(&core(CoreState::Restarting), InstanceEpoch(7), now);
    assert!(observer.evidence().is_some());
}

#[test]
fn evidence_of_a_restart_that_turned_back_does_not_make_a_later_crash_planned() {
    let now = Instant::now();
    let mut observer = EvidenceObserver::default();
    observer.observe_status(&status(RestartPhase::Finalizing, 7), now);
    assert!(observer.evidence().is_some());
    observer.observe_status(&status(RestartPhase::Running, 7), now);
    assert!(observer.evidence().is_none());
}

#[test]
fn the_restarting_window_is_120_seconds_from_the_evidence_and_never_longer() {
    let base = Instant::now();
    let mut reconnect = on_connection_lost(Some(&evidence(base))).unwrap();
    // The connection was lost long after the evidence: the clock did not restart.
    assert_eq!(
        reconnect.probe(base + secs(100), Probe::NotRunning),
        Step::Wait {
            remaining: secs(20)
        }
    );
    // Probing again, however often, and a core that is not ready, do not extend it.
    for state in [
        CoreState::Starting,
        CoreState::Quiescing,
        CoreState::Draining,
        CoreState::Restarting,
    ] {
        assert!(matches!(
            reconnect.probe(
                base + secs(119),
                Probe::Reachable {
                    epoch: InstanceEpoch(8),
                    state
                }
            ),
            Step::Wait { .. }
        ));
    }
    assert_eq!(
        reconnect.probe(base + secs(120), Probe::NotRunning),
        Step::Expired(Stage::RestartingToReady)
    );
    // Expired is final: a ready successor after the deadline does not revive it.
    assert_eq!(
        reconnect.probe(base + secs(121), ready(8)),
        Step::Expired(Stage::RestartingToReady)
    );
}

#[test]
fn a_ready_successor_with_a_new_epoch_opens_a_30_second_window_that_is_fixed() {
    let base = Instant::now();
    let mut reconnect = on_connection_lost(Some(&evidence(base))).unwrap();
    assert_eq!(
        reconnect.probe(base + secs(10), ready(8)),
        Step::SuccessorReady {
            epoch: InstanceEpoch(8)
        }
    );
    assert_eq!(
        reconnect.deadline(),
        (Stage::ReadyToReconnect, base + secs(40))
    );
    // Repeated probes never move the window.
    assert_eq!(
        reconnect.probe(base + secs(30), ready(8)),
        Step::Wait {
            remaining: secs(10)
        }
    );
    assert_eq!(
        reconnect.deadline(),
        (Stage::ReadyToReconnect, base + secs(40))
    );
    assert_eq!(reconnect.complete(base + secs(40)), Ok(()));
}

#[test]
fn rebuilding_the_session_after_the_window_is_too_late() {
    let base = Instant::now();
    let mut reconnect = on_connection_lost(Some(&evidence(base))).unwrap();
    reconnect.probe(base + secs(10), ready(8));
    assert_eq!(
        reconnect.complete(base + secs(41)),
        Err(Stage::ReadyToReconnect)
    );
    // And it stays expired.
    assert_eq!(
        reconnect.probe(base + secs(41), ready(8)),
        Step::Expired(Stage::ReadyToReconnect)
    );
}

#[test]
fn the_window_cannot_be_completed_before_a_successor_was_ready() {
    let base = Instant::now();
    let mut reconnect = on_connection_lost(Some(&evidence(base))).unwrap();
    assert_eq!(reconnect.complete(base), Err(Stage::RestartingToReady));
}

#[test]
fn the_predecessor_answering_ready_at_its_own_epoch_means_no_restart_happened() {
    let base = Instant::now();
    let mut reconnect = on_connection_lost(Some(&evidence(base))).unwrap();
    assert_eq!(
        reconnect.probe(base + secs(1), ready(7)),
        Step::PredecessorStillRunning
    );
}

#[test]
fn a_core_with_an_older_epoch_is_not_the_successor() {
    let base = Instant::now();
    let mut reconnect = on_connection_lost(Some(&evidence(base))).unwrap();
    assert_eq!(
        reconnect.probe(base + secs(1), ready(3)),
        Step::EpochWentBack {
            found: InstanceEpoch(3)
        }
    );
}
