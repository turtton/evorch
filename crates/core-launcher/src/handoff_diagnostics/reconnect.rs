//! Waiting for the successor of a planned restart. Connect-only: while the
//! core restarts nothing here may start another one, so the only operation is
//! [`attempt`]. The deadlines belong to the pure machine in
//! `workbench_client::restart::reconnect`; this module only feeds it probes.
use super::connect::{Attempt, ConnectError, ConnectTarget, attempt};
use crate::ConnectedCore;
use std::{future::Future, time::Duration};
use tokio::time::{Instant, MissedTickBehavior, interval, timeout, timeout_at};
use workbench_client::{
    ClientConfig,
    restart::{
        reconnect::{PlannedEvidence, Probe, Stage, Step, on_connection_lost},
        view::ReconnectView,
    },
};
use workbench_protocol::{handshake::CoreState, wire::InstanceEpoch};

/// The clock the evidence and the deadlines are measured on.
pub fn clock_now() -> std::time::Instant {
    Instant::now().into_std()
}

/// Something that can try once to reach the core. Production is [`Target`].
pub trait ConnectOnce {
    fn attempt(&self) -> impl Future<Output = Result<Attempt, ConnectError>>;
}

pub struct Target<'a> {
    pub target: &'a ConnectTarget,
    pub client: &'a ClientConfig,
}
impl ConnectOnce for Target<'_> {
    async fn attempt(&self) -> Result<Attempt, ConnectError> {
        attempt(self.target, self.client).await
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ReconnectPolicy {
    /// Between attempts. A listener appearing has no portable async
    /// notification, so this is production coordination, never a test wait.
    pub poll: Duration,
    /// One attempt may not outlast this: a core mid-exec can accept and then say nothing.
    pub attempt_limit: Duration,
}
impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            poll: Duration::from_millis(100),
            attempt_limit: Duration::from_secs(5),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ReconnectError {
    #[error("{}", workbench_client::restart::view::describe_expiry(*.0))]
    Expired(Stage),
    #[error("the restart did not happen: the core at epoch {0} still serves")]
    NotRestarted(u64),
    #[error("a core with epoch {found} answered but the predecessor was epoch {before}")]
    EpochWentBack { found: u64, before: u64 },
    #[error(transparent)]
    Connect(#[from] ConnectError),
    #[error("{0}")]
    Session(String),
}

/// How the wait ended: with the session rebuilt on a new epoch, or on the
/// predecessor that never went away.
#[derive(Debug)]
pub enum Reconnected<T> {
    Successor { epoch: InstanceEpoch, value: T },
    Predecessor { value: T },
}

/// Wait for a ready successor, then rebuild the session with `finish` inside
/// the fixed 30 second window. `progress` sees the stage and the time left
/// before each wait. Nothing is resent: `finish` decides what to ask.
pub async fn reconnect_after_planned_restart<T, Fut>(
    connector: &impl ConnectOnce,
    evidence: &PlannedEvidence,
    policy: ReconnectPolicy,
    mut progress: impl FnMut(ReconnectView),
    mut finish: impl FnMut(ConnectedCore) -> Fut,
) -> Result<Reconnected<T>, ReconnectError>
where
    Fut: Future<Output = Result<T, String>>,
{
    let mut machine = on_connection_lost(Some(evidence)).expect("evidence is given");
    let mut tick = interval(policy.poll);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        let (_, deadline) = machine.deadline();
        let limit = policy
            .attempt_limit
            .min(Instant::from_std(deadline).saturating_duration_since(Instant::now()));
        let found = timeout(limit, connector.attempt()).await;
        let (probe, held) = match found {
            Ok(Ok(Attempt::Ready(core))) => (
                Probe::Reachable {
                    epoch: core.client.server_hello().instance_epoch,
                    state: CoreState::Ready,
                },
                Some(core),
            ),
            Ok(Ok(Attempt::NotReady(_))) | Err(_) => (
                Probe::Reachable {
                    epoch: InstanceEpoch(0),
                    state: CoreState::Starting,
                },
                None,
            ),
            Ok(Ok(Attempt::NotRunning)) => (Probe::NotRunning, None),
            Ok(Err(e)) => return Err(e.into()),
        };
        match machine.probe(clock_now(), probe) {
            Step::Wait { remaining } => {
                progress(ReconnectView {
                    stage: machine.deadline().0,
                    remaining,
                });
                tick.tick().await;
            }
            Step::Expired(stage) => return Err(ReconnectError::Expired(stage)),
            Step::EpochWentBack { found } => {
                return Err(ReconnectError::EpochWentBack {
                    found: found.get(),
                    before: evidence.predecessor_epoch.get(),
                });
            }
            Step::PredecessorStillRunning => {
                let core = held.expect("a ready core was found");
                let value = finish(core).await.map_err(ReconnectError::Session)?;
                return Ok(Reconnected::Predecessor { value });
            }
            Step::SuccessorReady { epoch } => {
                let core = held.expect("a ready core was found");
                let (_, deadline) = machine.deadline();
                let value = match timeout_at(Instant::from_std(deadline), finish(core)).await {
                    Ok(Ok(value)) => value,
                    Ok(Err(e)) => return Err(ReconnectError::Session(e)),
                    Err(_) => return Err(ReconnectError::Expired(Stage::ReadyToReconnect)),
                };
                machine
                    .complete(clock_now())
                    .map_err(ReconnectError::Expired)?;
                return Ok(Reconnected::Successor { epoch, value });
            }
        }
    }
}
