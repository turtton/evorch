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
        if let Some(error) = &self.read_error {
            ui.colored_label(ui.visuals().warn_fg_color, error);
        }
        let phase = self.panel.status().map(|s| s.status.phase);
        match self.panel.status() {
            Some(status) => {
                ui.label(format!(
                    "状態：{}（リビジョン {}）",
                    text::phase(status.status.phase),
                    status.status.revision
                ));
                for run in approval_blocked_runs(&status.status) {
                    ui.colored_label(ui.visuals().warn_fg_color, text::approval_blocker(run));
                }
                if let Some(reason) = status.status.cancel_reason {
                    ui.colored_label(ui.visuals().warn_fg_color, text::cancel_reason(reason));
                }
                if status.audit.denial_persistence_failures.get() > 0 {
                    ui.colored_label(
                        ui.visuals().warn_fg_color,
                        "警告：拒否の監査記録を保存できなかったものがあります",
                    );
                }
            }
            None => {
                ui.label("状態を読み込んでいます。");
            }
        }
        let mut blocker_choice = None;
        if let Some(blockers) = self.panel.blockers()
            && !blockers.blockers.is_empty()
        {
            ui.label("再起動を待たせているシェル：");
            for (index, blocker) in blockers.blockers.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(format!("run {}", blocker.run));
                    if ui.small_button("このシェルを中断…").clicked() {
                        blocker_choice = Some(index);
                    }
                });
            }
        }
        if let Some(index) = blocker_choice
            && let Err(error) = self.panel.begin_interrupt(&BlockerSelector::Index(index))
        {
            self.panel
                .push_notice(format!("{error}。何も送っていません"));
        }
        match self.panel.confirming().cloned() {
            Some(Confirming::Restart(binding)) => {
                ui.separator();
                ui.label(format!(
                    "コアを再起動します（対象 {}、コア世代 {}）。実行中の作業は安全点で止まり、新しいコアで続きます。",
                    binding.target_id.as_str(),
                    binding.expected_core_epoch.get()
                ));
                self.confirm_buttons(ui, ctx, "再起動する");
            }
            Some(Confirming::Interrupt { run, .. }) => {
                ui.separator();
                ui.label(format!(
                    "run {run} のシェルを中断します。実行中のコマンドは止まります。"
                ));
                self.confirm_buttons(ui, ctx, "中断する");
            }
            None => {
                ui.horizontal(|ui| {
                    let idle = !self.panel.restart_in_flight()
                        && phase.is_none_or(|p| {
                            p == workbench_protocol::namespaces::lifecycle::RestartPhase::Running
                        });
                    if ui
                        .add_enabled(
                            idle && self.panel.targets().is_some(),
                            egui::Button::new("コアを再起動…"),
                        )
                        .clicked()
                        && let Err(error) = self.panel.begin_restart(admin_id("restart"))
                    {
                        self.panel
                            .push_notice(format!("{error}。何も送っていません"));
                    }
                    if phase.is_some_and(cancellable)
                        && ui.button("再起動を取り消す").clicked()
                        && let Some(request) = self.panel.cancel(admin_id("cancel"))
                    {
                        self.send(ctx, request);
                    }
                });
            }
        }
        for notice in self.panel.notices() {
            ui.label(notice);
        }
        if !self.panel.notices().is_empty() && ui.small_button("通知を消す").clicked() {
            self.panel.clear_notices();
        }
        if let Some(count) = self.panel.undisposed()
            && count > 0
        {
            ui.label(format!(
                "未処分のグラフ {count} 件：`evorch core graphs list` で確認してください。"
            ));
        }
    }
}

impl RestartFeature {
    fn confirm_buttons(&mut self, ui: &mut egui::Ui, ctx: &mut FeatureContext<'_>, label: &str) {
        ui.horizontal(|ui| {
            if ui.button(label).clicked()
                && let Some(request) = self.panel.accept(admin_id("interrupt"))
            {
                self.send(ctx, request);
                self.refresh_due = true;
            }
            if ui.button("やめる").clicked() {
                self.panel.dismiss_confirmation();
            }
        });
    }
}
