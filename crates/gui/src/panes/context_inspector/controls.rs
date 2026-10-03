//! Toolbar of the Context tab: view switcher, run shape editors and run picker.

use runtime::Role;

use super::{ContextInspectorPane, InspectorMode, RunChoice, Selection};
use crate::model::context_inspector::{PreviewSpec, ROLES, categories_for};
use crate::theme::icons;

impl ContextInspectorPane {
    /// Returns whether Refresh was clicked.
    pub(super) fn toolbar(&mut self, ui: &mut egui::Ui, runs: &[RunChoice]) -> bool {
        let mut refresh = false;
        ui.horizontal_wrapped(|ui| {
            for mode in InspectorMode::ALL {
                if crate::panes::usage::charts::segment(ui, self.mode == mode, mode.label())
                    && self.mode != mode
                {
                    self.mode = mode;
                    self.selection = Selection::None;
                }
            }
            ui.separator();
            refresh = ui
                .button(icons::with_icon(icons::ARROWS_CLOCKWISE, "Refresh"))
                .on_hover_text("Recompose from the current configuration and files")
                .clicked();
        });
        ui.horizontal_wrapped(|ui| match self.mode {
            InspectorMode::Preview => {
                if spec_editor(ui, "preview", &mut self.spec) {
                    self.selection = Selection::None;
                }
            }
            InspectorMode::Run => self.run_picker(ui, runs),
            InspectorMode::Compare => {
                spec_editor(ui, "compare-left", &mut self.left);
                ui.label("vs");
                spec_editor(ui, "compare-right", &mut self.right);
            }
        });
        refresh
    }

    fn run_picker(&mut self, ui: &mut egui::Ui, runs: &[RunChoice]) {
        if runs.is_empty() {
            ui.label("No runs in this thread yet.");
            return;
        }
        let selected = runs
            .iter()
            .find(|choice| choice.run_id == self.run)
            .map_or(self.run.as_str(), |choice| choice.label.as_str());
        egui::ComboBox::from_id_salt("context-run")
            .selected_text(selected)
            .show_ui(ui, |ui| {
                for choice in runs {
                    if ui
                        .selectable_label(self.run == choice.run_id, &choice.label)
                        .clicked()
                    {
                        self.run = choice.run_id.clone();
                        self.selection = Selection::None;
                    }
                }
            });
    }
}

/// Role, category and run-shape controls. Returns whether anything changed.
fn spec_editor(ui: &mut egui::Ui, id: &str, spec: &mut PreviewSpec) -> bool {
    let before = spec.clone();
    egui::ComboBox::from_id_salt((id, "role"))
        .selected_text(spec.role.name())
        .show_ui(ui, |ui| {
            for role in ROLES {
                if ui
                    .selectable_label(spec.role == role, role.name())
                    .clicked()
                {
                    *spec = PreviewSpec::new(role);
                }
            }
        });
    let categories = categories_for(spec.role);
    if !categories.is_empty() {
        egui::ComboBox::from_id_salt((id, "category"))
            .selected_text(spec.category.as_deref().unwrap_or("no category"))
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut spec.category, None, "no category");
                for category in categories {
                    ui.selectable_value(&mut spec.category, Some(category.to_owned()), category);
                }
            });
    }
    ui.checkbox(&mut spec.child, "Delegated")
        .on_hover_text("Started by a parent run; delegated runs cannot escalate");
    if spec.role == Role::Worker && !spec.child {
        ui.checkbox(&mut spec.conversation, "Conversation")
            .on_hover_text("Composer conversation mode: a root worker with scoped web access");
    } else {
        spec.conversation = false;
    }
    ui.checkbox(&mut spec.isolated, "Isolated worktree");
    if spec.conversation {
        spec.category = Some("conversation".into());
    } else if spec.category.as_deref() == Some("conversation") {
        spec.category = None;
    }
    *spec != before
}
