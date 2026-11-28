//! The settings modal that hosts the Providers, Routing and Agent roles tabs.

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

/// Shown instead of a tab whose config could not be read; editing would save an empty draft.
pub fn unavailable_tab(ui: &mut egui::Ui, error: Option<&str>) {
    crate::theme::widgets::reason_label(
        ui,
        crate::theme::tokens::palette().ERROR_FG,
        error.unwrap_or("This tab could not be loaded. Close settings and open them again."),
    );
}
