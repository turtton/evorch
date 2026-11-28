//! The desktop actor against a core over a real Unix socket: after the core's
//! own evidence of a planned restart the lost connection is waited out
//! connect-only and the successor's session is rebuilt; a lost connection
//! without that evidence (and without the core's goodbye) asks the launcher once,
//! within the shared crash budget, and nothing is ever resent.
//!
//! Real sockets and real time throughout: no paused clock. Every wait is on an
//! event (the state the actor published); nextest owns hang detection.
#![cfg(target_os = "linux")]

#[path = "../../evorch/tests/restart_admin/fake_core.rs"]
mod fake_core;

use core_launcher::{CoreBinary, Launcher};
use fake_core::*;
use gui::core_client::{
    Shared,
    connection::{Action, serve},
    features::{FeatureView, Selection},
};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;
use workbench_client::ClientConfig;
use workbench_protocol::{
    handshake::{BuildInfo, Capability, LocalSurface},
    ids::{ClientId, RestartId},
    namespaces::lifecycle::RestartPhase,
    wire::InstanceEpoch,
};

struct World {
    config: tempfile::TempDir,
    _sockets: tempfile::TempDir,
    socket: PathBuf,
    marker: PathBuf,
    binary: PathBuf,
    identity: String,
}

fn world() -> World {
    let config = tempfile::tempdir().unwrap();
    let sockets = tempfile::Builder::new()
        .prefix("ga")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::set_permissions(
        sockets.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    let socket = std::fs::canonicalize(sockets.path()).unwrap().join("s");
    let marker = std::fs::canonicalize(sockets.path())
        .unwrap()
        .join("core-was-started");
    // A core binary that records that it was ever run: it must not be.
    let binary = std::fs::canonicalize(sockets.path()).unwrap().join("core");
    std::fs::write(
        &binary,
        format!("#!/bin/sh\necho started >> '{}'\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&binary, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let identity = core_instance::ConfigIdentity::new(config.path())
        .unwrap()
        .identity()
        .to_owned();
    World {
        config,
        _sockets: sockets,
        socket,
        marker,
        binary,
        identity,
    }
}

fn capabilities() -> Vec<Capability> {
    let mut all = Capability::IMPLEMENTED.to_vec();
    if !all.contains(&Capability::SafeRestart) {
        all.push(Capability::SafeRestart);
    }
    all
}

impl World {
    fn core(&self, epoch: u64) -> FakeCore {
        FakeCore::start_as(&self.socket, epoch, capabilities(), &self.identity)
    }

    fn launcher(&self) -> Launcher {
        let mut client = ClientConfig::new(
            ClientId::new("gui-test").unwrap(),
            BuildInfo {
                name: "evorch-gui".into(),
                version: "0".into(),
                revision: None,
            },
        );
        client.local_surface = Some(LocalSurface::Desktop);
        Launcher::with_socket(
            core_instance::ConfigIdentity::new(self.config.path()).unwrap(),
            CoreBinary::explicit(&self.binary).unwrap(),
            client,
            self.socket.clone(),
        )
        .unwrap()
    }
}

struct Gui {
    shared: Arc<Mutex<Shared>>,
    actions: mpsc::Sender<Action>,
    actor: tokio::task::JoinHandle<()>,
}

impl Gui {
    fn start(launcher: Launcher) -> Self {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let (actions, rx) = mpsc::channel(32);
        let actor = tokio::spawn(serve(
            launcher,
            rx,
            shared.clone(),
            egui::Context::default(),
        ));
        Self {
            shared,
            actions,
            actor,
        }
    }

    /// Wait until `ready` holds of what the actor published.
    async fn until(&self, ready: impl Fn(&Shared) -> bool) {
        let changed = self.shared.lock().unwrap().changed.clone();
        loop {
            let notified = changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if ready(&self.shared.lock().unwrap()) {
                return;
            }
            notified.await;
        }
    }

    /// One UI pass: what the features ask for goes to the actor, as the window would.
    async fn pass(&self) {
        let frame = {
            let mut state = self.shared.lock().unwrap();
            let Shared {
                features,
                cache,
                capabilities,
                connected,
                ..
            } = &mut *state;
            features.maintain_minimized(FeatureView {
                connected: *connected,
                capabilities,
                cache,
                selection: &Selection::default(),
            })
        };
        for action in frame.actions {
            self.actions.send(Action::Feature(action)).await.unwrap();
        }
    }

    fn epoch(shared: &Shared) -> Option<u64> {
        shared
            .connected
            .then(|| shared.cache.cursor().map(|c| c.core_epoch.get()))
            .flatten()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn after_the_cores_own_restart_evidence_the_successor_is_waited_for_and_no_core_is_started() {
    {
        let world = world();
        let predecessor = world.core(7);
        let gui = Gui::start(world.launcher());
        gui.until(|s| Gui::epoch(s) == Some(7)).await;
        gui.pass().await;
        predecessor.state().restart_id = Some(RestartId::new("restart-1").unwrap());
        predecessor.set_phase(RestartPhase::Capturing);
        // The status the core serves is what makes it evidence; read it as the window does.
        let changed = gui.shared.lock().unwrap().changed.clone();
        loop {
            // Registered before the check, so a change in between is not missed.
            let notified = changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            gui.pass().await;
            let seen = gui
                .shared
                .lock()
                .unwrap()
                .features
                .planned_restart(InstanceEpoch(7), std::time::Instant::now())
                .is_some();
            if seen {
                break;
            }
            notified.await;
        }
        predecessor.stop();
        gui.until(|s| s.status.contains("Restarting")).await;
        let successor = world.core(8);
        gui.until(|s| Gui::epoch(s) == Some(8)).await;
        assert!(
            !world.marker.exists(),
            "the wait is connect-only: no core was started"
        );
        assert!(
            successor.commands().is_empty(),
            "nothing is resent to the successor: {:?}",
            successor.commands()
        );
        let evidence = gui
            .shared
            .lock()
            .unwrap()
            .features
            .planned_restart(InstanceEpoch(8), std::time::Instant::now());
        assert!(evidence.is_none(), "a new session starts from nothing");
        gui.actions.send(Action::Shutdown).await.unwrap();
        gui.actor.await.unwrap();
        successor.stop();
    }
}

/// How many times the core binary was started: it appends a line when run.
fn starts(world: &World) -> usize {
    std::fs::read_to_string(&world.marker).map_or(0, |text| text.lines().count())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_abnormal_loss_attempts_one_start_then_recovers_connect_only_without_input() {
    let world = world();
    let first = world.core(7);
    let gui = Gui::start(world.launcher());
    gui.until(|s| Gui::epoch(s) == Some(7)).await;
    first.stop();
    gui.until(|s| {
        s.status_detail
            .as_deref()
            .is_some_and(|e| e.contains("core exited"))
    })
    .await;
    assert_eq!(starts(&world), 1, "one automatic start, no more");
    let config = core_instance::ConfigIdentity::new(world.config.path()).unwrap();
    assert_eq!(
        core_launcher::CrashBudget::recent_crashes(config.config_dir()).unwrap(),
        1
    );
    let second = world.core(8);
    gui.until(|s| Gui::epoch(s) == Some(8)).await;
    assert_eq!(
        starts(&world),
        1,
        "recovery only joins the externally started core"
    );
    assert!(gui.shared.lock().unwrap().status_detail.is_none());
    assert!(second.commands().is_empty());
    gui.actions.send(Action::Shutdown).await.unwrap();
    gui.actor.await.unwrap();
    second.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_exhausted_budget_keeps_its_hover_reason_and_recovers_without_starting() {
    {
        let world = world();
        // Three crashes already on record for this config directory.
        let config = core_instance::ConfigIdentity::new(world.config.path()).unwrap();
        let ledger = world.config.path().join(".evorch-core.crash-budget");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        std::fs::write(
            &ledger,
            format!(r#"{{"version":1,"crashes_ms":[{now},{now},{now}]}}"#),
        )
        .unwrap();
        std::fs::set_permissions(&ledger, std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .unwrap();
        let first = world.core(7);
        let gui = Gui::start(world.launcher());
        gui.until(|s| Gui::epoch(s) == Some(7)).await;
        first.stop();
        gui.until(|s| {
            s.status_detail
                .as_deref()
                .is_some_and(|e| e.contains("3 core crashes"))
        })
        .await;
        {
            let state = gui.shared.lock().unwrap();
            assert!(
                !state.status.contains("3 core crashes"),
                "details are hover-only"
            );
        }
        let second = world.core(8);
        gui.until(|s| Gui::epoch(s) == Some(8)).await;
        assert_eq!(
            starts(&world),
            0,
            "connect-only ignores no budget: it starts nothing"
        );
        assert_eq!(
            core_launcher::CrashBudget::recent_crashes(config.config_dir()).unwrap(),
            3
        );
        second.stop();
        gui.actions.send(Action::Shutdown).await.unwrap();
        gui.actor.await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_goodbye_and_the_end_of_the_stream_arriving_together_is_still_a_goodbye() {
    // The actor's select may see the close first; the goodbye is already queued.
    // Repeated so that either order is taken, each time against a new core.
    for _ in 0..24 {
        {
            let world = world();
            let core = world.core(7);
            let gui = Gui::start(world.launcher());
            gui.until(|s| Gui::epoch(s) == Some(7)).await;
            core.push(workbench_protocol::events::UiEvent::Projection(
                workbench_protocol::namespaces::ProjectionEvent::SubscriptionClosed {
                    reason: workbench_protocol::events::CloseReason::CoreShuttingDown,
                },
            ));
            core.stop_after_flush();
            gui.until(|s| s.status.contains("announced its shutdown"))
                .await;
            assert_eq!(starts(&world), 0, "a goodbye is not a crash");
            gui.actions.send(Action::Shutdown).await.unwrap();
            gui.actor.await.unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_goodbye_read_while_the_snapshot_is_paged_is_still_a_goodbye() {
    {
        let world = world();
        let core = world.core(7);
        core.state().hold_snapshot_page = true;
        let gui = Gui::start(world.launcher());
        // The actor has asked for the second page, so it holds its subscription and
        // is inside the snapshot paging. (The subscribe being recorded is not
        // enough: its reply may not have been read yet.)
        core.wait_for(|s| {
            s.seen.iter().any(|seen| {
                matches!(
                    seen.body,
                    workbench_protocol::envelope::RequestBody::Query(
                        workbench_protocol::queries::Query::Projection(
                            workbench_protocol::namespaces::projection::ProjectionQuery::ReadSnapshot(_)
                        )
                    )
                )
            })
        })
        .await;
        core.push(workbench_protocol::events::UiEvent::Projection(
            workbench_protocol::namespaces::ProjectionEvent::SubscriptionClosed {
                reason: workbench_protocol::events::CloseReason::CoreShuttingDown,
            },
        ));
        core.stop_after_flush();
        gui.until(|s| s.status.contains("announced its shutdown"))
            .await;
        assert_eq!(
            starts(&world),
            0,
            "a goodbye read while paging is not a crash"
        );
        gui.actions.send(Action::Shutdown).await.unwrap();
        gui.actor.await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_core_that_said_goodbye_is_not_started_again() {
    {
        let world = world();
        let first = world.core(7);
        let gui = Gui::start(world.launcher());
        gui.until(|s| Gui::epoch(s) == Some(7)).await;
        first.push(workbench_protocol::events::UiEvent::Projection(
            workbench_protocol::namespaces::ProjectionEvent::SubscriptionClosed {
                reason: workbench_protocol::events::CloseReason::CoreShuttingDown,
            },
        ));
        // The actor resyncs on the notice: its second subscribe proves it was read.
        first.wait_for(|s| s.subscribes.len() >= 2).await;
        first.stop_by_ref();
        gui.until(|s| s.status.contains("announced its shutdown"))
            .await;
        assert_eq!(starts(&world), 0, "an orderly stop is not restarted");
        gui.actions.send(Action::Shutdown).await.unwrap();
        gui.actor.await.unwrap();
    }
}

/// The core's own notice that this subscriber fell behind: the end of the
/// subscription, with a new snapshot as the way on.
fn slow_subscriber() -> workbench_protocol::events::UiEvent {
    workbench_protocol::events::UiEvent::Projection(
        workbench_protocol::namespaces::ProjectionEvent::ResyncRequired {
            reason: workbench_protocol::events::ResyncReason::SlowSubscriber,
        },
    )
}

fn read_a_snapshot_page(state: &fake_core::State) -> bool {
    state.seen.iter().any(|seen| {
        matches!(
            seen.body,
            workbench_protocol::envelope::RequestBody::Query(
                workbench_protocol::queries::Query::Projection(
                    workbench_protocol::namespaces::projection::ProjectionQuery::ReadSnapshot(_)
                )
            )
        )
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ended_subscription_read_while_the_snapshot_is_paged_is_reconnected() {
    // No `orderly`, no closed connection: the core only ended the subscription
    // while the snapshot was being read. It must not be left disconnected.
    let world = world();
    let core = world.core(7);
    core.state().hold_snapshot_page = true;
    let gui = Gui::start(world.launcher());
    // The actor asked for the second page, so it is inside the snapshot paging.
    core.wait_for(read_a_snapshot_page).await;
    core.state().hold_snapshot_page = false;
    core.push(slow_subscriber());
    // A second connection subscribing is the reconnect; the session it reads
    // is the one the window shows.
    core.wait_for(|s| s.subscribes.iter().any(|(conn, ..)| *conn >= 2))
        .await;
    gui.until(|s| Gui::epoch(s) == Some(7)).await;
    assert_eq!(starts(&world), 0, "joining the running core starts nothing");
    gui.actions.send(Action::Shutdown).await.unwrap();
    gui.actor.await.unwrap();
    assert!(
        core.state().subscribes.iter().all(|(conn, ..)| *conn <= 2),
        "ending the window does not reconnect: {:?}",
        core.state().subscribes
    );
    core.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ended_subscription_read_during_the_session_is_read_again() {
    let world = world();
    let core = world.core(7);
    let gui = Gui::start(world.launcher());
    gui.until(|s| Gui::epoch(s) == Some(7)).await;
    core.push(slow_subscriber());
    // A resync on this connection or a reconnect both subscribe again.
    core.wait_for(|s| s.subscribes.len() >= 2).await;
    gui.until(|s| Gui::epoch(s) == Some(7)).await;
    assert_eq!(starts(&world), 0, "the core never went away");
    gui.actions.send(Action::Shutdown).await.unwrap();
    gui.actor.await.unwrap();
    core.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_goodbye_read_while_paging_reconnects_without_starting_a_core() {
    let world = world();
    let core = world.core(7);
    core.state().hold_snapshot_page = true;
    let gui = Gui::start(world.launcher());
    core.wait_for(read_a_snapshot_page).await;
    core.push(workbench_protocol::events::UiEvent::Projection(
        workbench_protocol::namespaces::ProjectionEvent::SubscriptionClosed {
            reason: workbench_protocol::events::CloseReason::CoreShuttingDown,
        },
    ));
    core.state().hold_snapshot_page = false;
    core.wait_for(|s| s.subscribes.iter().any(|(conn, ..)| *conn >= 2))
        .await;
    gui.until(|s| Gui::epoch(s) == Some(7)).await;
    assert_eq!(
        starts(&world),
        0,
        "goodbye permits connect-only, not a start"
    );
    gui.actions.send(Action::Shutdown).await.unwrap();
    gui.actor.await.unwrap();
    core.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_initial_start_failure_recovers_when_an_external_core_appears() {
    let world = world();
    let gui = Gui::start(world.launcher());
    gui.until(|s| s.status_detail.is_some()).await;
    assert_eq!(starts(&world), 1);
    let core = world.core(7);
    gui.until(|s| Gui::epoch(s) == Some(7)).await;
    assert_eq!(starts(&world), 1);
    assert!(core.commands().is_empty());
    gui.actions.send(Action::Shutdown).await.unwrap();
    gui.actor.await.unwrap();
    core.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_socket_loss_recovers_automatically_without_starting_or_replaying() {
    let world = world();
    let core = world.core(7);
    let gui = Gui::start(world.launcher());
    gui.until(|s| Gui::epoch(s) == Some(7)).await;
    let projection = gui.shared.lock().unwrap().projection;
    core.disconnect_clients();
    gui.until(|s| s.connected && s.projection > projection)
        .await;
    assert_eq!(starts(&world), 0);
    assert!(core.commands().is_empty());
    gui.actions.send(Action::Shutdown).await.unwrap();
    gui.actor.await.unwrap();
    core.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_compatibility_failure_keeps_connecting_until_a_matching_core_appears() {
    let world = world();
    let incompatible = world.core(6);
    incompatible.state().reject_handshake = true;
    let gui = Gui::start(world.launcher());
    gui.until(|s| s.status.contains("compatibility mismatch"))
        .await;
    assert_eq!(starts(&world), 0, "an incompatible core is never replaced");
    incompatible.stop();
    let core = world.core(7);
    gui.until(|s| Gui::epoch(s) == Some(7)).await;
    assert_eq!(starts(&world), 0);
    gui.actions.send(Action::Shutdown).await.unwrap();
    gui.actor.await.unwrap();
    core.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_subscribe_failure_is_retried_without_manual_reconnect() {
    let world = world();
    let core = world.core(7);
    core.state().fail_subscribes = 1;
    let gui = Gui::start(world.launcher());
    gui.until(|s| Gui::epoch(s) == Some(7)).await;
    {
        let state = core.state();
        assert_eq!(state.fail_subscribes, 0);
        assert_eq!(
            state
                .seen
                .iter()
                .filter(|s| matches!(
                    s.body,
                    workbench_protocol::envelope::RequestBody::Control(
                        workbench_protocol::envelope::Control::Subscribe(_)
                    )
                ))
                .count(),
            2
        );
    }
    assert!(gui.shared.lock().unwrap().status_detail.is_none());
    assert_eq!(starts(&world), 0);
    gui.actions.send(Action::Shutdown).await.unwrap();
    gui.actor.await.unwrap();
    core.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_snapshot_failure_is_retried_without_manual_reconnect() {
    let world = world();
    let core = world.core(7);
    core.state().subscribe_mode = SubscribeMode::AlwaysReplay;
    let gui = Gui::start(world.launcher());
    core.wait_for(|s| s.subscribes.len() >= 2).await;
    core.state().subscribe_mode = SubscribeMode::Honest;
    gui.until(|s| Gui::epoch(s) == Some(7)).await;
    assert!(gui.shared.lock().unwrap().status_detail.is_none());
    assert_eq!(starts(&world), 0);
    gui.actions.send(Action::Shutdown).await.unwrap();
    gui.actor.await.unwrap();
    core.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_first_history_page_is_recorded_and_forgotten_on_reconnect() {
    let world = world();
    let core = world.core(7);
    let gui = Gui::start(world.launcher());
    gui.until(|s| Gui::epoch(s) == Some(7)).await;
    let projection = gui.shared.lock().unwrap().projection;
    // The restart fake rejects conversation reads. The real actor must record
    // that failure, rather than leaving only a transient status message.
    gui.actions
        .send(Action::History(
            workbench_protocol::ids::ThreadId::new("selected-thread").unwrap(),
            None,
            false,
        ))
        .await
        .unwrap();
    gui.until(|s| s.cache.history_read_failed("selected-thread"))
        .await;
    core.disconnect_clients();
    gui.until(|s| s.connected && s.projection > projection)
        .await;
    assert!(
        !gui.shared
            .lock()
            .unwrap()
            .cache
            .history_read_failed("selected-thread")
    );
    assert_eq!(starts(&world), 0);
    gui.actions.send(Action::Shutdown).await.unwrap();
    gui.actor.await.unwrap();
    core.stop();
}
