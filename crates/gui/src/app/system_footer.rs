//! Lays the system readings out at the right end of the footer.

pub(super) use crate::panes::system_footer::SystemFooter;

use super::WorkbenchState;
use crate::model::tasks::AgentRunSource;

impl<S: AgentRunSource> WorkbenchState<S> {
    pub(super) fn footer_ui(&mut self, ui: &mut egui::Ui) {
        let snapshot = self.system_footer.latest(ui.ctx()).cloned();
        crate::panes::system_footer::with_right_stats(ui, snapshot.as_ref(), |ui| {
            self.footer_content_ui(ui);
        });
    }
}
