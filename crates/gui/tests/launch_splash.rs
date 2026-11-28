//! The connection actor's launch progress against a real Unix socket: a core that
//! is already running is connected and read without the launcher reporting a
//! stage, and the splash is over once its projection is in; a core that cannot
//! be started leaves the failure for the existing status and Reconnect.
//!
//! Every wait is on the state the actor published (`Shared::changed`); nextest
//! owns hang detection.
#![cfg(target_os = "linux")]

#[path = "../../evorch/tests/restart_admin/fake_core.rs"]
mod fake_core;

use core_launcher::{CoreBinary, Launcher};
use fake_core::*;
use gui::core_client::{
    Shared,
    connection::{Action, serve},
    launch_progress::LaunchProgress,
};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;
use workbench_client::ClientConfig;
use workbench_protocol::{
    handshake::{BuildInfo, Capability, LocalSurface},
    ids::ClientId,
};

struct World {
    config: tempfile::TempDir,
    _sockets: tempfile::TempDir,
    socket: PathBuf,
    binary: PathBuf,
    identity: String,
}

/// A world whose core binary exits at once, so a start fails.
fn world() -> World {
    let config = tempfile::tempdir().unwrap();
    let sockets = tempfile::Builder::new()
        .prefix("gl")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::set_permissions(
        sockets.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    let dir = std::fs::canonicalize(sockets.path()).unwrap();
    let binary = dir.join("core");
    std::fs::write(&binary, "#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(&binary, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let identity = core_instance::ConfigIdentity::new(config.path())
        .unwrap()
        .identity()
        .to_owned();
    World {
        config,
        socket: dir.join("s"),
        _sockets: sockets,
        binary,
        identity,
    }
}

impl World {
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
    /// As `run` builds the window's state: the splash is up before the actor runs.
    fn start(launcher: Launcher) -> Self {
        let shared = Arc::new(Mutex::new(Shared {
            launch: LaunchProgress::starting(),
            ..Shared::default()
        }));
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

    async fn stop(self) {
        self.actions.send(Action::Shutdown).await.unwrap();
        self.actor.await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_running_core_is_connected_and_the_splash_ends_when_its_projection_is_in() {
    let world = world();
    let core = FakeCore::start_as(
        &world.socket,
        7,
        Capability::IMPLEMENTED.to_vec(),
        &world.identity,
    );
    let gui = Gui::start(world.launcher());
    assert!(gui.shared.lock().unwrap().launch.covers(0));
    gui.until(|s| s.connected).await;
    {
        let shared = gui.shared.lock().unwrap();
        assert!(shared.projection > 0);
        assert!(!shared.launch.covers(shared.projection));
    }
    gui.stop().await;
    core.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_core_that_cannot_be_started_leaves_its_failure_to_the_status_and_reconnect() {
    let world = world();
    let gui = Gui::start(world.launcher());
    gui.until(|s| s.launch.is_failed()).await;
    {
        let shared = gui.shared.lock().unwrap();
        assert!(!shared.connected);
        assert_eq!(shared.projection, 0);
        assert!(shared.launch.covers(shared.projection));
        assert!(!shared.status.is_empty());
        assert!(shared.status_detail.is_some());
    }
    gui.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_core_that_says_goodbye_while_the_first_snapshot_is_paged_leaves_no_progress_shown() {
    let world = world();
    let core = FakeCore::start_as(
        &world.socket,
        7,
        Capability::IMPLEMENTED.to_vec(),
        &world.identity,
    );
    core.state().hold_snapshot_page = true;
    let gui = Gui::start(world.launcher());
    // The actor has asked for the second page: it is reading the session.
    core.wait_for(|s| {
        s.seen.iter().any(|seen| {
            matches!(
                seen.body,
                workbench_protocol::envelope::RequestBody::Query(
                    workbench_protocol::queries::Query::Projection(
                        workbench_protocol::namespaces::projection::ProjectionQuery::ReadSnapshot(
                            _
                        )
                    )
                )
            )
        })
    })
    .await;
    assert!(
        gui.shared.lock().unwrap().launch.headline().is_some(),
        "the session is being read"
    );
    core.push(workbench_protocol::events::UiEvent::Projection(
        workbench_protocol::namespaces::ProjectionEvent::SubscriptionClosed {
            reason: workbench_protocol::events::CloseReason::CoreShuttingDown,
        },
    ));
    core.stop_after_flush();
    gui.until(|s| s.status.contains("announced its shutdown"))
        .await;
    {
        let shared = gui.shared.lock().unwrap();
        assert_eq!(shared.projection, 0);
        assert!(shared.launch.covers(shared.projection));
        assert!(
            shared.launch.headline().is_none(),
            "no progress is shown once the session is over: the status and Reconnect are"
        );
    }
    gui.stop().await;
}
