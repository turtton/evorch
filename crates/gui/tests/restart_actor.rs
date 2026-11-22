//! The desktop actor against a core over a real Unix socket: after the core's
//! own evidence of a planned restart the lost connection is waited out
//! connect-only and the successor's session is rebuilt; a lost connection
//! without that evidence (and without the core's goodbye) asks the launcher once,
//! within the shared crash budget, and nothing is ever resent.
//!
//! Real sockets and real time throughout: no paused clock. Every wait is on an
//! event (the state the actor published), and the whole scenario is bounded.
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
    time::Duration,
};
use tokio::sync::mpsc;
use workbench_client::ClientConfig;
use workbench_protocol::{
    handshake::{BuildInfo, Capability, LocalSurface},
    ids::{ClientId, RestartId},
    namespaces::lifecycle::RestartPhase,
    wire::InstanceEpoch,
};

const BOUND: Duration = Duration::from_secs(60);

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
    tokio::time::timeout(BOUND, async {
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
        gui.until(|s| s.status.contains("再起動中")).await;
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
    })
    .await
    .expect("the scenario finishes");
}

/// How many times the core binary was started: it appends a line when run.
fn starts(world: &World) -> usize {
    std::fs::read_to_string(&world.marker).map_or(0, |text| text.lines().count())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_abnormal_loss_starts_a_core_once_within_the_budget_and_then_waits_for_the_person() {
    tokio::time::timeout(BOUND, async {
        let world = world();
        let first = world.core(7);
        let gui = Gui::start(world.launcher());
        gui.until(|s| Gui::epoch(s) == Some(7)).await;
        // Gone with no goodbye and no restart evidence: not a planned restart, so
        // nothing is waited for connect-only; the launcher is asked once. The test
        // binary exits at once without a core, which the launcher reports.
        first.stop();
        gui.until(|s| s.status.contains("接続失敗")).await;
        assert_eq!(starts(&world), 1, "one automatic start, no more");
        let config = core_instance::ConfigIdentity::new(world.config.path()).unwrap();
        assert_eq!(
            core_launcher::CrashBudget::recent_crashes(config.config_dir()).unwrap(),
            1,
            "the start that never became a core is a counted crash"
        );
        // A request now is refused as for any lost connection; that it is, is
        // the proof the actor is waiting for the person and not reconnecting.
        let second = world.core(8);
        gui.pass().await;
        gui.actions
            .send(Action::Feature(gui::core_client::features::FeatureAction {
                feature: gui::core_client::features::FeatureId::Restart,
                tag: 1,
                request: gui::core_client::features::FeatureRequest::Query(
                    workbench_protocol::queries::Query::Lifecycle(
                        workbench_protocol::namespaces::lifecycle::LifecycleQuery::Restart(
                            workbench_protocol::namespaces::lifecycle::restart::RestartQuery::Status,
                        ),
                    ),
                ),
            }))
            .await
            .unwrap();
        gui.until(|s| s.status.contains("明示的に再接続")).await;
        assert!(second.state().seen.is_empty(), "no one connected to it");
        gui.actions.send(Action::Reconnect).await.unwrap();
        gui.until(|s| Gui::epoch(s) == Some(8)).await;
        assert_eq!(starts(&world), 1, "joining a running core starts nothing");
        gui.actions.send(Action::Shutdown).await.unwrap();
        gui.actor.await.unwrap();
        second.stop();
    })
    .await
    .expect("the scenario finishes");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_exhausted_budget_is_shown_with_its_reason_and_nothing_is_started_or_retried() {
    tokio::time::timeout(BOUND, async {
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
        gui.until(|s| s.status.contains("再試行しません")).await;
        let shown = gui.shared.lock().unwrap().status.clone();
        assert!(
            shown.contains("3 core crashes"),
            "the reason is shown: {shown}"
        );
        assert!(shown.contains("再接続"), "and what to do next: {shown}");
        assert_eq!(starts(&world), 0, "nothing was started");
        assert_eq!(
            core_launcher::CrashBudget::recent_crashes(config.config_dir()).unwrap(),
            3
        );
        gui.actions.send(Action::Shutdown).await.unwrap();
        gui.actor.await.unwrap();
    })
    .await
    .expect("the scenario finishes");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_goodbye_and_the_end_of_the_stream_arriving_together_is_still_a_goodbye() {
    // The actor's select may see the close first; the goodbye is already queued.
    // Repeated so that either order is taken, each time against a new core.
    for _ in 0..24 {
        tokio::time::timeout(BOUND, async {
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
            gui.until(|s| s.status.contains("終了の通知あり")).await;
            assert_eq!(starts(&world), 0, "a goodbye is not a crash");
            gui.actions.send(Action::Shutdown).await.unwrap();
            gui.actor.await.unwrap();
        })
        .await
        .expect("the scenario finishes");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_core_that_said_goodbye_is_not_started_again() {
    tokio::time::timeout(BOUND, async {
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
        gui.until(|s| s.status.contains("終了の通知あり")).await;
        assert_eq!(starts(&world), 0, "an orderly stop is not restarted");
        gui.actions.send(Action::Shutdown).await.unwrap();
        gui.actor.await.unwrap();
    })
    .await
    .expect("the scenario finishes");
}
