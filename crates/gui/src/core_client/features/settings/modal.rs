//! The Routing and Agent Roles sections of Model Settings: drafts seeded from the core's
//! settings document, saved one route or one role at a time as a whole document that quotes
//! the revision it was made from.
//!
//! A save writes only the selected route (a rename and the bindings that name it included)
//! or the selected role: the edits of the others stay drafts, neither saved nor discarded. A
//! document of another revision (this client's own save, or another writer's) is taken in by
//! `adopt_routing` / `adopt_roles`: a route or role without edits follows the file, and one
//! with edits is kept as drafted unless the file changed that very route or role under it,
//! which is flagged and is not saved over the newer one.
use super::roles::{RolesAction, RolesUi, roles_body};
use super::routing::{RouteAction, RoutingUi, routing_body};
use crate::model::role_profiles::{RoleProfileAction, RoleProfilePicker};
use crate::model::role_settings::{ROLES, RoleInfo, RoleSettingsModel};
use crate::model::routing_settings::RoutingSettingsModel;
use crate::model::settings_document::SettingsDocument;
use crate::panes::role_profiles::role_profile_header;
use crate::panes::settings::{SettingsTab, unavailable_tab};
use std::collections::BTreeMap;
use workbench_protocol::namespaces::settings::{SettingsRevision, SettingsSnapshot};

pub(super) const STALE_DRAFT: &str =
    "The settings were changed elsewhere. This draft is checked against them when they arrive.";
pub(super) const PARSE_CONFLICT: &str = "The settings file can no longer be read here, so this draft cannot be checked against it. \
     Saving is disabled; close settings and open them again.";

pub(super) struct Seeded<M> {
    pub model: M,
    pub document: SettingsDocument,
    pub based_on: SettingsRevision,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ModalJob {
    /// A route saved under `name`, which it had as `key` in the draft. `moved_refs`: it was a
    /// saved route renamed, so the file's bindings that named it were renamed with it.
    Route {
        key: String,
        name: String,
        moved_refs: bool,
    },
    RouteDeleted(String),
    /// The role at this address in the settings file.
    Role(&'static str),
    Created(String),
    Deleted(String),
}

impl ModalJob {
    pub(super) fn notice(&self) -> String {
        match self {
            Self::Route { name, .. } => {
                format!("Route '{name}' saved. Reload models to apply it.")
            }
            Self::RouteDeleted(name) => {
                format!("Route '{name}' deleted. Reload models to apply it.")
            }
            Self::Role(address) => {
                let label = ROLES
                    .iter()
                    .find(|role| role.address == *address)
                    .map_or(*address, |role| role.label);
                format!("{label} saved. Reload models to apply it.")
            }
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

/// What the footer of Model Settings shows for the Routing and Agent Roles sections.
pub(super) struct FooterState {
    pub save_label: &'static str,
    /// What the selected route or role has unsaved, and what the others have.
    pub summary: Option<String>,
    pub can_save: bool,
    pub can_discard: bool,
    /// Why Save is off while something is unsaved.
    pub why_not: Option<&'static str>,
}

#[derive(Default)]
struct Unavailable {
    revision: Option<SettingsRevision>,
    reason: String,
}

/// The role a route was created for, to return to once the route is saved.
struct Origin {
    role: &'static str,
    route: String,
}

pub(super) struct Modal {
    pub tab: SettingsTab,
    pub routing: Option<Seeded<RoutingSettingsModel>>,
    pub roles: Option<Seeded<RoleSettingsModel>>,
    /// Set while a save of a role waits for the newer snapshot it was refused against.
    roles_stale: Option<String>,
    routing_ui: RoutingUi,
    roles_ui: RolesUi,
    origin: Option<Origin>,
    unavailable: Unavailable,
}

fn parse(snapshot: &SettingsSnapshot) -> Result<SettingsDocument, String> {
    if let Some(problem) = snapshot.problem {
        return Err(super::problem_label(problem).to_owned());
    }
    SettingsDocument::parse(&snapshot.document)
        .map_err(|error| format!("The settings file cannot be edited here: {error}"))
}

fn role_named(address: &str) -> Option<&'static RoleInfo> {
    ROLES.iter().find(|role| role.address == address)
}

impl Modal {
    pub(super) fn new(tab: SettingsTab) -> Self {
        Self {
            tab,
            routing: None,
            roles: None,
            roles_stale: None,
            routing_ui: RoutingUi::default(),
            roles_ui: RolesUi::default(),
            origin: None,
            unavailable: Unavailable::default(),
        }
    }

    pub(super) fn dirty(&self) -> bool {
        self.routing
            .as_ref()
            .is_some_and(|routing| !routing.model.dirty_routes().is_empty())
            || self.routing_ui.typed_rename().is_some()
            || self
                .roles
                .as_ref()
                .is_some_and(|roles| !roles.model.dirty_roles().is_empty())
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
        self.origin = None;
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
                let mut model = document.seed_roles(profile.as_deref());
                model.selected = Some(ROLES[0].address);
                self.roles = Some(Seeded {
                    model,
                    document,
                    based_on,
                });
                self.roles_stale = None;
            }
            SettingsTab::Providers => {}
        }
        Ok(())
    }

    /// A snapshot of another revision: each draft takes in what the file holds now.
    pub(super) fn follow(&mut self, snapshot: &SettingsSnapshot) {
        let moved = |based_on: &SettingsRevision| based_on != &snapshot.revision;
        let routing_moved = self.routing.as_ref().is_some_and(|r| moved(&r.based_on));
        let roles_moved = self.roles.as_ref().is_some_and(|r| moved(&r.based_on));
        if !routing_moved && !roles_moved {
            return;
        }
        let parsed = parse(snapshot);
        if routing_moved && let Some(routing) = &mut self.routing {
            match &parsed {
                Ok(document) => {
                    document.adopt_routing(&mut routing.model, None);
                    routing.document = document.clone();
                    routing.based_on = snapshot.revision.clone();
                }
                Err(_) if routing.model.dirty_routes().is_empty() => self.routing = None,
                Err(_) => routing.model.conflict = Some(PARSE_CONFLICT.to_owned()),
            }
        }
        if roles_moved && let Some(roles) = &mut self.roles {
            match &parsed {
                Ok(document) => {
                    document.adopt_roles(&mut roles.model, &BTreeMap::new(), None);
                    roles.document = document.clone();
                    roles.based_on = snapshot.revision.clone();
                    self.roles_stale = None;
                }
                Err(_) if roles.model.dirty_roles().is_empty() => self.roles = None,
                Err(_) => self.roles_stale = Some(PARSE_CONFLICT.to_owned()),
            }
        }
    }

    /// A draft already rebased past the revision `save` quoted has been checked against the
    /// newer file, and is left as it is.
    pub(super) fn stale(&mut self, save: &ModalSave) {
        let quoted = |based_on: &SettingsRevision| based_on == &save.expected_revision;
        match &save.job {
            ModalJob::Route { .. } | ModalJob::RouteDeleted(_) => {
                if let Some(routing) = &mut self.routing
                    && quoted(&routing.based_on)
                {
                    routing.model.conflict = Some(STALE_DRAFT.to_owned());
                }
            }
            ModalJob::Role(_) => {
                if self
                    .roles
                    .as_ref()
                    .is_some_and(|roles| quoted(&roles.based_on))
                {
                    self.roles_stale = Some(STALE_DRAFT.to_owned());
                }
            }
            ModalJob::Created(_) | ModalJob::Deleted(_) => {}
        }
    }

    pub(super) fn saved(&mut self, job: &ModalJob, text: &str, revision: Option<SettingsRevision>) {
        let document = SettingsDocument::parse(text).ok();
        let Some((revision, document)) = revision.zip(document) else {
            // The written revision is unknown: the drafts wait for the next snapshot instead of
            // being saved over a revision they cannot quote. Profile jobs need clean drafts,
            // which are seeded again then.
            match job {
                ModalJob::Created(_) | ModalJob::Deleted(_) => {
                    self.routing = None;
                    self.roles = None;
                }
                ModalJob::Route { .. } | ModalJob::RouteDeleted(_) => {
                    if let Some(routing) = &mut self.routing {
                        routing.model.conflict = Some(STALE_DRAFT.to_owned());
                    }
                }
                ModalJob::Role(_) => self.roles_stale = Some(STALE_DRAFT.to_owned()),
            }
            return;
        };
        match job {
            ModalJob::Created(name) => self.reseed(&document, &revision, Some(name)),
            ModalJob::Deleted(_) => self.reseed(&document, &revision, None),
            ModalJob::Route { .. } | ModalJob::RouteDeleted(_) | ModalJob::Role(_) => {
                self.adopt(job, document, revision);
            }
        }
    }

    fn adopt(&mut self, job: &ModalJob, document: SettingsDocument, revision: SettingsRevision) {
        let mut renames = BTreeMap::new();
        if let Some(routing) = &mut self.routing {
            let saved = match job {
                ModalJob::Route {
                    key,
                    name,
                    moved_refs,
                } => {
                    if *moved_refs {
                        renames.insert(key.clone(), name.clone());
                    }
                    Some((key.as_str(), name.as_str()))
                }
                ModalJob::RouteDeleted(key) => {
                    routing.model.routes.remove(key);
                    routing.model.route_name_edits.remove(key);
                    routing.model.base_routes.remove(key);
                    None
                }
                _ => None,
            };
            document.adopt_routing(&mut routing.model, saved);
            routing.document = document.clone();
            routing.based_on = revision.clone();
        }
        if let Some(roles) = &mut self.roles {
            let saved = match job {
                ModalJob::Role(address) => Some(*address),
                _ => None,
            };
            document.adopt_roles(&mut roles.model, &renames, saved);
            roles.document = document;
            roles.based_on = revision;
            self.roles_stale = None;
        }
        if let ModalJob::Route { key, .. } = job
            && self.tab == SettingsTab::Routing
            && self
                .origin
                .as_ref()
                .is_some_and(|origin| origin.route == *key)
            && let Some(origin) = self.origin.take()
        {
            self.tab = SettingsTab::AgentRoles;
            if let Some(roles) = &mut self.roles {
                roles.model.selected = Some(origin.role);
            }
        }
    }

    fn reseed(
        &mut self,
        document: &SettingsDocument,
        revision: &SettingsRevision,
        profile: Option<&str>,
    ) {
        if let Some(routing) = &mut self.routing {
            routing.model = document.seed_routing(profile);
            routing.document = document.clone();
            routing.based_on = revision.clone();
        }
        if let Some(roles) = &mut self.roles {
            let selected = roles.model.selected;
            roles.model = document.seed_roles(profile);
            roles.model.selected = selected;
            roles.document = document.clone();
            roles.based_on = revision.clone();
        }
        self.routing_ui.reset_edits();
        self.roles_stale = None;
        self.origin = None;
    }

    pub(super) fn body(
        &mut self,
        ui: &mut egui::Ui,
        snapshot: Option<&SettingsSnapshot>,
        busy: bool,
        height: f32,
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
        match self.tab {
            SettingsTab::Providers => None,
            SettingsTab::Routing => {
                let routing = self.routing.as_mut()?;
                let action = ui
                    .add_enabled_ui(!busy, |ui| {
                        routing_body(ui, &mut routing.model, &mut self.routing_ui, height)
                    })
                    .inner?;
                match action {
                    RouteAction::Delete(key) => self.delete_route(&key),
                }
            }
            SettingsTab::AgentRoles => {
                let roles = self.roles.as_mut()?;
                let action = ui
                    .add_enabled_ui(!busy, |ui| {
                        roles_body(
                            ui,
                            &mut roles.model,
                            &mut self.roles_ui,
                            self.roles_stale.as_deref(),
                            height,
                        )
                    })
                    .inner?;
                match action {
                    RolesAction::CreateRoute(logical) => {
                        self.create_route(&logical);
                        None
                    }
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

    /// What the footer shows and offers for the open section, or `None` where it has nothing
    /// to save (Providers, or a section not seeded).
    pub(super) fn footer_state(&self) -> Option<FooterState> {
        match self.tab {
            SettingsTab::Providers => None,
            SettingsTab::Routing => {
                let model = &self.routing.as_ref()?.model;
                let mut dirty = model.dirty_routes();
                let selected = model.selected.as_deref();
                // A name typed but not yet accepted is an unsaved edit of its route too.
                let typing = self.routing_ui.typed_rename();
                if let Some(key) = typing
                    && !dirty.iter().any(|name| name == key)
                {
                    dirty.push(key.to_owned());
                }
                let selected_dirty =
                    selected.is_some_and(|key| model.route_is_dirty(key) || typing == Some(key));
                let blocked = model.conflict.is_some()
                    || selected.is_some_and(|key| model.conflicted.contains(key));
                let others = dirty.len() - usize::from(selected_dirty);
                let summary = summary(
                    selected_dirty
                        .then(|| selected.map(|key| model.shown_name(key).to_owned()))
                        .flatten(),
                    others,
                    "route",
                );
                Some(FooterState {
                    save_label: "Save routing",
                    can_save: selected_dirty && !blocked,
                    can_discard: selected_dirty,
                    why_not: (selected_dirty && blocked)
                        .then_some("Changed elsewhere: discard these edits to use the saved route")
                        .or_else(|| {
                            (!selected_dirty && others > 0)
                                .then_some("Open a route with unsaved changes to save it")
                        }),
                    summary,
                })
            }
            SettingsTab::AgentRoles => {
                let model = &self.roles.as_ref()?.model;
                let dirty = model.dirty_roles();
                let selected = model.selected.and_then(role_named);
                let selected_dirty = selected.is_some_and(|role| model.role_is_dirty(role));
                let blocked = self.roles_stale.is_some()
                    || selected.is_some_and(|role| model.conflicted.contains(role.address));
                let others = dirty.len() - usize::from(selected_dirty);
                let summary = summary(
                    selected_dirty
                        .then(|| selected.map(|role| role.label.to_owned()))
                        .flatten(),
                    others,
                    "role",
                );
                Some(FooterState {
                    save_label: "Save role",
                    can_save: selected_dirty && !blocked,
                    can_discard: selected_dirty,
                    why_not: (selected_dirty && blocked)
                        .then_some("Changed elsewhere: discard these edits to use the saved role")
                        .or_else(|| {
                            (!selected_dirty && others > 0)
                                .then_some("Select a role with unsaved changes to save it")
                        }),
                    summary,
                })
            }
        }
    }

    /// Discards the edits of the selected route or role, and of nothing else.
    pub(super) fn discard_selected(&mut self) {
        match self.tab {
            SettingsTab::Routing => {
                if let Some(routing) = &mut self.routing
                    && let Some(key) = routing.model.selected.clone()
                {
                    routing.model.discard_route(&key);
                    self.routing_ui.stop_editing(&key);
                }
            }
            SettingsTab::AgentRoles => {
                if let Some(roles) = &mut self.roles
                    && let Some(role) = roles.model.selected.and_then(role_named)
                {
                    roles.model.discard_role(role);
                    self.roles_ui.stop_editing();
                }
            }
            SettingsTab::Providers => {}
        }
    }

    /// The save of the selected route or role.
    pub(super) fn save_selected(&mut self) -> Option<ModalSave> {
        match self.tab {
            SettingsTab::Routing => self.save_route(),
            SettingsTab::AgentRoles => self.save_role(),
            SettingsTab::Providers => None,
        }
    }

    fn save_route(&mut self) -> Option<ModalSave> {
        let routing = self.routing.as_mut()?;
        let key = routing.model.selected.clone()?;
        if routing.model.conflict.is_some() || routing.model.conflicted.contains(&key) {
            return None;
        }
        if let Err(error) = self.routing_ui.commit_name(&mut routing.model, &key) {
            routing.model.validation_error = Some(error);
            return None;
        }
        if !routing.model.route_is_dirty(&key) {
            return None;
        }
        match routing.document.route_text(&routing.model, &key) {
            Ok(text) => {
                routing.model.validation_error = None;
                let name = routing.model.shown_name(&key).trim().to_owned();
                let moved_refs = name != key && !routing.model.route_is_new(&key);
                Some(ModalSave {
                    job: ModalJob::Route {
                        key,
                        name,
                        moved_refs,
                    },
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

    fn save_role(&mut self) -> Option<ModalSave> {
        if self.roles_stale.is_some() {
            return None;
        }
        let roles = self.roles.as_mut()?;
        let role = roles.model.selected.and_then(role_named)?;
        if roles.model.conflicted.contains(role.address) || !roles.model.role_is_dirty(role) {
            return None;
        }
        match roles.document.role_text(&roles.model, role) {
            Ok(text) => {
                roles.model.error = None;
                Some(ModalSave {
                    job: ModalJob::Role(role.address),
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

    fn delete_route(&mut self, key: &str) -> Option<ModalSave> {
        let routing = self.routing.as_mut()?;
        if routing.model.conflict.is_some() {
            return None;
        }
        match routing.document.route_removed_text(&routing.model, key) {
            Ok(text) => {
                routing.model.validation_error = None;
                Some(ModalSave {
                    job: ModalJob::RouteDeleted(key.to_owned()),
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

    /// Opens Routing on the route a role's binding asks for, to be saved there; saving it
    /// returns to the role.
    fn create_route(&mut self, logical: &str) {
        let Some(roles) = &self.roles else {
            return;
        };
        let agents = roles.model.agents.clone();
        let role = roles.model.selected;
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
        self.origin = role.map(|role| Origin {
            role,
            route: logical.to_owned(),
        });
        if let Some(routing) = &mut self.routing {
            routing.model.add_prefill_route(logical, &agents);
            routing.model.selected = Some(logical.to_owned());
            self.routing_ui.rename_new(logical);
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

/// "Unsaved routing changes (worker)" with a note of how many others are unsaved too.
fn summary(selected: Option<String>, others: usize, noun: &str) -> Option<String> {
    let plural = if others == 1 { "" } else { "s" };
    match (selected, others) {
        (None, 0) => None,
        (Some(name), 0) => Some(format!("Unsaved changes: {name}")),
        (Some(name), others) => Some(format!(
            "Unsaved changes: {name} (+{others} other {noun}{plural})"
        )),
        (None, others) => Some(format!("{others} other {noun}{plural} unsaved")),
    }
}
