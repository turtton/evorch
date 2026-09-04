//! Project sidebar pane stub.

use workspace_ui::SidebarState;

pub fn sidebar_pane(ui: &mut egui::Ui, _sidebar: &SidebarState) {
    ui.heading("Projects");
    ui.label("Project and thread navigation");
}
