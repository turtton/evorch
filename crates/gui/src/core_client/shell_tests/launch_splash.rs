//! The splash a window shows until its first projection is in: the launcher's
//! stages, the connection and the session being read, and then the workspace.
use std::sync::atomic::{AtomicUsize, Ordering};

use egui_kittest::kittest::Queryable;

use super::*;
use crate::core_client::Draft;
use crate::core_client::launch_progress::{LaunchProgress, observer};
use core_launcher::LaunchStage;

fn waiting() -> Screen {
    screen_from(egui::vec2(1440.0, 900.0), no_local(), |shared| {
        shared.connected = false;
        shared.projection = 0;
        shared.launch = LaunchProgress::starting();
    })
}

fn no_local() -> crate::core_client::local::LocalPanes {
    crate::core_client::local::LocalPanes::new(
        Arc::new(crate::diff::FixtureDiffSource::empty()),
        None,
    )
}

impl Screen {
    fn launch(&mut self, change: impl FnOnce(&mut LaunchProgress) -> bool) {
        assert!(change(&mut self.shared.lock().unwrap().launch));
        self.harness.run_steps(2);
    }
}

#[test]
fn every_launch_stage_is_shown_and_the_first_projection_closes_the_splash() {
    let mut screen = waiting();
    screen.harness.run_steps(2);
    screen.harness.get_by_label("Connecting to the core…");
    for (stage, text) in [
        (
            LaunchStage::WaitingForLaunch,
            "Waiting for another client to start the core…",
        ),
        (LaunchStage::BeforeSpawn, "Starting the core…"),
        (
            LaunchStage::Spawned(4242),
            "The core started (pid 4242). Waiting for it to answer…",
        ),
        (
            LaunchStage::LifetimeBusy,
            "Waiting for the existing core to become ready…",
        ),
        (
            LaunchStage::AlreadyRunning,
            "A core is already running. Connecting to it…",
        ),
    ] {
        screen.launch(|launch| launch.stage(stage));
        screen.harness.get_by_label(text);
        assert!(
            screen
                .harness
                .query_by_label("Connecting to the core…")
                .is_none()
        );
    }
    screen.launch(LaunchProgress::reading_state);
    screen.harness.get_by_label("Reading the core's state…");
    assert!(
        screen
            .harness
            .query_by_label("Waiting for the existing core to become ready…")
            .is_none()
    );
}

#[test]
fn a_connection_that_needs_no_launch_still_shows_connecting_then_reading_until_begin_session() {
    let mut screen = waiting();
    screen.harness.run_steps(2);
    screen.harness.get_by_label("Connecting to the core…");
    // The core answered at once: the launcher reported no stage, and the socket
    // being connected is not the end of the wait.
    screen.launch(LaunchProgress::reading_state);
    screen.harness.get_by_label("Reading the core's state…");
    {
        let mut shared = screen.shared.lock().unwrap();
        shared.begin_session(Default::default(), Vec::new(), &[], Vec::new());
    }
    screen.harness.run_steps(2);
    assert!(
        screen
            .harness
            .query_by_label("Reading the core's state…")
            .is_none()
    );
    assert!(screen.shared.lock().unwrap().projection > 0);
}

#[test]
fn a_failure_is_shown_with_the_existing_status_and_reconnect_restarts_the_attempt() {
    let mut screen = waiting();
    let detail = "injected full error detail at /private/core.sock";
    {
        let mut shared = screen.shared.lock().unwrap();
        shared.status = "The core did not start.".into();
        shared.status_detail = Some(detail.into());
    }
    screen.launch(LaunchProgress::failed);
    assert!(
        screen
            .harness
            .query_by_label("Connecting to the core…")
            .is_none()
    );
    // The footer and the splash both name the status; the error itself is on hover.
    assert_eq!(
        screen
            .harness
            .get_all_by_label("The core did not start.")
            .count(),
        2
    );
    assert!(screen.harness.query_by_label(detail).is_none());
    screen.harness.get_by_label("Reconnect").click();
    screen.harness.run_steps(2);
    assert!(
        screen
            .settle()
            .iter()
            .any(|action| matches!(action, Action::Reconnect))
    );
}

#[test]
fn a_loss_after_the_first_projection_keeps_the_conversation_and_the_draft_in_view() {
    let mut screen = screen_from(egui::vec2(1440.0, 900.0), no_local(), |shared| {
        shared.projection = 1;
        shared.drafts.insert(
            "thr-1".into(),
            Draft {
                text: "unsent words".into(),
                revision: 1,
                ..Default::default()
            },
        );
    });
    screen.settle();
    // The connection is lost and the actor reconnects: it asks for a connection,
    // the launcher reports stages and then the session is read again.
    {
        let mut shared = screen.shared.lock().unwrap();
        shared.connected = false;
        shared.status = crate::core_client::connection::CONNECTING.into();
        shared.launch = LaunchProgress::starting();
    }
    for change in [
        (|launch: &mut LaunchProgress| launch.stage(LaunchStage::BeforeSpawn))
            as fn(&mut LaunchProgress) -> bool,
        LaunchProgress::reading_state,
        LaunchProgress::failed,
    ] {
        screen.launch(change);
        for text in [
            "Starting the core…",
            "Reading the core's state…",
            "Reconnect",
        ] {
            assert!(screen.harness.query_by_label(text).is_none(), "{text}");
        }
        screen.harness.get_by_label_contains("The handler ignored");
        let message = screen.harness.get_by_label("Message");
        assert_eq!(message.value().as_deref(), Some("unsent words"));
    }
}

#[test]
fn a_repeated_stage_is_neither_published_nor_repainted() {
    let shared = Arc::new(Mutex::new(Shared::default()));
    let ctx = egui::Context::default();
    let repaints = Arc::new(AtomicUsize::new(0));
    ctx.set_request_repaint_callback({
        let repaints = repaints.clone();
        move |_| {
            repaints.fetch_add(1, Ordering::SeqCst);
        }
    });
    let observe = observer(shared.clone(), ctx.clone());
    // egui folds repaint requests until a pass runs, so each count follows one.
    let pass = || {
        ctx.run_ui(egui::RawInput::default(), |_| {})
            .textures_delta
            .clear();
    };
    let changed = shared.lock().unwrap().changed.clone();
    // The first passes of a context ask for repaints of their own.
    for _ in 0..3 {
        pass();
    }
    let woken = |stage| {
        let notified = changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        observe(stage);
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        std::future::Future::poll(notified, &mut cx).is_ready()
    };
    assert!(woken(LaunchStage::WaitingForLaunch));
    pass();
    let first = repaints.load(Ordering::SeqCst);
    assert!(first > 0);
    // The launcher polls every 20 ms and says the same each time.
    for _ in 0..50 {
        assert!(!woken(LaunchStage::WaitingForLaunch));
        pass();
    }
    assert_eq!(repaints.load(Ordering::SeqCst), first);
    assert!(woken(LaunchStage::BeforeSpawn));
    assert!(repaints.load(Ordering::SeqCst) > first);
}
