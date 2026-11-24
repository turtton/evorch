//! The settings modal that hosts the Providers, Routing and Agent roles tabs.

use crate::theme::{text::h3, tokens::*, widgets::surface_frame};

/// The tabs of the settings modal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    Providers,
    Routing,
    AgentRoles,
}

impl SettingsTab {
    pub const ALL: [Self; 3] = [Self::Providers, Self::Routing, Self::AgentRoles];

    /// The heading shown above the tab strip while this tab is active.
    pub const fn title(self) -> &'static str {
        match self {
            Self::Providers => "Provider settings",
            Self::Routing => "Routing settings",
            Self::AgentRoles => "Agent role settings",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Providers => "Providers",
            Self::Routing => "Routing",
            Self::AgentRoles => "Agent roles",
        }
    }
}

/// What the modal frame itself asked for; tab bodies report their own actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsModalAction {
    SelectTab(SettingsTab),
    Close,
}

/// Draws the shared frame: the tab strip, `body` for the active tab and one close button.
/// While `busy` (a save is running) the tab strip and the close button are disabled.
pub fn settings_modal<A>(
    ctx: &egui::Context,
    active: SettingsTab,
    busy: bool,
    body: impl FnOnce(&mut egui::Ui) -> Option<A>,
) -> (Option<SettingsModalAction>, Option<A>) {
    let mut modal_action = None;
    let mut body_action = None;
    egui::Modal::new(egui::Id::new("settings"))
        .backdrop_color(palette().OVERLAY)
        .frame(surface_frame(palette().SURFACE_RAISED))
        .show(ctx, |ui| {
            ui.set_width(
                (ctx.viewport_rect().width() * 0.6).min(PROVIDER_MODAL_MAX_WIDTH) - SP_4 * 4.0,
            );
            ui.spacing_mut().item_spacing = egui::vec2(SP_2, SP_2);
            ui.label(h3(active.title()));
            ui.add_enabled_ui(!busy, |ui| {
                ui.horizontal_wrapped(|ui| {
                    crate::theme::widgets::readable_selection(ui);
                    for tab in SettingsTab::ALL {
                        let selected = tab == active;
                        if ui.selectable_label(selected, tab.label()).clicked() && !selected {
                            modal_action = Some(SettingsModalAction::SelectTab(tab));
                        }
                    }
                });
            });
            ui.separator();
            body_action = body(ui);
            ui.separator();
            ui.add_enabled_ui(!busy, |ui| {
                if ui.button("Close").clicked() {
                    modal_action = Some(SettingsModalAction::Close);
                }
            });
        });
    (modal_action, body_action)
}

/// Shown instead of a tab whose config could not be read; editing would save an empty draft.
pub fn unavailable_tab(ui: &mut egui::Ui, error: Option<&str>) {
    crate::theme::widgets::reason_label(
        ui,
        crate::theme::tokens::palette().ERROR_FG,
        error.unwrap_or("This tab could not be loaded. Close settings and open them again."),
    );
}

#[cfg(test)]
mod tests {
    use super::{SettingsTab, settings_modal};
    use crate::theme::style::{ThemePreset, install_preset};
    use crate::theme::tokens::palette;

    // Given: the settings modal in Graphite, whose selection stroke is its fill
    // When: the Routing tab is active
    // Then: its label is drawn in the accent's foreground, so it reads on the accent
    #[test]
    fn the_active_tab_label_reads_on_the_accent_in_graphite() {
        let mut harness = egui_kittest::Harness::new_ui(|ui| {
            install_preset(ui.ctx(), ThemePreset::Graphite);
            settings_modal::<()>(ui.ctx(), SettingsTab::Routing, false, |_| None);
        });
        harness.run_steps(2);
        let label = harness
            .output()
            .shapes
            .iter()
            .find_map(|clipped| match &clipped.shape {
                egui::Shape::Text(text) if text.galley.text() == "Routing" => {
                    Some(text.override_text_color.unwrap_or(text.fallback_color))
                }
                _ => None,
            });
        assert_eq!(label, Some(palette().ACCENT_FG));
    }
}
