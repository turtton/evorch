//! Desktop feature of the `settings` area: the configuration as the core holds
//! it, the legacy settings modal (Providers, Routing, Agent roles) on top of it,
//! an editor for the whole document, and the reload that makes the running core
//! follow a save.
//!
//! The screen never writes a file. It sends commands that quote the revision
//! the draft was made from, so a save over a newer file is refused by the core
//! (`stale_revision`) and the draft is kept. *Saved* and *applied* are shown
//! apart: a save changes nothing a run already sent, and only
//! `reload_models` makes the requests that follow (of a running conversation
//! too) use it.
//!
//! As in the other areas an event only says something changed; the snapshot or
//! the operation is then asked for again, and so is every operation whose end
//! was not seen (after a reconnect, or when a reply was lost).
use std::collections::{BTreeMap, BTreeSet};

use super::auth::{self as auth_controller, AuthFeature};
use super::{Availability, Feature, FeatureContext, FeatureId, FeatureReply, Visibility};
use crate::model::settings_document::new_provider_defaults;
use crate::panes::provider_widgets::provider_kind_label;
use crate::panes::settings::SettingsTab;
use crate::theme::{
    icons,
    text::{medium, muted},
    tokens::*,
    widgets::{accessible, primary_button, soft_frame},
};
use workbench_protocol::{
    commands::{Command, CommandResult},
    errors::ErrorCode,
    events::UiEvent,
    handshake::Capability,
    ids::OperationId,
    namespaces::settings::{
        DeleteProvider, ProviderView, RefreshSettings, ReloadModels, SaveDocument, SaveProvider,
        SettingsCommand, SettingsEvent, SettingsOperationKind, SettingsOperationView,
        SettingsProblem, SettingsQuery, SettingsReply, SettingsResult, SettingsRevision,
        SettingsSnapshot,
    },
    operation::{CancelReason, OperationState},
    queries::{Query, QueryReply},
};

mod config_file;
mod feature_options;
mod modal;
mod model_fetch;
mod model_shell;
mod provider_form;
mod providers;
mod roles;
mod routing;
mod rows;
use feature_options::FeatureOptions;
use modal::{Modal, ModalSave};
use model_fetch::Fetch;
use provider_form::{FormAction, FormKind, ProviderForm};
use providers::{LoginAfterSave, ProviderAction, form_edited};

/// What a menu or a button elsewhere asks of the settings screens. It is applied by the
/// window (`apply_effect`); nothing of it is a protocol message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsIntent {
    /// Open Model Settings on a section, without touching the dock.
    ModelSettings(SettingsTab),
    /// Bring the Config file tab forward.
    ConfigFile,
    /// Open the Features modal.
    Features,
    /// Bring the Restart tab forward (from the Features modal's band).
    RestartCore,
}

#[derive(Debug, Clone)]
enum Pending {
    Snapshot,
    Operation(OperationId),
    Command(OperationId),
    /// The Features modal's view of the settings.
    FeatureView,
    /// The read of what a finished fetch of provider models found.
    ProviderModels(OperationId),
}

/// The document being edited and what it was made from.
struct Draft {
    text: String,
    /// The text of the snapshot it started as; the draft is dirty when it differs.
    base_text: String,
    /// The revision quoted when it is saved.
    based_on: SettingsRevision,
}

impl Draft {
    fn dirty(&self) -> bool {
        self.text != self.base_text
    }
}

pub struct SettingsFeature {
    next_tag: u64,
    pending: BTreeMap<u64, Pending>,
    snapshot: Option<SettingsSnapshot>,
    draft: Option<Draft>,
    /// The operations this screen started, running or finished.
    operations: BTreeMap<OperationId, SettingsOperationView>,
    /// Operations whose end is to be asked for.
    to_check: BTreeSet<OperationId>,
    /// Operations the core no longer knows while their end was not seen (it
    /// restarted): their result is unknown, and they no longer keep the screen
    /// busy. The snapshot shows what they left in the file.
    lost: BTreeSet<OperationId>,
    /// The text each running whole-document save was sent with.
    saving: BTreeMap<OperationId, String>,
    status: String,
    stale: bool,
    snapshot_in_flight: bool,
    /// The provider form while one is open, and the save it is waiting on.
    form: Option<ProviderForm>,
    provider_saving: Option<OperationId>,
    /// The form the running provider save was made from: only that form is closed when it ends.
    saving_form: Option<u64>,
    form_serial: u64,
    /// The Features modal (its own draft, view and save).
    features: FeatureOptions,
    /// The provider selected in the Providers list (the form edits it), when one is.
    selected: Option<String>,
    /// The name the running provider save writes: it is selected when the save worked.
    saving_name: Option<String>,
    login_after_save: Option<LoginAfterSave>,
    /// The model fetch of the provider being edited, and the fetches whose result is to be read.
    fetch: Option<Fetch>,
    read_models: Vec<OperationId>,
    /// What the controller of the `auth` area holds: the credential of every provider.
    auth: AuthFeature,
    modal: Option<Modal>,
    modal_saves: BTreeMap<OperationId, ModalSave>,
    confirm_delete: Option<String>,
    /// A modal or provider save was refused as stale against the snapshot this screen holds.
    refresh_after_stale: bool,
    quiet_refreshes: BTreeSet<OperationId>,
    /// The revision each running provider save or delete quoted.
    provider_quoted: BTreeMap<OperationId, SettingsRevision>,
    /// Closing was asked while edits were unsaved; the footer asks whether to discard them.
    confirm_close: bool,
}

impl Default for SettingsFeature {
    fn default() -> Self {
        Self {
            next_tag: 0,
            pending: BTreeMap::new(),
            snapshot: None,
            draft: None,
            operations: BTreeMap::new(),
            to_check: BTreeSet::new(),
            lost: BTreeSet::new(),
            saving: BTreeMap::new(),
            status: String::new(),
            stale: true,
            snapshot_in_flight: false,
            form: None,
            provider_saving: None,
            saving_form: None,
            form_serial: 0,
            features: FeatureOptions::default(),
            selected: None,
            saving_name: None,
            login_after_save: None,
            fetch: None,
            read_models: Vec::new(),
            auth: AuthFeature::default(),
            modal: None,
            modal_saves: BTreeMap::new(),
            confirm_delete: None,
            refresh_after_stale: false,
            quiet_refreshes: BTreeSet::new(),
            provider_quoted: BTreeMap::new(),
            confirm_close: false,
        }
    }
}

#[cfg(test)]
impl SettingsFeature {
    /// A feature whose credential controller reads the time from `now`.
    pub(in crate::core_client::features) fn with_clock(now: fn() -> u64) -> Self {
        Self {
            auth: AuthFeature::with_clock(now),
            ..Self::default()
        }
    }
}

fn new_operation() -> OperationId {
    OperationId::new(format!("gui-settings-{}", uuid::Uuid::new_v4())).expect("a uuid fits")
}

fn problem_label(problem: SettingsProblem) -> &'static str {
    match problem {
        SettingsProblem::Unreadable => "The settings file cannot be read.",
        SettingsProblem::InvalidConfig => "The settings file is not a valid configuration.",
        SettingsProblem::DocumentTooLarge => "The settings file is too large to edit here.",
    }
}

const STALE_REREAD: &str =
    "The settings were changed elsewhere. Reading them again; your edits are kept.";

fn failure_label(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::StaleRevision => {
            "The settings were changed elsewhere. Re-read them before saving."
        }
        ErrorCode::PermissionDenied => {
            "Restart targets (core.restart.targets) cannot be changed from this screen. Change them in the settings file on the machine that runs the core."
        }
        ErrorCode::InvalidRequest => "The settings could not be saved (they are not valid).",
        ErrorCode::StorageUnavailable => "The settings file cannot be written.",
        _ => "The operation failed.",
    }
}

fn applied_label(snapshot: &SettingsSnapshot) -> String {
    // Unknown (a reload failed part-way) is not "applied", whatever the
    // saved file looks like: the models may be a mixture.
    if snapshot.applied_revision.is_none() {
        match &snapshot.apply_failure {
            Some(reason) => format!(
                "The applied state is unknown (the last reload failed: {reason}). Reload models."
            ),
            None => "The applied state is unknown. Reload models.".to_owned(),
        }
    } else if snapshot.pending_apply() {
        "Saved settings are not applied to the running core yet.".to_owned()
    } else {
        "Saved settings are applied.".to_owned()
    }
}

impl SettingsFeature {
    fn tag(&mut self, pending: Pending) -> u64 {
        self.next_tag += 1;
        self.pending.insert(self.next_tag, pending);
        self.next_tag
    }

    /// Whether a save or reload is running. A model fetch waits on a provider, not on the file,
    /// so it does not hold the screen.
    fn running(&self) -> bool {
        self.operations.iter().any(|(id, view)| {
            !view.operation.state.is_final()
                && !self.lost.contains(id)
                && view.kind != SettingsOperationKind::FetchProviderModels
        })
    }

    /// Opens the settings modal on `tab`, or shows `tab` in the open one. A running save keeps
    /// an open modal where it is.
    pub fn open_modal(&mut self, tab: SettingsTab) {
        let running = self.running();
        match &mut self.modal {
            Some(_) if running => {}
            Some(modal) => modal.select(tab),
            None => self.modal = Some(Modal::new(tab)),
        }
        if tab == SettingsTab::Providers {
            self.auth.distrust();
        }
    }

    pub fn modal_tab(&self) -> Option<SettingsTab> {
        self.modal.as_ref().map(|modal| modal.tab)
    }

    fn close_modal(&mut self) {
        self.modal = None;
        self.form = None;
        self.selected = None;
        self.login_after_save = None;
        self.fetch = None;
        self.auth.cancel_confirm();
        self.confirm_delete = None;
        self.confirm_close = false;
    }

    /// Whether closing the modal would drop something typed and not saved: a Routing or Agent
    /// Roles draft, or a provider form that differs from what it was opened as.
    fn modal_dirty(&self) -> bool {
        self.modal.as_ref().is_some_and(Modal::dirty)
            || self
                .form
                .as_ref()
                .zip(self.snapshot.as_ref())
                .is_some_and(|(form, snapshot)| form_edited(snapshot, form))
    }

    /// The × icon, Esc or the backdrop: closes at once, or asks first when edits are unsaved. A
    /// second request while it asks keeps the edits.
    fn request_close(&mut self) {
        if self.confirm_close {
            self.confirm_close = false;
        } else if self.modal_dirty() {
            self.confirm_close = true;
        } else {
            self.close_modal();
        }
    }

    /// The core answers `not_found` for an operation whose end was not seen.
    /// The save may or may not have been written: the busy state is cleared,
    /// the draft is kept as it is, and the snapshot is read again (a draft
    /// made from a revision the file has left is then offered a re-read, not
    /// replaced).
    fn operation_lost(&mut self, id: &OperationId) {
        if !self.operations.contains_key(id) {
            return;
        }
        self.lost.insert(id.clone());
        self.feature_save_dropped(
            id,
            Some("The result of the save could not be confirmed (the core may have restarted). Check what was saved.".into()),
        );
        self.saving.remove(id);
        self.quiet_refreshes.remove(id);
        self.provider_quoted.remove(id);
        self.modal_saves.remove(id);
        self.drop_login_after(id);
        self.fetch_lost(
            id,
            "The result of the fetch could not be confirmed. Fetch again.",
        );
        if self.provider_saving.as_ref() == Some(id) {
            self.provider_saving = None;
            self.saving_name = None;
        }
        self.stale = true;
        self.status =
            "The result of the operation could not be confirmed (the core may have restarted). Check what was saved; your edits are kept."
                .into();
    }

    /// A whole-document save was written: the draft is dropped only when it is
    /// what was sent. Text typed since stays as the draft, now measured
    /// against what was saved.
    fn saved_document(&mut self, id: &OperationId, saved: Option<SettingsRevision>) {
        let Some(sent) = self.saving.remove(id) else {
            return;
        };
        match (&mut self.draft, saved) {
            (Some(draft), Some(revision)) if draft.text != sent => {
                draft.base_text = sent;
                draft.based_on = revision;
            }
            (Some(draft), None) if draft.text != sent => {
                // The revision is not known: the draft cannot be saved over
                // the file it was made from, and is kept to be copied.
                draft.base_text = sent;
            }
            _ => self.draft = None,
        }
    }

    /// Record what the core says of an operation of this screen.
    fn operation_update(&mut self, view: SettingsOperationView) {
        let id = view.operation.operation_id.clone();
        let Some(known) = self.operations.get(&id) else {
            return;
        };
        // An end is final: an older reply arriving after the completion
        // (still `Pending`) does not make the screen busy again.
        if known.operation.state.is_final() && !view.operation.state.is_final() {
            return;
        }
        let state = view.operation.state;
        let kind = view.kind;
        let message = view.message.clone();
        let revision = view.revision.clone();
        self.operations.insert(id.clone(), view);
        self.lost.remove(&id);
        if kind == SettingsOperationKind::SaveFeatureSettings {
            if state.is_final() {
                self.feature_save_ended(&id, state, message);
            }
            return;
        }
        if kind == SettingsOperationKind::FetchProviderModels {
            self.fetch_ended(&id, state, message);
            return;
        }
        if state.is_final() && !matches!(state, OperationState::Ready) {
            self.saving.remove(&id);
        }
        let quiet_refresh = state.is_final() && self.quiet_refreshes.remove(&id);
        let provider_quoted = if state.is_final() {
            self.provider_quoted.remove(&id)
        } else {
            None
        };
        let modal_save = if state.is_final() {
            self.modal_saves.remove(&id)
        } else {
            None
        };
        if state.is_final() && self.provider_saving.as_ref() == Some(&id) {
            self.provider_saving = None;
            // A saved provider closes the form; a refused one keeps what was typed.
            let saved_form = self.saving_form.take();
            let saved_name = self.saving_name.take();
            let worked = matches!(state, OperationState::Ready);
            if worked && self.form.as_ref().map(|form| form.serial) == saved_form {
                self.form = None;
                self.selected = saved_name;
            }
            self.provider_save_ended(&id, worked);
        }
        match state {
            OperationState::Pending => {}
            OperationState::Ready => {
                self.status = match (kind, &modal_save) {
                    (SettingsOperationKind::ReloadModels, _) => {
                        "Models reloaded. Requests from now on use them (what was already sent does not change).".into()
                    }
                    (SettingsOperationKind::Refresh, _) if quiet_refresh => {
                        std::mem::take(&mut self.status)
                    }
                    (SettingsOperationKind::Refresh, _) => String::new(),
                    (_, Some(save)) => save.job.notice(),
                    (SettingsOperationKind::SaveProvider | SettingsOperationKind::DeleteProvider, None) => {
                        "Provider settings updated. Reload models to apply them.".into()
                    }
                    _ => "Saved. Reload models to apply it.".into(),
                };
                if kind == SettingsOperationKind::SaveDocument {
                    self.saved_document(&id, revision.clone());
                }
                // The core reads the providers again before an account action is offered: what
                // the card asks about must be what the core would delete.
                if matches!(
                    kind,
                    SettingsOperationKind::SaveProvider
                        | SettingsOperationKind::DeleteProvider
                        | SettingsOperationKind::SaveDocument
                ) {
                    self.auth.distrust();
                }
                if let (Some(save), Some(modal)) = (&modal_save, &mut self.modal) {
                    modal.saved(&save.job, &save.text, revision);
                }
                self.stale = true;
            }
            OperationState::Failed { code } => {
                self.status = match message {
                    Some(message) if code != ErrorCode::StaleRevision => {
                        format!("{} {message}", failure_label(code))
                    }
                    _ => failure_label(code).to_owned(),
                };
                if code == ErrorCode::StaleRevision
                    && let (Some(save), Some(modal)) = (&modal_save, &mut self.modal)
                {
                    modal.stale(save);
                }
                // The core keeps its cached snapshot until it re-reads the file, and the modal
                // hides the tab's re-read button; a whole-file save is left to that button.
                let quoted = modal_save
                    .as_ref()
                    .map(|save| save.expected_revision.clone())
                    .or(provider_quoted);
                if code == ErrorCode::StaleRevision
                    && let Some(quoted) = quoted
                    && self
                        .snapshot
                        .as_ref()
                        .is_none_or(|snapshot| snapshot.revision == quoted)
                {
                    self.refresh_after_stale = true;
                    self.status = STALE_REREAD.into();
                }
                self.stale = true;
            }
            OperationState::Cancelled { reason } => {
                self.status = match reason {
                    CancelReason::Requested => "The operation was cancelled.",
                    CancelReason::Restart | CancelReason::Shutdown => {
                        "The operation did not run because of a restart or shutdown."
                    }
                }
                .into();
            }
        }
    }

    fn snapshot_arrived(&mut self, snapshot: SettingsSnapshot) {
        let reload = match &self.draft {
            None => true,
            Some(draft) => !draft.dirty() && draft.based_on != snapshot.revision,
        };
        if reload {
            self.draft = Some(Draft {
                text: snapshot.document.clone(),
                base_text: snapshot.document.clone(),
                based_on: snapshot.revision.clone(),
            });
        } else if let Some(draft) = &self.draft
            && draft.based_on != snapshot.revision
        {
            self.status =
                "The settings were changed elsewhere. Re-read them to save your edits.".into();
        }
        if let Some(modal) = &mut self.modal {
            modal.follow(&snapshot);
        }
        self.snapshot = Some(snapshot);
    }

    fn send(&mut self, ctx: &mut FeatureContext<'_>, command: SettingsCommand) {
        let id = command.operation_id().clone();
        let kind = command.kind();
        match &command {
            SettingsCommand::SaveProvider(save) => {
                self.provider_quoted
                    .insert(id.clone(), save.expected_revision.clone());
            }
            SettingsCommand::DeleteProvider(delete) => {
                self.provider_quoted
                    .insert(id.clone(), delete.expected_revision.clone());
            }
            _ => {}
        }
        self.operations.insert(
            id.clone(),
            SettingsOperationView {
                operation: workbench_protocol::operation::OperationView {
                    operation_id: id.clone(),
                    state: OperationState::Pending,
                    revision: workbench_protocol::wire::Revision(0),
                    result: None,
                },
                kind,
                revision: None,
                message: None,
            },
        );
        let tag = self.tag(Pending::Command(id));
        ctx.command(tag, Command::Settings(command));
    }

    fn send_document(
        &mut self,
        ctx: &mut FeatureContext<'_>,
        expected_revision: SettingsRevision,
        text: String,
    ) -> OperationId {
        let operation_id = new_operation();
        self.send(
            ctx,
            SettingsCommand::SaveDocument(SaveDocument {
                operation_id: operation_id.clone(),
                expected_revision,
                text,
            }),
        );
        operation_id
    }

    fn reload_models(&mut self, ctx: &mut FeatureContext<'_>) {
        let operation_id = new_operation();
        self.send(
            ctx,
            SettingsCommand::ReloadModels(ReloadModels { operation_id }),
        );
    }
}

impl Feature for SettingsFeature {
    fn id(&self) -> FeatureId {
        FeatureId::Settings
    }

    fn title(&self) -> &'static str {
        "Config file"
    }

    fn availability(&self) -> Availability {
        Availability::Implemented {
            required: &[Capability::Commands, Capability::Queries],
        }
    }

    fn reset(&mut self) {
        self.auth.reset();
        // A save whose end the reconnect may have hidden starts no sign-in.
        if self
            .login_after_save
            .as_ref()
            .is_some_and(|login| !login.saved())
        {
            self.login_after_save = None;
        }
        self.snapshot = None;
        self.snapshot_in_flight = false;
        self.stale = true;
        self.read_models.clear();
        if let Some(fetch) = &mut self.fetch {
            fetch.phase = model_fetch::FetchPhase::Failed(
                "The connection to the core was reset. Fetch again.".into(),
            );
        }
        // What was asked before the snapshot is answered, if at all, by a reply
        // that is no longer wanted; the operations are asked for again.
        self.pending
            .retain(|_, pending| matches!(pending, Pending::Command(_)));
        self.features.reset();
        for (id, view) in &self.operations {
            if !view.operation.state.is_final() && !self.lost.contains(id) {
                self.to_check.insert(id.clone());
            }
        }
    }

    fn listens_to(&self, namespace: &str) -> bool {
        matches!(namespace, "settings" | "auth")
    }

    fn event(&mut self, event: &UiEvent) {
        match event {
            UiEvent::Settings(SettingsEvent::SettingsSnapshotChanged { .. }) => {
                self.stale = true;
                self.features.changed_elsewhere();
            }
            UiEvent::Settings(SettingsEvent::SettingsOperationCompleted { operation }) => {
                self.operation_update(operation.clone());
            }
            UiEvent::Auth(_) => self.auth.event(event),
            _ => {}
        }
    }

    fn reply(&mut self, tag: u64, reply: FeatureReply) {
        if tag >= auth_controller::TAG_BASE {
            self.auth.reply(tag, reply);
            return;
        }
        let Some(pending) = self.pending.remove(&tag) else {
            return;
        };
        match (pending, reply) {
            (Pending::Snapshot, FeatureReply::Query(result)) => {
                self.snapshot_in_flight = false;
                match result {
                    Ok(QueryReply::Settings(SettingsReply::Settings(snapshot))) => {
                        self.snapshot_arrived(*snapshot);
                    }
                    Ok(_) => {}
                    Err(error) => {
                        self.status = error.message;
                        self.stale = true;
                    }
                }
            }
            (Pending::FeatureView, FeatureReply::Query(result)) => self.features_reply(result),
            (Pending::Operation(id), FeatureReply::Query(result)) => match result {
                Ok(QueryReply::Settings(SettingsReply::Operation(view))) => {
                    self.operation_update(view);
                }
                Ok(_) => {}
                Err(error) if error.code == Some(ErrorCode::NotFound) => self.operation_lost(&id),
                Err(error) => {
                    // Nothing asks for this operation again, so it is not left holding the
                    // modal busy until a reconnect.
                    self.operation_lost(&id);
                    self.status = format!("{} {}", self.status, error.message);
                }
            },
            (Pending::ProviderModels(id), FeatureReply::Query(result)) => {
                let found = match result {
                    Ok(QueryReply::Settings(SettingsReply::ProviderModels(found))) => Ok(found),
                    Ok(_) => Err("The core answered with something else.".to_owned()),
                    Err(error) => Err(error.message),
                };
                self.fetch_read(&id, found);
            }
            (Pending::Command(id), FeatureReply::Command(result)) => match result {
                Ok(reply) => {
                    if let Some(CommandResult::Settings(SettingsResult::Operation(view))) =
                        reply.result.map(|boxed| *boxed)
                    {
                        self.operation_update(view);
                    }
                }
                Err(error) => {
                    self.status = error.message.clone();
                    if !error.outcome_unknown {
                        self.fetch_lost(&id, &error.message);
                    }
                    // A save whose end is not known starts no sign-in, whatever it turns out to be.
                    self.drop_login_after(&id);
                    if error.outcome_unknown {
                        // The command may have been applied: its operation is
                        // asked for, not sent again.
                        self.to_check.insert(id);
                    } else {
                        self.feature_save_dropped(&id, Some(error.message.clone()));
                        self.operations.remove(&id);
                        self.saving.remove(&id);
                        self.quiet_refreshes.remove(&id);
                        self.provider_quoted.remove(&id);
                        self.modal_saves.remove(&id);
                        if self.provider_saving.as_ref() == Some(&id) {
                            self.provider_saving = None;
                            self.saving_name = None;
                        }
                    }
                }
            },
            _ => {}
        }
    }

    fn maintain(&mut self, ctx: &mut FeatureContext<'_>, visibility: Visibility) {
        if !visibility.available {
            return;
        }
        self.auth
            .maintain(ctx, self.modal_tab() == Some(SettingsTab::Providers));
        self.start_login_after_save(ctx);
        for id in std::mem::take(&mut self.read_models) {
            let tag = self.tag(Pending::ProviderModels(id.clone()));
            ctx.query(
                tag,
                Query::Settings(SettingsQuery::GetProviderModels { operation_id: id }),
            );
        }
        // The snapshot is read for whatever shows it: the Config file tab, or the Model Settings
        // modal over any tab. What was started is looked up whether or not anything shows it.
        let shown = visibility.is_visible() || self.modal.is_some();
        if shown && self.stale && !self.snapshot_in_flight {
            self.stale = false;
            self.snapshot_in_flight = true;
            let tag = self.tag(Pending::Snapshot);
            ctx.query(tag, Query::Settings(SettingsQuery::GetSettingsSnapshot {}));
        }
        if std::mem::take(&mut self.refresh_after_stale) {
            let operation_id = new_operation();
            self.quiet_refreshes.insert(operation_id.clone());
            self.send(
                ctx,
                SettingsCommand::RefreshSettings(RefreshSettings { operation_id }),
            );
        }
        for id in std::mem::take(&mut self.to_check) {
            let tag = self.tag(Pending::Operation(id.clone()));
            ctx.query(
                tag,
                Query::Settings(SettingsQuery::GetSettingsOperation { operation_id: id }),
            );
        }
        self.features_maintain(ctx);
    }

    fn render(&mut self, ui: &mut egui::Ui, ctx: &mut FeatureContext<'_>) {
        self.config_file(ui, ctx);
    }

    fn render_overlay(&mut self, egui_ctx: &egui::Context, ctx: &mut FeatureContext<'_>) {
        self.render_model_settings(egui_ctx, ctx);
        self.render_features(egui_ctx, ctx);
    }

    fn as_settings_mut(&mut self) -> Option<&mut SettingsFeature> {
        Some(self)
    }
}

#[cfg(test)]
mod modal_tests;
#[cfg(test)]
mod tests;
