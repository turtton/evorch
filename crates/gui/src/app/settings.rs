//! The settings modal that hosts the Providers, Routing and Agent roles tabs.
//!
//! One modal session spans the time the modal is open. Opening it from closed starts a
//! session; opening it again, or picking a tab, only changes the visible tab. Drafts live
//! in the tab models and survive tab switches; closing the modal discards all of them.
//! Config is read when a session starts, when a tab is first shown and when a save finishes.

use super::WorkbenchState;
use crate::model::provider_settings::{ProfileEditor, ProviderSettingsModel};
use crate::model::role_settings::RoleSettingsModel;
use crate::model::routing_settings::RoutingSettingsModel;
use crate::model::tasks::AgentRunSource;
use crate::panes::provider_settings::{ProviderSettingsAction, provider_settings_body};
use crate::panes::role_settings::{RoleSettingsAction, role_settings_body};
use crate::panes::routing_settings::{RoutingSettingsAction, routing_settings_body};
use crate::panes::settings::{SettingsModalAction, SettingsTab, settings_modal};

/// Which tab is visible, and which tabs hold a draft seeded in this session.
#[derive(Debug, Default)]
pub(super) struct SettingsSession {
    /// `None` while the modal is closed.
    pub(super) tab: Option<SettingsTab>,
    pub(super) routing_seeded: bool,
    pub(super) roles_seeded: bool,
}

enum BodyAction {
    Provider(ProviderSettingsAction),
    Routing(RoutingSettingsAction),
    Roles(RoleSettingsAction),
}

impl<S: AgentRunSource> WorkbenchState<S> {
    /// The tab shown by the settings modal, `None` while it is closed.
    pub const fn settings_tab(&self) -> Option<SettingsTab> {
        self.settings.tab
    }

    /// Whether the Routing or Agent roles draft differs from what it was seeded or saved with.
    pub fn settings_has_unsaved_edits(&self) -> bool {
        (self.settings.routing_seeded && self.routing_settings.is_dirty())
            || (self.settings.roles_seeded && self.role_settings.is_dirty())
    }

    /// Opens the modal on `tab`. `profile` picks the role profile when a session starts.
    pub fn open_settings_tab(&mut self, tab: SettingsTab, profile: Option<&str>) {
        if self.settings_save_in_progress() {
            return;
        }
        if self.settings.tab.is_none() {
            self.begin_settings_session();
            self.settings.tab = Some(tab);
            self.seed_tab(tab, profile);
            return;
        }
        let already_shown = self.settings.tab == Some(tab);
        if !already_shown {
            self.select_settings_tab(tab);
        }
        if let Some(profile) = profile
            && self.session_profile().as_deref() != Some(profile)
            && !self.settings_has_unsaved_edits()
            && let Err(error) = self.select_settings_profile(profile)
        {
            self.role_settings.error = Some(error);
        }
    }

    /// Opens the modal on Providers with `model` as the resident provider model, keeping its
    /// open editor. Used to start from a prepared fixture instead of a config on disk.
    pub fn open_provider_settings_with(&mut self, model: ProviderSettingsModel) {
        self.begin_settings_session();
        self.provider_settings = model;
        self.settings.tab = Some(SettingsTab::Providers);
    }

    /// Builder form of [`Self::open_provider_settings_with`].
    #[must_use]
    pub fn with_open_provider_settings(mut self, model: ProviderSettingsModel) -> Self {
        self.open_provider_settings_with(model);
        self
    }

    /// Opens the modal on Agent roles with `model` as an already seeded draft.
    pub fn open_role_settings_with(&mut self, model: RoleSettingsModel) {
        self.begin_settings_session();
        self.role_settings = model;
        self.settings.roles_seeded = true;
        self.settings.tab = Some(SettingsTab::AgentRoles);
    }

    /// Opens the modal on Routing with `model` as an already seeded draft.
    pub fn open_routing_settings_with(&mut self, model: RoutingSettingsModel) {
        self.begin_settings_session();
        self.routing_settings = model;
        self.settings.routing_seeded = true;
        self.settings.tab = Some(SettingsTab::Routing);
    }
    /// Shows `tab` keeping every draft. Moving by hand drops the way back to roles.
    pub fn select_settings_tab(&mut self, tab: SettingsTab) {
        if self.settings_save_in_progress() || self.settings.tab.is_none() {
            return;
        }
        if self.settings.tab == Some(tab) {
            return;
        }
        self.routing_settings.origin_role_settings = false;
        self.settings.tab = Some(tab);
        self.ensure_tab_seeded(tab);
    }

    /// Closes the modal and discards the drafts of every tab.
    pub fn close_settings_modal(&mut self) {
        if self.settings_save_in_progress() {
            return;
        }
        self.dismiss_settings_modal();
    }

    /// Closes the modal without the save check: another settings dialog takes over.
    pub(super) fn dismiss_settings_modal(&mut self) {
        if self.settings.tab.is_none() {
            return;
        }
        self.settings = SettingsSession::default();
        self.provider_settings.editor = None;
        self.provider_settings.error = None;
        self.provider_settings.confirm_delete = None;
        self.routing_settings.conflict = None;
        self.routing_settings.origin_role_settings = false;
    }

    pub(super) fn begin_settings_session(&mut self) {
        self.dismiss_other_settings();
        self.settings = SettingsSession::default();
        self.provider_settings.error = None;
        self.provider_settings.editor = None;
        self.provider_settings.confirm_delete = None;
        self.routing_settings.conflict = None;
        self.routing_settings.origin_role_settings = false;
    }

    fn dismiss_other_settings(&mut self) {
        self.sandbox_settings.open = false;
        self.agent_features.open = false;
        self.self_improvement_settings.open = false;
        self.storage_settings.open = false;
        self.close_theme_settings();
    }

    /// The role profile both draft tabs edit, once one of them is seeded.
    pub(super) fn session_profile(&self) -> Option<String> {
        if self.settings.routing_seeded {
            Some(self.routing_settings.profiles.selected.clone())
        } else if self.settings.roles_seeded {
            Some(self.role_settings.profiles.selected.clone())
        } else {
            None
        }
    }

    fn seed_tab(&mut self, tab: SettingsTab, profile: Option<&str>) {
        match tab {
            SettingsTab::Providers => {}
            SettingsTab::Routing => self.seed_routing_tab(profile),
            SettingsTab::AgentRoles => self.seed_roles_tab(profile),
        }
    }

    /// Seeds `tab` on the first visit of the session.
    pub(super) fn ensure_tab_seeded(&mut self, tab: SettingsTab) {
        let seeded = match tab {
            SettingsTab::Providers => return,
            SettingsTab::Routing => self.settings.routing_seeded,
            SettingsTab::AgentRoles => self.settings.roles_seeded,
        };
        if !seeded {
            let profile = self.session_profile();
            self.seed_tab(tab, profile.as_deref());
        }
    }

    pub(super) fn seed_routing_tab(&mut self, profile: Option<&str>) {
        match self.load_profile_settings(profile) {
            Ok((config, picker)) => {
                self.routing_settings =
                    crate::model::routing_settings::RoutingSettingsModel::seed_from_config(&config);
                self.routing_settings.profiles = picker;
                self.settings.routing_seeded = true;
            }
            Err(error) => self.routing_settings.validation_error = Some(error),
        }
    }

    pub(super) fn seed_roles_tab(&mut self, profile: Option<&str>) {
        match self.load_profile_settings(profile) {
            Ok((config, picker)) => {
                self.seed_role_settings(&config, picker);
                self.settings.roles_seeded = true;
            }
            Err(error) => self.role_settings.error = Some(error),
        }
    }

    /// Switches every seeded tab to `profile`, reloading it from disk.
    pub(super) fn select_settings_profile(&mut self, profile: &str) -> Result<(), String> {
        let config = config::Config::load_unresolved(&self.user_settings_options())
            .map_err(|error| error.to_string())?;
        self.reseed_settings_tabs(&config, Some(profile), None);
        Ok(())
    }

    /// Re-seeds each tab seeded in this session, plus `own`, from `config` (drafts are
    /// replaced). Callers only do this while no tab holds unsaved edits.
    pub(super) fn reseed_settings_tabs(
        &mut self,
        config: &config::Config,
        preferred: Option<&str>,
        own: Option<SettingsTab>,
    ) {
        let (view, picker) = self.profile_settings_from(config, preferred);
        if self.settings.routing_seeded || own == Some(SettingsTab::Routing) {
            let origin = self.routing_settings.origin_role_settings;
            self.routing_settings =
                crate::model::routing_settings::RoutingSettingsModel::seed_from_config(&view);
            self.routing_settings.profiles = picker.clone();
            self.routing_settings.origin_role_settings = origin;
        }
        if self.settings.roles_seeded || own == Some(SettingsTab::AgentRoles) {
            self.seed_role_settings(&view, picker);
        }
    }

    /// After any in-app write: every seeded tab learns the saved routes, providers and
    /// bindings, and no draft is touched. A Routing draft that an in-app write made stale
    /// cannot be saved over it.
    pub(super) fn refresh_settings_tabs_after_save(&mut self) {
        if !self.settings.routing_seeded && !self.settings.roles_seeded {
            return;
        }
        let profile = self.session_profile();
        let (view, picker) = match self.load_profile_settings(profile.as_deref()) {
            Ok(loaded) => loaded,
            Err(error) => {
                tracing::warn!(%error, "settings tabs not refreshed after a save");
                return;
            }
        };
        if self.settings.routing_seeded {
            if !self.routing_settings.is_dirty() {
                let origin = self.routing_settings.origin_role_settings;
                self.routing_settings =
                    crate::model::routing_settings::RoutingSettingsModel::seed_from_config(&view);
                self.routing_settings.profiles = picker;
                self.routing_settings.origin_role_settings = origin;
            } else {
                let unchanged = view.routing.routes == self.routing_settings.base_routes;
                self.routing_settings.refresh_references(&view);
                if !unchanged {
                    self.routing_settings.conflict = Some(
                        "Routing was changed by another save while this draft had unsaved edits. \
                         Saving is disabled so the newer routes are not overwritten; close settings \
                         and open them again to start from the saved routing."
                            .to_owned(),
                    );
                }
            }
        }
        if self.settings.roles_seeded {
            self.role_settings.refresh_references(&view);
            self.refresh_role_previews();
        }
    }

    pub(super) fn render_settings_modal(&mut self, ctx: &egui::Context) {
        let Some(tab) = self.settings.tab else {
            return;
        };
        let busy = self.settings_save_in_progress();
        let locked = self.settings_has_unsaved_edits();
        let (modal_action, body_action) = settings_modal(ctx, tab, busy, |ui| match tab {
            SettingsTab::Providers => {
                provider_settings_body(ui, &mut self.provider_settings, &self.codex_auth)
                    .map(BodyAction::Provider)
            }
            SettingsTab::Routing => {
                if self.settings.routing_seeded {
                    routing_settings_body(ui, &mut self.routing_settings, locked)
                        .map(BodyAction::Routing)
                } else {
                    unavailable_tab(ui, self.routing_settings.validation_error.as_deref());
                    None
                }
            }
            SettingsTab::AgentRoles => {
                if self.settings.roles_seeded {
                    role_settings_body(ui, &mut self.role_settings, locked).map(BodyAction::Roles)
                } else {
                    unavailable_tab(ui, self.role_settings.error.as_deref());
                    None
                }
            }
        });
        match modal_action {
            Some(SettingsModalAction::SelectTab(tab)) => self.select_settings_tab(tab),
            Some(SettingsModalAction::Close) => self.close_settings_modal(),
            None => {}
        }
        match body_action {
            Some(BodyAction::Provider(action)) => self.apply_provider_action(action),
            Some(BodyAction::Routing(RoutingSettingsAction::Save)) => {
                self.submit_routing_settings();
            }
            Some(BodyAction::Routing(RoutingSettingsAction::Profile(action))) => {
                self.apply_routing_profile_action(action);
            }
            Some(BodyAction::Roles(RoleSettingsAction::Save)) => self.submit_role_settings(),
            Some(BodyAction::Roles(RoleSettingsAction::CreateRoute(logical))) => {
                self.open_routing_settings_prefill(&logical);
            }
            Some(BodyAction::Roles(RoleSettingsAction::Profile(action))) => {
                self.apply_role_profile_action(action);
            }
            None => {}
        }
    }

    fn apply_provider_action(&mut self, action: ProviderSettingsAction) {
        match action {
            ProviderSettingsAction::Save => self.submit_provider_settings(),
            ProviderSettingsAction::Delete(name) => self.delete_provider_settings(name),
            ProviderSettingsAction::Cancel => self.close_provider_settings(),
            ProviderSettingsAction::StartCodexLogin => self.start_codex_login(),
            ProviderSettingsAction::StartSubscriptionLogin => self.start_subscription_login(),
            ProviderSettingsAction::CompleteSubscriptionLogin => self.complete_subscription_login(),
            ProviderSettingsAction::CancelSubscriptionLogin => {
                if let Some(editor) = self.provider_settings.subscription_mut() {
                    editor.cancel_login();
                }
            }
            ProviderSettingsAction::RefreshModels => self
                .provider_settings
                .start_models_fetch_with_store(self.credential_store.clone()),
        }
    }

    /// Whether the Providers tab is editing a profile rather than listing them.
    pub fn provider_editor_active(&self) -> bool {
        matches!(
            self.provider_settings.editor,
            Some(
                ProfileEditor::OpenAiCompatible(_)
                    | ProfileEditor::Subscription(_)
                    | ProfileEditor::Codex(_)
            )
        )
    }
}

/// Shown instead of a tab whose config could not be read; editing would save an empty draft.
fn unavailable_tab(ui: &mut egui::Ui, error: Option<&str>) {
    crate::theme::widgets::reason_label(
        ui,
        crate::theme::tokens::palette().ERROR_FG,
        error.unwrap_or("This tab could not be loaded. Close settings and open them again."),
    );
}
