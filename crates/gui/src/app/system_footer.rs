//! Starts the system sampler on the first footer frame and lays the readings out at its right end.

use std::path::PathBuf;

use super::WorkbenchState;
use crate::model::system_stats::SystemSampler;
use crate::model::tasks::AgentRunSource;

#[derive(Default)]
pub(super) struct SystemFooter {
    /// The directory holding the database; its filesystem is the one reported.
    storage_directory: Option<PathBuf>,
    sampler: Option<SystemSampler>,
    started: bool,
}

impl SystemFooter {
    /// Repaint-policy fixtures must not start the machine's periodic sampler.
    #[cfg(test)]
    pub(super) fn without_sampling() -> Self {
        Self {
            started: true,
            ..Default::default()
        }
    }

    pub(super) fn set_storage_directory(&mut self, directory: Option<PathBuf>) {
        self.storage_directory = directory;
    }

    fn latest(
        &mut self,
        ctx: &egui::Context,
    ) -> Option<&crate::model::system_stats::SystemSnapshot> {
        if !self.started {
            self.started = true;
            let ctx = ctx.clone();
            // One coalesced repaint per reading; nothing repaints while no reading arrives.
            self.sampler = SystemSampler::start(self.storage_directory.clone(), move || {
                crate::repaint::coalesced(&ctx);
            });
        }
        self.sampler.as_mut()?.latest()
    }
}

impl<S: AgentRunSource> WorkbenchState<S> {
    pub(super) fn footer_ui(&mut self, ui: &mut egui::Ui) {
        let snapshot = self.system_footer.latest(ui.ctx()).cloned();
        crate::panes::system_footer::with_right_stats(ui, snapshot.as_ref(), |ui| {
            self.footer_content_ui(ui);
        });
    }
}
