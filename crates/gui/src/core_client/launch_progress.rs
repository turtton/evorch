//! What the window says while it waits for the core for the first time: the
//! launcher's stages, then the session being read, until the first projection is
//! in. It is client-only state: nothing here is a protocol endpoint, and the
//! core's own maintenance progress is not shown.
//!
//! * The splash covers the workspace only until this process has installed its
//!   first projection (`Shared::projection`). A later loss or reconnect keeps the
//!   conversations and drafts in view and speaks through the footer and the
//!   restart panel, as before.
//! * It closes when `begin_session` installs the projection, not when the socket
//!   connects, so a core that answers but cannot yet be read is still "reading".
//! * A failure is shown with the existing `Shared::status` and `status_detail`;
//!   its only action is the existing `Reconnect`.
//! * The launcher reports the same stage on every poll. A repeat changes nothing
//!   and wakes nobody; the observer takes the lock for a field write and does no I/O.
use std::{
    borrow::Cow,
    sync::{Arc, Mutex},
};

use core_launcher::LaunchStage;
use egui::{Align, Id, Layout, Order, Rect, Sense, UiBuilder};

use super::{Shared, connection::Action, shell::Effect};
use crate::theme::{
    text::{h3, muted},
    tokens::*,
    widgets::primary_button,
};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Nothing is being waited for: a window that was handed a ready `Shared`.
    #[default]
    Idle,
    /// A connection, which may start the core, has been asked for.
    Connecting,
    /// The launcher reported where it is.
    Launching(LaunchStage),
    /// Connected; the session (capabilities, snapshot) is being read.
    ReadingState,
    /// The attempt failed; `Shared::status` says how.
    Failed,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LaunchProgress {
    phase: Phase,
}

impl LaunchProgress {
    /// The state of a window whose connection actor was just started.
    pub fn starting() -> Self {
        Self {
            phase: Phase::Connecting,
        }
    }

    /// Each setter returns whether anything changed, so a repeat is not published.
    fn set(&mut self, phase: Phase) -> bool {
        let changed = self.phase != phase;
        self.phase = phase;
        changed
    }

    pub fn connecting(&mut self) -> bool {
        self.set(Phase::Connecting)
    }

    pub fn stage(&mut self, stage: LaunchStage) -> bool {
        self.set(Phase::Launching(stage))
    }

    pub fn reading_state(&mut self) -> bool {
        self.set(Phase::ReadingState)
    }

    pub fn failed(&mut self) -> bool {
        self.set(Phase::Failed)
    }

    pub fn is_failed(&self) -> bool {
        self.phase == Phase::Failed
    }

    /// Whether the splash is over the workspace: something is awaited and this
    /// process has not installed a projection yet.
    pub fn covers(&self, projection: u64) -> bool {
        projection == 0 && self.phase != Phase::Idle
    }

    /// What is being waited for; `None` when nothing is, or the attempt failed.
    pub fn headline(&self) -> Option<Cow<'static, str>> {
        Some(match self.phase {
            Phase::Idle | Phase::Failed => return None,
            Phase::Connecting => "Connecting to the core…".into(),
            Phase::Launching(LaunchStage::WaitingForLaunch) => {
                "Waiting for another client to start the core…".into()
            }
            Phase::Launching(LaunchStage::BeforeSpawn) => "Starting the core…".into(),
            Phase::Launching(LaunchStage::Spawned(pid)) => {
                format!("The core started (pid {pid}). Waiting for it to answer…").into()
            }
            Phase::Launching(LaunchStage::LifetimeBusy) => {
                "Waiting for the existing core to become ready…".into()
            }
            Phase::Launching(LaunchStage::AlreadyRunning) => {
                "A core is already running. Connecting to it…".into()
            }
            Phase::ReadingState => "Reading the core's state…".into(),
        })
    }
}

/// Apply `change` to the launch progress, and publish only if it changed.
pub(super) fn note(
    shared: &Arc<Mutex<Shared>>,
    ctx: &egui::Context,
    change: impl FnOnce(&mut LaunchProgress) -> bool,
) {
    let changed = {
        let mut state = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !change(&mut state.launch) {
            return;
        }
        state.changed.clone()
    };
    changed.notify_waiters();
    ctx.request_repaint();
}

/// The launcher's observer: it is called on every poll of the launcher.
pub(super) fn observer(
    shared: Arc<Mutex<Shared>>,
    ctx: egui::Context,
) -> impl Fn(LaunchStage) + Send + Sync {
    move |stage| note(&shared, &ctx, |launch| launch.stage(stage))
}

/// Draw the splash over `rect` (the workspace, below the footer) when it covers
/// it. Only a failure has an action: the existing `Reconnect`.
pub(super) fn show(ctx: &egui::Context, rect: Rect, shared: &Shared, effects: &mut Vec<Effect>) {
    if !shared.launch.covers(shared.projection) {
        return;
    }
    let fill = ctx.global_style().visuals.panel_fill;
    egui::Area::new(Id::new("core-launch-splash"))
        .order(Order::Middle)
        .fixed_pos(rect.min)
        .constrain(false)
        .show(ctx, |ui| {
            ui.painter().rect_filled(rect, 0.0, fill);
            // Nothing below it is usable yet: keep pointer input from reaching it.
            ui.allocate_rect(rect, Sense::click_and_drag());
            ui.scope_builder(UiBuilder::new().max_rect(rect), |ui| {
                ui.with_layout(Layout::top_down(Align::Center), |ui| {
                    ui.add_space(rect.height() * 0.3);
                    match shared.launch.headline() {
                        Some(headline) => {
                            ui.spinner();
                            ui.add_space(SP_2);
                            ui.label(h3(headline));
                        }
                        None => {
                            // As in the footer, the full error is only on hover.
                            let status = ui.label(h3(&shared.status));
                            if let Some(detail) = &shared.status_detail {
                                status.on_hover_text(detail);
                            }
                            ui.add_space(SP_2);
                            if primary_button(ui, "Reconnect").clicked() {
                                effects.push(Effect::send(Action::Reconnect));
                            }
                            ui.label(muted(
                                "Reconnect may start the core within its start budget.",
                            ));
                        }
                    }
                });
            });
        });
}
