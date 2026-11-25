//! Desktop feature of the public restart: what the core says about it, the
//! confirmation a restart or a shell interrupt needs, and why a restart was
//! refused. The model is [`crate::core_client::restart::panel::RestartPanel`];
//! this file is only its wiring to the shared connection.
//!
//! * Shown only while the connected core advertises `safe_restart.v1`, which it
//!   does only to a verified local peer and only when its composition is complete.
//! * A command leaves only from the dialog the person accepted, once. A lost
//!   reply is read back by operation id after the reconnect; nothing is resent.
//! * While this window's own restart request has not ended, a lost connection is
//!   treated as the restart (see [`Feature::planned_restart`]): the actor waits
//!   for the successor connect-only instead of starting another core.
//! * The recovery of what a restart left behind (Q1/Q2) is the management CLI's:
//!   `evorch core graphs list` and `evorch core quarantine list`.
use super::{
    Availability, Feature, FeatureContext, FeatureError, FeatureId, FeatureReply, Visibility,
};
use crate::core_client::restart::{
    panel::{Confirming, PanelRequest, RestartPanel},
    text,
};
use crate::theme::icons;
use crate::theme::text::{medium, muted};
use crate::theme::tokens::palette;
use crate::theme::widgets::{
    accessible, compact_row, ghost, primary_button, reason_label, surface_frame,
};
use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};
use workbench_client::{
    ClientError,
    restart::{
        intent::{BlockerSelector, cancellable},
        reconnect::PlannedEvidence,
        view::approval_blocked_runs,
    },
};
use workbench_protocol::{
    ErrorCode, RetryAdvice, WorkbenchError,
    commands::Command,
    events::UiEvent,
    handshake::Capability,
    namespaces::lifecycle::{AdminId, LifecycleCommand, LifecycleEvent, restart::RestartCommand},
    queries::QueryReply,
    wire::InstanceEpoch,
};

/// How often a restart this window asked for is read back while it is open.
const POLL: Duration = Duration::from_millis(500);
const NOTICES_KEPT: usize = 12;

#[derive(Default)]
pub struct RestartFeature {
    panel: RestartPanel,
    next_tag: u64,
    /// Tags of commands in flight; everything else is a read.
    commands: BTreeSet<u64>,
    /// The subset of `commands` that are the restart request itself.
    restart_requests: BTreeSet<u64>,
    refresh_due: bool,
    reconcile_due: bool,
    last_poll: Option<Instant>,
    read_error: Option<String>,
}

fn admin_id(kind: &str) -> AdminId {
    AdminId::new(format!("gui-{kind}-{}", uuid::Uuid::new_v4())).expect("a uuid is an admin id")
}

fn client_error(error: &FeatureError) -> ClientError {
    ClientError::Server(WorkbenchError::new(
        error.code.unwrap_or(ErrorCode::Internal),
        error.message.clone(),
        error.retry.unwrap_or(RetryAdvice::Never),
    ))
}

impl RestartFeature {
    fn tag(&mut self) -> u64 {
        self.next_tag += 1;
        self.next_tag
    }

    fn send(&mut self, ctx: &mut FeatureContext<'_>, request: PanelRequest) {
        let tag = self.tag();
        match request {
            PanelRequest::Query(query) => ctx.query(tag, query),
            PanelRequest::Command(command) => {
                if matches!(
                    command,
                    Command::Lifecycle(LifecycleCommand::Restart(RestartCommand::Request(_)))
                ) {
                    self.restart_requests.insert(tag);
                }
                self.commands.insert(tag);
                ctx.command(tag, command);
            }
        }
    }

    fn refresh(&mut self, ctx: &mut FeatureContext<'_>) {
        self.last_poll = Some(Instant::now());
        for request in self.panel.refresh_queries() {
            self.send(ctx, request);
        }
        for request in self.panel.reconcile_queries() {
            self.send(ctx, request);
        }
    }
}

impl Feature for RestartFeature {
    fn id(&self) -> FeatureId {
        FeatureId::Restart
    }

    fn title(&self) -> &'static str {
        "Restart"
    }

    fn availability(&self) -> Availability {
        Availability::Implemented {
            required: &[Capability::SafeRestart],
        }
    }

    fn reset(&mut self) {
        self.panel.on_new_session();
        self.commands.clear();
        self.restart_requests.clear();
        self.read_error = None;
        self.refresh_due = true;
        self.reconcile_due = self.panel.has_uncertain();
    }

    fn disconnected(&mut self) {
        self.refresh_due = false;
    }

    fn event(&mut self, event: &UiEvent) {
        if matches!(
            event,
            UiEvent::Lifecycle(
                LifecycleEvent::CoreStatusChanged { .. } | LifecycleEvent::AdminChanged { .. }
            )
        ) {
            self.refresh_due = true;
        }
    }

    fn maintain(&mut self, ctx: &mut FeatureContext<'_>, _visibility: Visibility) {
        if !ctx.connected() || !ctx.has(Capability::SafeRestart) {
            return;
        }
        let polling =
            self.panel.restart_in_flight() && self.last_poll.is_none_or(|at| at.elapsed() >= POLL);
        if std::mem::take(&mut self.refresh_due) || polling {
            self.refresh(ctx);
        }
        if std::mem::take(&mut self.reconcile_due) {
            for request in self.panel.reconcile_queries() {
                self.send(ctx, request);
            }
        }
        if let Some(request) = self.panel.take_undisposed_query() {
            self.send(ctx, request);
        }
    }

    fn reply(&mut self, tag: u64, reply: FeatureReply) {
        let command = self.commands.remove(&tag);
        let restart_request = self.restart_requests.remove(&tag);
        match reply {
            FeatureReply::Query(Ok(QueryReply::Lifecycle(lifecycle))) => {
                self.read_error = None;
                self.panel
                    .on_reply(&QueryReply::Lifecycle(lifecycle), Instant::now());
            }
            FeatureReply::Query(Ok(_)) => {}
            FeatureReply::Query(Err(error)) => {
                // A refused read is shown in one place and replaced, not piled up.
                self.read_error = Some(if error.outcome_unknown {
                    text::outcome_unknown()
                } else {
                    text::failure(&client_error(&error))
                });
            }
            FeatureReply::Command(Ok(reply)) => {
                self.panel.on_command_reply(&reply);
                self.refresh_due = true;
            }
            FeatureReply::Command(Err(error)) => {
                if restart_request && !error.outcome_unknown {
                    self.panel.restart_not_accepted();
                }
                self.panel.on_request_failure(
                    command || self.commands.is_empty(),
                    &client_error(&error),
                    error.outcome_unknown,
                );
                self.refresh_due = true;
            }
        }
        self.panel.trim_notices(NOTICES_KEPT);
    }

    fn planned_restart(&self, epoch: InstanceEpoch, now: Instant) -> Option<PlannedEvidence> {
        self.panel.planned_restart(epoch, now)
    }

    fn render(&mut self, ui: &mut egui::Ui, ctx: &mut FeatureContext<'_>) {
        if self.panel.restart_in_flight() {
            ui.ctx().request_repaint_after(POLL);
        }
        let warn = palette().WARNING_FG;
        if let Some(error) = &self.read_error {
            reason_label(ui, warn, error);
        }
        let phase = self.panel.status().map(|s| s.status.phase);
        match self.panel.status() {
            Some(status) => {
                ui.label(medium(format!(
                    "Status: {} (revision {})",
                    text::phase(status.status.phase),
                    status.status.revision
                )));
                for run in approval_blocked_runs(&status.status) {
                    reason_label(ui, warn, text::approval_blocker(run));
                }
                if let Some(reason) = status.status.cancel_reason {
                    reason_label(ui, warn, text::cancel_reason(reason));
                }
                if status.audit.denial_persistence_failures.get() > 0 {
                    reason_label(
                        ui,
                        warn,
                        "Warning: some denial audit records could not be stored",
                    );
                }
            }
            None => {
                ui.label(muted("Loading the restart status…"));
            }
        }
        let mut blocker_choice = None;
        if let Some(blockers) = self.panel.blockers()
            && !blockers.blockers.is_empty()
        {
            ui.label(muted("Shells holding up the restart:"));
            for (index, blocker) in blockers.blockers.iter().enumerate() {
                ui.push_id(index, |ui| {
                    compact_row(ui, false, |ui| {
                        ui.label(muted(format!("run {}", blocker.run)));
                        if ui.add(ghost("Interrupt this shell…")).clicked() {
                            blocker_choice = Some(index);
                        }
                    });
                });
            }
        }
        if let Some(index) = blocker_choice
            && let Err(error) = self.panel.begin_interrupt(&BlockerSelector::Index(index))
        {
            self.panel
                .push_notice(format!("{error}. Nothing was sent."));
        }
        match self.panel.confirming().cloned() {
            Some(Confirming::Restart(binding)) => {
                let question = format!(
                    "Restart the core (target {}, core epoch {}). Running work stops at a safe point and continues in the new core.",
                    binding.target_id.as_str(),
                    binding.expected_core_epoch.get()
                );
                self.confirmation(ui, ctx, &question, "Restart now");
            }
            Some(Confirming::Interrupt { run, .. }) => {
                let question =
                    format!("Interrupt the shell of run {run}. The running command stops.");
                self.confirmation(ui, ctx, &question, "Interrupt");
            }
            None => {
                ui.horizontal(|ui| {
                    let idle = !self.panel.restart_in_flight()
                        && phase.is_none_or(|p| {
                            p == workbench_protocol::namespaces::lifecycle::RestartPhase::Running
                        });
                    let restart = ui.add_enabled(
                        idle && self.panel.targets().is_some(),
                        egui::Button::new(icons::with_icon(
                            icons::ARROW_CLOCKWISE,
                            "Restart core…",
                        )),
                    );
                    accessible(&restart, "Restart core…");
                    if restart.clicked()
                        && let Err(error) = self.panel.begin_restart(admin_id("restart"))
                    {
                        self.panel
                            .push_notice(format!("{error}. Nothing was sent."));
                    }
                    if phase.is_some_and(cancellable)
                        && ui.add(ghost("Cancel restart")).clicked()
                        && let Some(request) = self.panel.cancel(admin_id("cancel"))
                    {
                        self.send(ctx, request);
                    }
                });
            }
        }
        for notice in self.panel.notices() {
            ui.label(muted(notice));
        }
        if !self.panel.notices().is_empty() && ui.add(ghost("Clear notices")).clicked() {
            self.panel.clear_notices();
        }
        if let Some(count) = self.panel.undisposed()
            && count > 0
        {
            ui.label(muted(format!(
                "{count} undisposed graph(s): check them with `evorch core graphs list`."
            )));
        }
    }
}

impl RestartFeature {
    fn confirmation(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &mut FeatureContext<'_>,
        question: &str,
        label: &str,
    ) {
        surface_frame(palette().SURFACE_RAISED).show(ui, |ui| {
            ui.label(question);
            ui.horizontal(|ui| {
                if primary_button(ui, label).clicked()
                    && let Some(request) = self.panel.accept(admin_id("interrupt"))
                {
                    self.send(ctx, request);
                    self.refresh_due = true;
                }
                if ui.add(ghost("Cancel")).clicked() {
                    self.panel.dismiss_confirmation();
                }
            });
        });
    }
}

#[cfg(test)]
mod tests;
