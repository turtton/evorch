use super::WorkbenchState;
use super::ownership_status::{OwnershipSnapshot, OwnershipStatus};
use crate::model::tasks::AgentRunSource;
use crate::theme::tokens::palette;
use runtime::ownership::{OwnerHost, OwnerPermit, OwnershipError, RegistryError};
use std::sync::Arc;
use std::time::Instant;

#[derive(Debug)]
pub(super) struct OwnershipActionError {
    pub operation: &'static str,
    pub detail: String,
}

const SHUTDOWN: &str = "Shutdown";

impl<S: AgentRunSource> WorkbenchState<S> {
    pub(super) fn chat_permit(&mut self, thread: &str) -> Result<Option<OwnerPermit>, String> {
        let Some(host) = &self.ownership else {
            return Ok(None);
        };
        let result = match host.owned_permit(thread) {
            Ok(permit) => Ok(permit),
            Err(RegistryError::Absent) => {
                crate::runtime_sink::finish_chat_start(host, thread, host.start(thread))
            }
            Err(RegistryError::Ownership(OwnershipError::Fenced)) => {
                host.attach(thread).and_then(|owner| host.claim(&owner))
            }
            Err(error) => Err(error),
        };
        match result {
            Ok(permit) => {
                self.readonly_threads.remove(thread);
                self.ownership_action_error = None;
                Ok(Some(permit))
            }
            Err(RegistryError::Ownership(
                OwnershipError::OwnerResponsive | OwnershipError::Fenced,
            )) => Err("このスレッドは別ウィンドウが write mode で保持中です".into()),
            Err(error) => Err(format!("write mode を取得できません: {error}")),
        }
    }

    pub fn with_ownership(mut self, host: Arc<OwnerHost>) -> Self {
        self.ownership = Some(host);
        self
    }

    pub fn thread_writable(&self) -> bool {
        match (&self.ownership, &self.sidebar.active_thread) {
            (Some(host), Some(thread)) => {
                !self.readonly_threads.contains(&thread.to_string())
                    && host.owned_permit(&thread.to_string()).is_ok()
            }
            (Some(_), None) => false,
            (None, _) => true,
        }
    }

    pub(super) fn footer_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.menu_button("⚙", |ui| {
                if ui.button("Theme").clicked() {
                    self.open_theme_settings();
                    ui.close();
                }
                if ui.button("Providers").clicked() {
                    self.open_provider_settings();
                    ui.close();
                }
                if ui.button("Agent roles").clicked() {
                    self.open_role_settings();
                    ui.close();
                }
                if ui.button("Routing").clicked() {
                    self.open_routing_settings();
                    ui.close();
                }
            })
            .response
            .on_hover_text("Workbench settings");
            crate::panes::quota_footer::quota_footer(ui, &self.telemetry.quota);
            if self.telemetry.kimi_quota.configured() {
                ui.separator();
                crate::panes::quota_footer::kimi_quota_footer(ui, &self.telemetry.kimi_quota.state);
            }
        });
    }

    pub(super) fn ownership_ui(&mut self, ui: &mut egui::Ui) {
        let Some(host) = self.ownership.clone() else {
            return;
        };
        let thread = self.sidebar.active_thread.as_ref().map(ToString::to_string);
        if self.ownership_status.select_thread(thread.as_deref())
            && self
                .ownership_action_error
                .as_ref()
                .is_some_and(|error| error.operation != SHUTDOWN)
        {
            self.ownership_action_error = None;
        }
        if let Some(thread) = thread {
            let result = probe_ownership(&host, &thread, self.readonly_threads.contains(&thread));
            let now = Instant::now();
            self.ownership_status.observe(&thread, result, now);
            self.ownership_controls_ui(ui, &host, &thread, now);
        }
        self.ownership_shutdown_ui(ui, &host);
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(250));
    }

    fn ownership_controls_ui(
        &mut self,
        ui: &mut egui::Ui,
        host: &OwnerHost,
        thread: &str,
        now: Instant,
    ) {
        ui.horizontal_wrapped(|ui| {
            ownership_status_ui(ui, &self.ownership_status, now);
            match self.ownership_status.snapshot.clone() {
                Some(OwnershipSnapshot::Owned { owner, writable }) => {
                    if ui.button("Attach (read-only)").clicked() {
                        let result = host.attach(thread);
                        if result.is_ok() {
                            self.readonly_threads.insert(thread.to_owned());
                        }
                        self.record_ownership_action("Attach (read-only)", result);
                    }
                    if !writable && ui.button("Claim").clicked() {
                        // The snapshot is only an expected generation, never authorization.
                        let result = host.owned_permit(thread).or_else(|_| host.claim(&owner));
                        if result.is_ok() {
                            self.readonly_threads.remove(thread);
                        }
                        self.record_ownership_action("Claim", result);
                    }
                }
                Some(OwnershipSnapshot::Unowned) if ui.button("Start").clicked() => {
                    self.record_ownership_action("Start", host.start(thread));
                }
                Some(OwnershipSnapshot::Unowned) | None => {}
            }
            if let Some(error) = &self.ownership_action_error
                && error.operation != SHUTDOWN
            {
                ownership_action_error_ui(ui, error);
            }
        });
    }

    fn record_ownership_action<T>(
        &mut self,
        operation: &'static str,
        result: Result<T, RegistryError>,
    ) {
        self.ownership_action_error = result.err().map(|error| OwnershipActionError {
            operation,
            detail: error.to_string(),
        });
    }

    fn ownership_shutdown_ui(&mut self, ui: &mut egui::Ui, host: &OwnerHost) {
        let ctx = ui.ctx().clone();
        if ctx.input(|input| input.viewport().close_requested()) && !self.close_in_flight {
            self.shutdown_requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
        if self.shutdown_requested && !self.shutdown_confirmed && !self.close_in_flight {
            match host.has_active_turns() {
                Ok(false) => self.quiesce_ownership(host, &ctx),
                Ok(true) => {}
                Err(error) => self.record_ownership_action::<()>(SHUTDOWN, Err(error)),
            }
        }
        if self.shutdown_confirmed && !self.close_in_flight {
            self.quiesce_ownership(host, &ctx);
        }
        if self.shutdown_requested && !self.close_in_flight {
            egui::Window::new("Active thread ownership").collapsible(false).show(&ctx, |ui| {
                ui.label("Closing stops this process. Drain active tools and checkpoint before releasing ownership. Other windows may claim after release.");
                if self.shutdown_confirmed {
                    ui.label("Waiting for active tools and checkpoint…");
                } else {
                    if ui.button("Drain and close").clicked() {
                        self.quiesce_ownership(host, &ctx);
                    }
                    if ui.button("Keep open").clicked() {
                        self.shutdown_requested = false;
                        self.shutdown_confirmed = false;
                        if self.ownership_action_error.as_ref().is_some_and(|error| error.operation == SHUTDOWN) {
                            self.ownership_action_error = None;
                        }
                    }
                }
                if let Some(error) = &self.ownership_action_error
                    && error.operation == SHUTDOWN
                {
                    ownership_action_error_ui(ui, error);
                }
            });
        }
    }

    fn quiesce_ownership(&mut self, host: &OwnerHost, ctx: &egui::Context) {
        let result = host.begin_quiesce();
        match &result {
            Ok(false) => {
                self.close_in_flight = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Ok(true) => self.shutdown_confirmed = true,
            Err(_) => {}
        }
        self.record_ownership_action(SHUTDOWN, result);
    }
}

fn probe_ownership(
    host: &OwnerHost,
    thread: &str,
    readonly: bool,
) -> Result<OwnershipSnapshot, RegistryError> {
    let owner = host.attach(thread)?;
    // Unlike thread_writable (the fail-closed live authorization check), display
    // probes must distinguish a confirmed read-only result from a failed read.
    let writable = if readonly {
        false
    } else {
        match host.owned_permit(thread) {
            Ok(_) => true,
            Err(RegistryError::Ownership(OwnershipError::Fenced)) => false,
            Err(error) => return Err(error),
        }
    };
    Ok(OwnershipSnapshot::Owned { owner, writable })
}

fn short_owner_id(id: &str) -> String {
    let mut short: String = id.chars().take(5).collect();
    if id.chars().count() > 5 {
        short.push('…');
    }
    short
}

fn ownership_status_ui(ui: &mut egui::Ui, status: &OwnershipStatus, now: Instant) {
    let display = status.display(now);
    let mut tooltip = match &status.snapshot {
        Some(OwnershipSnapshot::Owned { owner, .. }) => format!(
            "Owner {} · generation {} · {:?} · {}",
            owner.lease.owner_id, owner.lease.generation, owner.state, display.access
        ),
        Some(OwnershipSnapshot::Unowned) => "No process owner · read-only".to_owned(),
        None => "checking ownership".to_owned(),
    };
    if let Some(failure) = &status.failure {
        tooltip.push_str("\n操作権限の確認を再試行中...\n");
        tooltip.push_str(&failure.detail);
    }
    match &status.snapshot {
        Some(OwnershipSnapshot::Owned { owner, .. }) => {
            ui.label(format!(
                "Owner {} · generation {} · {:?} ·",
                short_owner_id(&owner.lease.owner_id),
                owner.lease.generation,
                owner.state
            ))
            .on_hover_text(&tooltip);
        }
        Some(OwnershipSnapshot::Unowned) => {
            ui.label("No process owner ·").on_hover_text(&tooltip);
        }
        None => {}
    }
    let access = if display.warning {
        egui::RichText::new(format!("{} ⚠", display.access)).color(palette().WARNING_FG)
    } else {
        egui::RichText::new(display.access)
    };
    ui.label(access).on_hover_text(tooltip);
}

fn ownership_action_error_ui(ui: &mut egui::Ui, error: &OwnershipActionError) {
    ui.colored_label(
        palette().ERROR_FG,
        format!("{}: {}", error.operation, error.detail),
    );
}

#[cfg(test)]
#[path = "ownership_tests.rs"]
mod tests;
