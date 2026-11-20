use super::{connect::*, reconnect::*};
use crate::ConnectedCore;
use std::{
    collections::VecDeque,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use workbench_client::{
    ClientConfig, WorkbenchClient,
    fake::FakeTransport,
    restart::reconnect::{PlannedEvidence, Stage},
};
use workbench_protocol::{
    handshake::{
        ActivityCounts, BuildInfo, Capability, CoreState, CoreStatus, ProtocolVersion, ServerHello,
        WireLimits,
    },
    ids::{ClientId, CoreIdentity, RestartId},
    wire::InstanceEpoch,
};

fn hello(epoch: u64) -> ServerHello {
    ServerHello {
        protocol_version: ProtocolVersion::CURRENT,
        capabilities: Capability::IMPLEMENTED.to_vec(),
        server_build: BuildInfo {
            name: "t".into(),
            version: "0".into(),
            revision: None,
        },
        core: CoreIdentity::new("core").unwrap(),
        instance_epoch: InstanceEpoch(epoch),
        limits: WireLimits::default(),
        status: CoreStatus {
            state: CoreState::Ready,
            retention_reasons: vec![],
            running: ActivityCounts::default(),
        },
    }
}

async fn ready_core(epoch: u64) -> ConnectedCore {
    let (transport, mut server) = FakeTransport::pair();
    let config = ClientConfig::new(
        ClientId::new("c").unwrap(),
        BuildInfo {
            name: "t".into(),
            version: "0".into(),
            revision: None,
        },
    );
    let hello = hello(epoch);
    let (client, _) = tokio::join!(
        async { WorkbenchClient::connect(transport, config).await.unwrap() },
        server.handshake(&hello),
    );
    // The fake core stays alive with the connection.
    std::mem::forget(server);
    ConnectedCore {
        client,
        endpoint: "/unused".into(),
    }
}

enum Scripted {
    Down,
    NotReady,
    Ready(u64),
}

/// A core whose appearance is scripted by attempt number.
struct Script {
    steps: Mutex<VecDeque<Scripted>>,
    then: fn() -> Scripted,
    attempts: AtomicUsize,
}
impl Script {
    fn new(steps: Vec<Scripted>, then: fn() -> Scripted) -> Self {
        Self {
            steps: Mutex::new(steps.into()),
            then,
            attempts: AtomicUsize::new(0),
        }
    }
}
impl ConnectOnce for Script {
    async fn attempt(&self) -> Result<Attempt, ConnectError> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        let next = self.steps.lock().unwrap().pop_front();
        Ok(match next.unwrap_or_else(self.then) {
            Scripted::Down => Attempt::NotRunning,
            Scripted::NotReady => Attempt::NotReady("draining".into()),
            Scripted::Ready(epoch) => Attempt::Ready(ready_core(epoch).await),
        })
    }
}

fn evidence() -> PlannedEvidence {
    PlannedEvidence {
        restart_id: Some(RestartId::new("r1").unwrap()),
        predecessor_epoch: InstanceEpoch(7),
        observed_at: clock_now(),
    }
}

fn policy() -> ReconnectPolicy {
    ReconnectPolicy {
        poll: Duration::from_secs(1),
        attempt_limit: Duration::from_secs(5),
    }
}

async fn run(
    script: &Script,
    finish_hangs: bool,
) -> (Result<Reconnected<u64>, ReconnectError>, Vec<(Stage, u64)>) {
    let mut seen = Vec::new();
    let result = reconnect_after_planned_restart(
        script,
        &evidence(),
        policy(),
        |view| seen.push((view.stage, view.remaining.as_secs())),
        |core| async move {
            if finish_hangs {
                std::future::pending::<()>().await;
            }
            Ok(core.client.server_hello().instance_epoch.get())
        },
    )
    .await;
    (result, seen)
}

#[tokio::test(start_paused = true)]
async fn a_successor_that_appears_while_the_old_core_drains_is_reconnected() {
    let script = Script::new(
        vec![
            Scripted::Down,
            Scripted::NotReady,
            Scripted::Down,
            Scripted::Ready(8),
        ],
        || Scripted::Ready(8),
    );
    let (result, seen) = run(&script, false).await;
    let Ok(Reconnected::Successor { epoch, value }) = result else {
        panic!("a successor: {result:?}");
    };
    assert_eq!((epoch, value), (InstanceEpoch(8), 8));
    assert_eq!(script.attempts.load(Ordering::SeqCst), 4);
    assert_eq!(
        seen.len(),
        4,
        "a progress line before each wait and one when ready"
    );
    let (waiting, ready) = seen.split_at(3);
    assert!(
        waiting
            .iter()
            .all(|(stage, _)| *stage == Stage::RestartingToReady)
    );
    assert_eq!(ready[0].0, Stage::ReadyToReconnect);
    assert!(
        waiting.windows(2).all(|w| w[0].1 >= w[1].1) && waiting[0].1 > waiting[2].1,
        "the time left never grows: {seen:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn no_successor_ends_at_120_seconds_without_starting_or_killing_anything() {
    let script = Script::new(vec![], || Scripted::Down);
    let (result, seen) = run(&script, false).await;
    assert!(matches!(
        result,
        Err(ReconnectError::Expired(Stage::RestartingToReady))
    ));
    let last = seen.last().unwrap();
    assert!(
        last.1 <= 1,
        "the last line shows the deadline is here: {seen:?}"
    );
    // One attempt per poll for the whole window and nothing more: this module
    // has no way to start a core, only to try to connect.
    let attempts = script.attempts.load(Ordering::SeqCst);
    assert!((100..=125).contains(&attempts), "{attempts}");
}

#[tokio::test(start_paused = true)]
async fn a_successor_whose_session_is_not_rebuilt_in_30_seconds_is_expired() {
    let script = Script::new(vec![Scripted::Down], || Scripted::Ready(8));
    let (result, _) = run(&script, true).await;
    assert!(matches!(
        result,
        Err(ReconnectError::Expired(Stage::ReadyToReconnect))
    ));
}

#[tokio::test(start_paused = true)]
async fn the_predecessor_still_serving_is_not_a_restart() {
    let script = Script::new(vec![], || Scripted::Ready(7));
    let (result, _) = run(&script, false).await;
    assert!(matches!(result, Ok(Reconnected::Predecessor { value: 7 })));
}

#[tokio::test(start_paused = true)]
async fn an_older_core_is_never_adopted_as_the_successor() {
    let script = Script::new(vec![], || Scripted::Ready(3));
    let (result, _) = run(&script, false).await;
    assert!(matches!(
        result,
        Err(ReconnectError::EpochWentBack {
            found: 3,
            before: 7
        })
    ));
}

#[tokio::test(start_paused = true)]
async fn a_core_that_answers_but_is_not_ready_does_not_end_the_wait_early() {
    let script = Script::new(vec![], || Scripted::NotReady);
    let (result, _) = run(&script, false).await;
    assert!(matches!(
        result,
        Err(ReconnectError::Expired(Stage::RestartingToReady))
    ));
}

#[tokio::test(start_paused = true)]
async fn the_ready_stage_and_its_deadline_are_announced_before_the_session_is_rebuilt() {
    let script = Script::new(vec![Scripted::Down], || Scripted::Ready(8));
    let (result, seen) = run(&script, true).await;
    assert!(matches!(
        result,
        Err(ReconnectError::Expired(Stage::ReadyToReconnect))
    ));
    assert_eq!(
        seen.last(),
        Some(&(Stage::ReadyToReconnect, 30)),
        "the 30 second window is shown when it opens: {seen:?}"
    );
}
