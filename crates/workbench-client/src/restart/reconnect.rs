use std::time::{Duration, Instant};
use workbench_protocol::{
    handshake::{CoreState, CoreStatus},
    ids::RestartId,
    namespaces::lifecycle::{RestartPhase, restart::RestartManagementStatus},
    wire::InstanceEpoch,
};

/// Restarting until the successor accepts control connections. Fixed from the
/// moment the restart was seen to be under way; never extended.
pub const RESTARTING_TO_READY: Duration = Duration::from_secs(120);
/// From the successor's control readiness until the client has its session
/// back (handshake, snapshot, operation readback). Fixed at the first sight of
/// the ready successor; never extended.
pub const READY_TO_RECONNECT: Duration = Duration::from_secs(30);

/// What the core itself said that makes an end of the connection a planned
/// restart. A closed connection says nothing by itself: a crash, a kill and
/// an exec all look the same from outside.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedEvidence {
    pub restart_id: Option<RestartId>,
    /// The epoch of the core that is going away.
    pub predecessor_epoch: InstanceEpoch,
    pub observed_at: Instant,
}

/// Folds what the core reports into the evidence a lost connection is judged by.
#[derive(Debug, Default)]
pub struct EvidenceObserver {
    evidence: Option<PlannedEvidence>,
}
impl EvidenceObserver {
    pub fn evidence(&self) -> Option<&PlannedEvidence> {
        self.evidence.as_ref()
    }

    /// The restart is past capture: the old work is never revived, so only the
    /// exec (or a failed-closed stop, which clears this again) can follow.
    pub fn observe_status(&mut self, status: &RestartManagementStatus, now: Instant) {
        let committed = matches!(
            status.status.phase,
            RestartPhase::Capturing | RestartPhase::Finalizing | RestartPhase::ShuttingDown
        );
        match (&self.evidence, committed, &status.status.restart_id) {
            (None, true, Some(id)) => {
                self.evidence = Some(PlannedEvidence {
                    restart_id: Some(id.clone()),
                    predecessor_epoch: status.core_epoch,
                    observed_at: now,
                });
            }
            // The restart this evidence was for is gone and the core runs on:
            // an old observation must not turn a later crash into a restart.
            (Some(_), false, _)
                if matches!(
                    status.status.phase,
                    RestartPhase::Running | RestartPhase::Cancelling | RestartPhase::FailedClosed
                ) =>
            {
                self.evidence = None;
            }
            _ => {}
        }
    }

    /// The core's own state: `Restarting` is its statement that an exec follows.
    pub fn observe_core(&mut self, status: &CoreStatus, epoch: InstanceEpoch, now: Instant) {
        if status.state == CoreState::Restarting && self.evidence.is_none() {
            self.evidence = Some(PlannedEvidence {
                restart_id: None,
                predecessor_epoch: epoch,
                observed_at: now,
            });
        }
    }
}

/// What one connect-only attempt found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// Nothing listens (no socket, or a refused connection).
    NotRunning,
    Reachable {
        epoch: InstanceEpoch,
        state: CoreState,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    RestartingToReady,
    ReadyToReconnect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Try again; `remaining` is what is left of the deadline of this stage.
    Wait {
        remaining: Duration,
    },
    /// A successor with a new epoch is ready: the 30 second window has begun.
    SuccessorReady {
        epoch: InstanceEpoch,
    },
    /// The core that was going away still answers at the same epoch: the
    /// restart did not happen (cancelled or rolled back).
    PredecessorStillRunning,
    /// A core with an older epoch than the predecessor: not this core's successor.
    EpochWentBack {
        found: InstanceEpoch,
    },
    Expired(Stage),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Awaiting,
    Ready,
    Over(Step),
}

/// The reconnect after a planned restart. Every deadline is absolute and set
/// once; a retry, a repeated probe or a reconnect cannot move it.
#[derive(Debug)]
pub struct Reconnect {
    evidence: PlannedEvidence,
    restarting_deadline: Instant,
    ready_deadline: Option<Instant>,
    phase: Phase,
}

/// A lost connection is only a restart with evidence.
pub fn on_connection_lost(evidence: Option<&PlannedEvidence>) -> Option<Reconnect> {
    evidence.map(|e| Reconnect {
        restarting_deadline: e.observed_at + RESTARTING_TO_READY,
        evidence: e.clone(),
        ready_deadline: None,
        phase: Phase::Awaiting,
    })
}

impl Reconnect {
    pub fn evidence(&self) -> &PlannedEvidence {
        &self.evidence
    }

    pub fn probe(&mut self, now: Instant, found: Probe) -> Step {
        match self.phase {
            Phase::Over(step) => return step,
            Phase::Ready => return self.ready_step(now),
            Phase::Awaiting => {}
        }
        if now >= self.restarting_deadline {
            return self.finish(Step::Expired(Stage::RestartingToReady));
        }
        let wait = Step::Wait {
            remaining: self.restarting_deadline - now,
        };
        let Probe::Reachable { epoch, state } = found else {
            return wait;
        };
        if state != CoreState::Ready {
            return wait;
        }
        let before = self.evidence.predecessor_epoch;
        if epoch == before {
            self.finish(Step::PredecessorStillRunning)
        } else if epoch < before {
            Step::EpochWentBack { found: epoch }
        } else {
            self.ready_deadline = Some(now + READY_TO_RECONNECT);
            self.phase = Phase::Ready;
            Step::SuccessorReady { epoch }
        }
    }

    fn finish(&mut self, step: Step) -> Step {
        self.phase = Phase::Over(step);
        step
    }

    fn ready_step(&mut self, now: Instant) -> Step {
        let deadline = self.ready_deadline.expect("set with the ready phase");
        if now >= deadline {
            self.finish(Step::Expired(Stage::ReadyToReconnect))
        } else {
            Step::Wait {
                remaining: deadline - now,
            }
        }
    }

    /// The session was rebuilt. Late is not good enough: the window is checked
    /// at the moment of completion.
    pub fn complete(&mut self, now: Instant) -> Result<(), Stage> {
        let deadline = self.ready_deadline.ok_or(Stage::RestartingToReady)?;
        let result = if now > deadline {
            Err(Stage::ReadyToReconnect)
        } else {
            Ok(())
        };
        if result.is_err() {
            self.phase = Phase::Over(Step::Expired(Stage::ReadyToReconnect));
        }
        result
    }

    /// The deadline in force and its stage, for display.
    pub fn deadline(&self) -> (Stage, Instant) {
        match self.ready_deadline {
            Some(d) => (Stage::ReadyToReconnect, d),
            None => (Stage::RestartingToReady, self.restarting_deadline),
        }
    }
}
