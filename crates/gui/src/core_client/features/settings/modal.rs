//! The Routing and Agent roles tabs of the settings modal: drafts seeded from the core's
//! settings document, each saved as a whole document that quotes the revision it was made
//! from.
//!
//! A new snapshot of another revision leaves a clean draft reseeded; a dirty one is rebased
//! when its own section (the routes, or the agents) is what the draft started from, and is
//! otherwise refused a save, as legacy refuses a Routing draft whose routes another save
//! changed.
use crate::model::role_profiles::{RoleProfileAction, RoleProfilePicker};
use crate::model::role_settings::RoleSettingsModel;
use crate::model::routing_settings::RoutingSettingsModel;
use crate::model::settings_document::SettingsDocument;
use crate::model::settings_tabs::ROUTING_CONFLICT;
use crate::panes::role_profiles::role_profile_header;
use crate::panes::role_settings::{RoleSettingsAction, role_settings_hosted_body};
use crate::panes::routing_settings::{RoutingSettingsAction, routing_settings_hosted_body};
use crate::panes::settings::{SettingsTab, unavailable_tab};
use workbench_protocol::namespaces::settings::{SettingsRevision, SettingsSnapshot};

pub(super) const STALE_DRAFT: &str =
    "The settings were changed elsewhere. This draft is checked against them when they arrive.";
const ROLES_CONFLICT: &str = "Agent roles were changed by another save while this draft had unsaved edits. \
     Saving is disabled so the newer bindings are not overwritten; close settings and open them \
     again to start from the saved roles.";

pub(super) struct Seeded<M> {
    pub model: M,
    pub document: SettingsDocument,
    pub based_on: SettingsRevision,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ModalJob {
    Routing,
    Roles,
    Created(String),
    Deleted(String),
}

impl ModalJob {
    pub(super) fn notice(&self) -> String {
        match self {
            Self::Routing => "Routing settings updated. Reload models to apply them.".into(),
            Self::Roles => "Agent role settings updated. Reload models to apply them.".into(),
            Self::Created(name) => {
                format!("Role profile '{name}' created. Reload models to apply it.")
            }
            Self::Deleted(name) => {
                format!("Role profile '{name}' deleted. Reload models to apply it.")
            }
        }
    }
}

pub(super) struct ModalSave {
    pub job: ModalJob,
    pub text: String,
    pub expected_revision: SettingsRevision,
}

#[derive(Default)]
struct Unavailable {
    revision: Option<SettingsRevision>,
    reason: String,
}

pub(super) struct Modal {
    pub tab: SettingsTab,
    pub routing: Option<Seeded<RoutingSettingsModel>>,
    pub roles: Option<Seeded<RoleSettingsModel>>,
    roles_conflict: Option<String>,
    unavailable: Unavailable,
}

fn parse(snapshot: &SettingsSnapshot) -> Result<SettingsDocument, String> {
    if let Some(problem) = snapshot.problem {
        return Err(super::problem_label(problem).to_owned());
    }
    SettingsDocument::parse(&snapshot.document)
        .map_err(|error| format!("The settings file cannot be edited here: {error}"))
}

impl Modal {
    pub(super) fn new(tab: SettingsTab) -> Self {
        Self {
            tab,
            routing: None,
            roles: None,
            roles_conflict: None,
            unavailable: Unavailable::default(),
        }
    }

    pub(super) fn dirty(&self) -> bool {
        self.routing
            .as_ref()
            .is_some_and(|routing| routing.model.is_dirty())
            || self
                .roles
                .as_ref()
                .is_some_and(|roles| roles.model.is_dirty())
    }

    fn session_profile(&self) -> Option<String> {
        self.routing
            .as_ref()
            .map(|routing| routing.model.profiles.selected.clone())
            .or_else(|| {
                self.roles
                    .as_ref()
                    .map(|roles| roles.model.profiles.selected.clone())
            })
    }

    pub(super) fn select(&mut self, tab: SettingsTab) {
        if let Some(routing) = &mut self.routing {
            routing.model.origin_role_settings = false;
        }
        self.tab = tab;
    }

    /// `Err(None)` while no snapshot has arrived yet; `Err(Some(reason))` when the tab cannot be seeded.
    fn ensure_seeded(&mut self, snapshot: Option<&SettingsSnapshot>) -> Result<(), Option<String>> {
        let seeded = match self.tab {
            SettingsTab::Providers => return Ok(()),
            SettingsTab::Routing => self.routing.is_some(),
            SettingsTab::AgentRoles => self.roles.is_some(),
        };
        if seeded {
            return Ok(());
        }
        let Some(snapshot) = snapshot else {
            return Err(None);
        };
        if self.unavailable.revision.as_ref() == Some(&snapshot.revision) {
            return Err(Some(self.unavailable.reason.clone()));
        }
        let document = match parse(snapshot) {
            Ok(document) => document,
            Err(reason) => {
                self.unavailable = Unavailable {
                    revision: Some(snapshot.revision.clone()),
                    reason: reason.clone(),
                };
                return Err(Some(reason));
            }
        };
        let profile = self.session_profile();
        let based_on = snapshot.revision.clone();
        match self.tab {
            SettingsTab::Routing => {
                self.routing = Some(Seeded {
                    model: document.seed_routing(profile.as_deref()),
                    document,
                    based_on,
                });
            }
            SettingsTab::AgentRoles => {
                self.roles = Some(Seeded {
                    model: document.seed_roles(profile.as_deref()),
                    document,
                    based_on,
                });
                self.roles_conflict = None;
            }
            SettingsTab::Providers => {}
        }
        Ok(())
    }

    pub(super) fn follow(&mut self, snapshot: &SettingsSnapshot) {
        let moved = |based_on: &SettingsRevision| based_on != &snapshot.revision;
        let routing_moved = self.routing.as_ref().is_some_and(|r| moved(&r.based_on));
        let roles_moved = self.roles.as_ref().is_some_and(|r| moved(&r.based_on));
        if !routing_moved && !roles_moved {
            return;
        }
        let parsed = parse(snapshot);
        if routing_moved && let Some(routing) = &mut self.routing {
            let dirty = routing.model.is_dirty();
            match &parsed {
                Ok(document) if !dirty => {
                    document.reseed_routing(&mut routing.model);
                    rebase(routing, document, snapshot);
                }
                Ok(document) if document.routes_unchanged(&routing.model) => {
                    document.refresh_routing(&mut routing.model);
                    routing.model.conflict = None;
                    rebase(routing, document, snapshot);
                }
                Ok(document) => {
                    document.refresh_routing(&mut routing.model);
                    routing.model.conflict = Some(ROUTING_CONFLICT.to_owned());
                }
                Err(_) if !dirty => self.routing = None,
                Err(_) => routing.model.conflict = Some(ROUTING_CONFLICT.to_owned()),
            }
        }
        if roles_moved && let Some(roles) = &mut self.roles {
            let dirty = roles.model.is_dirty();
            match &parsed {
                Ok(document) if !dirty => {
                    roles.model = document.seed_roles(Some(&roles.model.profiles.selected));
                    self.roles_conflict = None;
                    rebase(roles, document, snapshot);
                }
                Ok(document) if document.agents_unchanged(&roles.model) => {
                    document.refresh_roles(&mut roles.model);
                    self.roles_conflict = None;
                    rebase(roles, document, snapshot);
                }
                Ok(document) => {
                    document.refresh_roles(&mut roles.model);
                    self.roles_conflict = Some(ROLES_CONFLICT.to_owned());
                }
                Err(_) if !dirty => self.roles = None,
                Err(_) => self.roles_conflict = Some(ROLES_CONFLICT.to_owned()),
            }
        }
    }

    /// A draft already rebased past the revision `save` quoted has been checked against the
    /// newer file, and is left as it is.
    pub(super) fn stale(&mut self, save: &ModalSave) {
        let quoted = |based_on: &SettingsRevision| based_on == &save.expected_revision;
        match save.job {
            ModalJob::Routing => {
                if let Some(routing) = &mut self.routing
                    && quoted(&routing.based_on)
                {
                    routing.model.conflict = Some(STALE_DRAFT.to_owned());
                }
            }
            ModalJob::Roles => {
                if self
                    .roles
                    .as_ref()
                    .is_some_and(|roles| quoted(&roles.based_on))
                {
                    self.roles_conflict = Some(STALE_DRAFT.to_owned());
                }
            }
            ModalJob::Created(_) | ModalJob::Deleted(_) => {}
        }
    }

    pub(super) fn saved(&mut self, job: &ModalJob, text: &str, revision: Option<SettingsRevision>) {
        let (Some(revision), Ok(document)) = (revision, SettingsDocument::parse(text)) else {
            // The written revision is unknown: the drafts are seeded again from the next
            // snapshot instead of being saved over a revision they cannot quote.
            match job {
                ModalJob::Routing => self.routing = None,
                ModalJob::Roles => self.roles = None,
                ModalJob::Created(_) | ModalJob::Deleted(_) => {
                    self.routing = None;
                    self.roles = None;
                }
            }
            return;
        };
        match job {
            ModalJob::Routing => {
                let Some(routing) = &mut self.routing else {
                    return;
                };
                let return_to_roles = routing.model.origin_role_settings;
                document.carry_routing_save(
                    &mut routing.model,
                    self.roles.as_mut().map(|roles| &mut roles.model),
                );
                routing.document = document.clone();
                routing.based_on = revision.clone();
                if let Some(roles) = &mut self.roles {
                    // A snapshot of this very save may have been followed first, and flagged
                    // the renames it carried into the agents as another save's change.
                    if document.agents_unchanged(&roles.model) {
                        self.roles_conflict = None;
                    }
                    roles.document = document;
                    roles.based_on = revision;
                }
                if return_to_roles && self.tab == SettingsTab::Routing {
                    self.tab = SettingsTab::AgentRoles;
                }
            }
            ModalJob::Roles => {
                if let Some(roles) = &mut self.roles {
                    roles.model = document.seed_roles(Some(&roles.model.profiles.selected));
                    roles.document = document.clone();
                    roles.based_on = revision.clone();
                }
                self.roles_conflict = None;
                if let Some(routing) = &mut self.routing {
                    if routing.model.is_dirty() {
                        document.refresh_routing(&mut routing.model);
                    } else {
                        document.reseed_routing(&mut routing.model);
                    }
                    routing.document = document;
                    routing.based_on = revision;
                }
            }
            ModalJob::Created(name) => self.reseed(&document, &revision, Some(name)),
            ModalJob::Deleted(_) => self.reseed(&document, &revision, None),
        }
    }

    fn reseed(
        &mut self,
        document: &SettingsDocument,
        revision: &SettingsRevision,
        profile: Option<&str>,
    ) {
        if let Some(routing) = &mut self.routing {
            let origin = routing.model.origin_role_settings;
            routing.model = document.seed_routing(profile);
            routing.model.origin_role_settings = origin;
            routing.document = document.clone();
            routing.based_on = revision.clone();
        }
        if let Some(roles) = &mut self.roles {
            roles.model = document.seed_roles(profile);
            roles.document = document.clone();
            roles.based_on = revision.clone();
        }
        self.roles_conflict = None;
    }

    pub(super) fn body(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: Option<&SettingsSnapshot>,
        busy: bool,
    ) -> Option<ModalSave> {
        match self.ensure_seeded(snapshot) {
            Ok(()) => {}
            Err(None) => {
                ui.label("Loading settings…");
                return None;
            }
            Err(Some(reason)) => {
                unavailable_tab(ui, Some(&reason));
                return None;
            }
        }
        let locked = self.dirty();
        match self.tab {
            SettingsTab::Providers => None,
            SettingsTab::Routing => {
                let routing = self.routing.as_mut()?;
                let action = ui
                    .add_enabled_ui(!busy, |ui| {
                        routing_settings_hosted_body(ui, &mut routing.model, locked)
                    })
                    .inner?;
                match action {
                    RoutingSettingsAction::Save => self.save_routing(),
                    RoutingSettingsAction::Profile(action) => self.profile(action, locked),
                }
            }
            SettingsTab::AgentRoles => {
                if let Some(conflict) = &self.roles_conflict {
                    crate::theme::widgets::reason_label(
                        ui,
                        crate::theme::tokens::palette().ERROR_FG,
                        conflict,
                    );
                }
                let roles = self.roles.as_mut()?;
                let action = ui
                    .add_enabled_ui(!busy, |ui| {
                        role_settings_hosted_body(ui, &mut roles.model, locked)
                    })
                    .inner?;
                match action {
                    RoleSettingsAction::Save => self.save_roles(),
                    RoleSettingsAction::CreateRoute(logical) => {
                        self.create_route(&logical);
                        None
                    }
                    RoleSettingsAction::Profile(action) => self.profile(action, locked),
                }
            }
        }
    }

    /// The profile control of the Model Settings header. It follows the section: Routing and
    /// Agent Roles edit the same profile (a change reseeds both), and Providers shows it muted,
    /// since a provider does not belong to a profile.
    pub(super) fn header_profile(&mut self, ui: &mut egui::Ui, busy: bool) -> Option<ModalSave> {
        let locked = self.dirty();
        let fallback = self.session_profile();
        let picker = match self.tab {
            SettingsTab::Routing => self
                .routing
                .as_mut()
                .map(|seeded| &mut seeded.model.profiles),
            SettingsTab::AgentRoles => self.roles.as_mut().map(|seeded| &mut seeded.model.profiles),
            SettingsTab::Providers => None,
        };
        let action = match picker {
            Some(picker) => role_profile_header(ui, picker, !busy, locked),
            None => {
                let mut shown = RoleProfilePicker::default();
                if let Some(profile) = fallback {
                    shown.names = vec![profile.clone()];
                    shown.selected = profile;
                }
                role_profile_header(ui, &mut shown, false, false);
                None
            }
        };
        self.profile(action?, locked)
    }

    fn save_routing(&mut self) -> Option<ModalSave> {
        let routing = self.routing.as_mut()?;
        if routing.model.conflict.is_some() {
            return None;
        }
        match routing.document.routing_text(&routing.model) {
            Ok(text) => {
                routing.model.validation_error = None;
                Some(ModalSave {
                    job: ModalJob::Routing,
                    text,
                    expected_revision: routing.based_on.clone(),
                })
            }
            Err(error) => {
                routing.model.validation_error = Some(error);
                None
            }
        }
    }

    fn save_roles(&mut self) -> Option<ModalSave> {
        if self.roles_conflict.is_some() {
            return None;
        }
        let roles = self.roles.as_mut()?;
        match roles.document.agents_text(&roles.model) {
            Ok(text) => {
                roles.model.error = None;
                Some(ModalSave {
                    job: ModalJob::Roles,
                    text,
                    expected_revision: roles.based_on.clone(),
                })
            }
            Err(error) => {
                roles.model.error = Some(error);
                None
            }
        }
    }

    fn create_route(&mut self, logical: &str) {
        let Some(roles) = &self.roles else {
            return;
        };
        let agents = roles.model.agents.clone();
        if self.routing.is_none() {
            self.routing = Some(Seeded {
                model: roles
                    .document
                    .seed_routing(Some(&roles.model.profiles.selected)),
                document: roles.document.clone(),
                based_on: roles.based_on.clone(),
            });
        }
        self.tab = SettingsTab::Routing;
        if let Some(routing) = &mut self.routing {
            routing.model.add_prefill_route(logical, &agents);
        }
    }

    fn profile(&mut self, action: RoleProfileAction, locked: bool) -> Option<ModalSave> {
        if locked {
            return None;
        }
        let (document, based_on, picker) = match (self.tab, &self.routing, &self.roles) {
            (SettingsTab::Routing, Some(seeded), _) => {
                (&seeded.document, &seeded.based_on, &seeded.model.profiles)
            }
            (_, _, Some(seeded)) => (&seeded.document, &seeded.based_on, &seeded.model.profiles),
            _ => return None,
        };
        let result = match &action {
            RoleProfileAction::Select(name) => {
                let (document, based_on) = (document.clone(), based_on.clone());
                self.reseed(&document, &based_on, Some(name));
                return None;
            }
            RoleProfileAction::Create(name) => picker.validate_new_name(name).and_then(|name| {
                document
                    .new_profile_text(&picker.selected, &name)
                    .map(|text| (ModalJob::Created(name), text))
            }),
            RoleProfileAction::Delete(name) => document
                .without_profile_text(name)
                .map(|text| (ModalJob::Deleted(name.clone()), text)),
        };
        let expected_revision = based_on.clone();
        match result {
            Ok((job, text)) => Some(ModalSave {
                job,
                text,
                expected_revision,
            }),
            Err(error) => {
                match (self.tab, &mut self.routing, &mut self.roles) {
                    (SettingsTab::Routing, Some(routing), _) => {
                        routing.model.validation_error = Some(error);
                    }
                    (_, _, Some(roles)) => roles.model.error = Some(error),
                    _ => {}
                }
                None
            }
        }
    }
}

fn rebase<M>(seeded: &mut Seeded<M>, document: &SettingsDocument, snapshot: &SettingsSnapshot) {
    seeded.document = document.clone();
    seeded.based_on = snapshot.revision.clone();
}
