use std::sync::Arc;
use runtime::ownership::{OwnerHost, RegistryError};
use super::WorkbenchState;
use crate::model::tasks::AgentRunSource;

impl<S: AgentRunSource> WorkbenchState<S> {
    pub fn with_ownership(mut self, host: Arc<OwnerHost>) -> Self {
        self.ownership = Some(host);
        self
    }

    pub fn thread_writable(&self) -> bool {
        match (&self.ownership, &self.sidebar.active_thread) {
            (Some(host), Some(thread)) => !self.readonly_threads.contains(&thread.to_string()) && host.owned_permit(&thread.to_string()).is_ok(),
            (Some(_), None) => false,
            (None, _) => true,
        }
    }

    pub(super) fn ownership_ui(&mut self, ui: &mut egui::Ui) {
        let Some(host) = self.ownership.clone() else { return; };
        if let Some(thread) = self.sidebar.active_thread.clone() {
            let thread = thread.to_string();
            ui.horizontal_wrapped(|ui| {
                match host.attach(thread.as_str()) {
                    Ok(owner) => {
                        let writable = self.thread_writable();
                        ui.label(format!("Owner {} · generation {} · {:?} · {}", owner.lease.owner_id, owner.lease.generation, owner.state, if writable { "write" } else { "read-only" }));
                        if ui.button("Attach (read-only)").clicked() {
                            self.readonly_threads.insert(thread.clone());
                            self.ownership_error = host.attach(thread.as_str()).err().map(|error| error.to_string());
                        }
                        if !writable && ui.button("Claim").clicked() {
                            let result = host.owned_permit(&thread).or_else(|_| host.claim(&owner));
                            match result {
                                Ok(_) => { self.readonly_threads.remove(&thread); self.ownership_error = None; }
                                Err(error) => self.ownership_error = Some(error.to_string()),
                            }
                        }
                    }
                    Err(RegistryError::Absent) => {
                        ui.label("No process owner · read-only");
                        if ui.button("Start").clicked() {
                            self.ownership_error = host.start(thread.as_str()).err().map(|error| error.to_string());
                        }
                    }
                    Err(error) => { self.ownership_error = Some(error.to_string()); }
                }
            });
        }
        if let Some(error) = &self.ownership_error { ui.label(error); }
        let ctx = ui.ctx().clone();
        if ctx.input(|input| input.viewport().close_requested()) {
            self.shutdown_requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
        if self.shutdown_requested && !self.shutdown_confirmed {
            let has_active_turns = host.has_active_turns().unwrap_or(true);
            if !has_active_turns {
                match host.quiesce() {
                    Ok(false) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                    Ok(true) => self.shutdown_confirmed = true,
                    Err(error) => self.ownership_error = Some(error.to_string()),
                }
            }
        }
        if self.shutdown_requested && !self.shutdown_confirmed {
            egui::Window::new("Active thread ownership").collapsible(false).show(&ctx, |ui| {
                ui.label("Closing stops this process. Drain active tools and checkpoint before releasing ownership. Other windows may claim after release.");
                if ui.button("Drain and close").clicked() {
                    match host.quiesce() {
                        Ok(_) => self.shutdown_confirmed = true,
                        Err(error) => self.ownership_error = Some(error.to_string()),
                    }
                }
                if ui.button("Keep open").clicked() { self.shutdown_requested = false; }
            });
        }
        if self.shutdown_confirmed {
            match host.quiesce() {
                Ok(false) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                Ok(true) => { ui.label("Waiting for active tools and checkpoint…"); }
                Err(error) => self.ownership_error = Some(error.to_string()),
            }
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(250));
    }
}
