use super::*;
use crate::lifecycle::{LifecycleDriver, OperatorSignals};
use std::time::Duration;

async fn masked_child() -> bool {
    if std::env::var_os("EVORCH_MASKED_TEST_CHILD").is_some() {
        return false;
    }
    // Reuse the production-entry signal mask fixture before libtest threads.
    let name = std::thread::current().name().unwrap().to_owned();
    let output = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &name, "--nocapture"])
        .env("EVORCH_MASKED_TEST_CHILD", "1")
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    true
}
fn signal() {
    // This is the opt-in child process, never the user's running core.
    assert_eq!(unsafe { libc::kill(libc::getpid(), libc::SIGTERM) }, 0);
}
#[derive(Clone, Copy)]
enum Mode {
    Signal,
    Control,
    Deadline,
    Interrupted,
}

async fn scenario(job: Job, mode: Mode, panic: bool) {
    tokio::task::LocalSet::new()
        .run_until(scenario_inner(job, mode, panic))
        .await;
}

async fn scenario_inner(job: Job, mode: Mode, panic: bool) {
    let f = Fixture::new().await;
    let socket = f.dir.sockets().join("lifecycle.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let mut driver = LifecycleDriver::new(&f.host);
    let control = driver.control_drain();
    let signals = OperatorSignals::new(crate::signal_test_entry::mask()).unwrap();
    let backend = f.backend.clone();
    let gate = f.hold(job, panic);
    let mut host = f.host;
    let mut lifecycle = tokio::task::spawn_local(async move {
        let result = driver
            .run_with_web(&host, backend, listener, None, signals)
            .await;
        host.shutdown().await.unwrap();
        result
    });
    let mut peer = Peer::connect(&socket, "active-job").await;
    let mut observer = Peer::connect(&socket, "shutdown-observer").await;
    let body = match job {
        Job::Request => query(),
        Job::Subscribe => subscribe(),
        Job::Unsubscribe => RequestBody::Control(Control::Unsubscribe {
            subscription_id: SubscriptionId::new("missing").unwrap(),
        }),
        _ => unreachable!(),
    };
    peer.request("held-job", body).await;
    gate.started.await.unwrap();
    // Only deadline semantics use time; no wall-clock completion condition.
    tokio::time::pause();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let draining = control.drain(deadline);
    tokio::pin!(draining);
    if matches!(mode, Mode::Signal) {
        signal();
    } else {
        pending(draining.as_mut()).await;
    }
    // EOF on an otherwise idle connection explicitly witnesses shutdown
    // publication; the held connection must still prevent lifecycle return.
    observer.eof().await;
    pending(Pin::new(&mut lifecycle)).await;
    let mut completed = gate.completed;
    assert!(matches!(
        completed.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    match mode {
        Mode::Deadline => {
            // Tokio rounds timers to milliseconds. Cross the deadline tick
            // explicitly: a held blocking job prevents automatic advancement.
            tokio::time::advance(Duration::from_secs(60) + Duration::from_millis(1)).await;
            assert!(
                draining.as_mut().await.is_err(),
                "deadline is a failure, not completion"
            );
            signal();
        }
        Mode::Interrupted => {
            signal();
            assert!(
                draining.as_mut().await.is_err(),
                "signal interruption must fail control drain"
            );
        }
        _ => (),
    }
    pending(Pin::new(&mut lifecycle)).await;
    gate.release.send(()).unwrap();
    completed.await.unwrap();
    peer.eof().await;
    if matches!(mode, Mode::Control) {
        assert_eq!(
            draining.as_mut().await.is_err(),
            panic,
            "connection panic must fail drain"
        );
        signal();
    }
    lifecycle.await.unwrap().unwrap();
    tokio::time::resume();
}

#[tokio::test]
async fn signal_and_control_drain_join_request_subscribe_and_unsubscribe() {
    if masked_child().await {
        return;
    }
    for mode in [Mode::Signal, Mode::Control] {
        for job in [Job::Request, Job::Subscribe, Job::Unsubscribe] {
            scenario(job, mode, false).await;
        }
    }
}

#[tokio::test]
async fn deadline_and_signal_interruption_still_join_inflight_jobs() {
    if masked_child().await {
        return;
    }
    scenario(Job::Request, Mode::Deadline, false).await;
    scenario(Job::Subscribe, Mode::Interrupted, false).await;
}

#[tokio::test]
async fn blocking_panic_remains_a_failed_connection_for_control_drain() {
    if masked_child().await {
        return;
    }
    scenario(Job::Request, Mode::Control, true).await;
}
