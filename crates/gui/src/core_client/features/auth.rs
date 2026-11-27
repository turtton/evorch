//! The controller of the `auth` area inside the settings feature: the credential of every
//! provider, API keys typed in, and browser logins. It has no tab of its own; the Providers
//! section of Model Settings draws it as the Account card of the selected provider.
//!
//! Its tags start at [`TAG_BASE`], so that a reply that reaches the settings feature is told
//! from the replies of the settings' own requests.
//!
//! A login's redirect lands on the machine that runs the core. When this
//! screen runs on another machine, the person pastes the address the browser
//! ended on (or the code) into the box below the login link; the core checks it
//! against that login and uses it once. What is typed or pasted is a secret:
//! it is sent once, cleared from its box at that moment, and never put in a
//! message or a label.
//!
//! As in the other areas an event only says something changed; the status, the
//! login or the operation is then asked for again, and so is every operation
//! whose end was not seen. A command is never sent again by this feature.
use std::collections::{BTreeMap, BTreeSet};

use super::{FeatureContext, FeatureReply};
use crate::format::token_expiry;
use crate::panes::provider_widgets::{API_KEY_EMPTY_HINT, API_KEY_STORED_HINT};
use crate::theme::{
    icons,
    text::{medium, muted},
    tokens::*,
    widgets::{accessible, soft_frame, status_dot},
};
use workbench_protocol::{
    commands::{Command, CommandResult},
    errors::ErrorCode,
    events::UiEvent,
    ids::OperationId,
    namespaces::auth::{
        AuthCommand, AuthEvent, AuthOperationKind, AuthOperationView, AuthQuery, AuthReply,
        AuthResult, AuthStatus, CancelLogin, CredentialSlot, CredentialState, DeleteCredential,
        LoginInputOutcome, LoginInputRejection, LoginMethod, LoginPhase, LoginView,
        RefreshCredentials, SaveApiKey, SecretValue, StartLogin, SubmitLoginInput,
    },
    namespaces::settings::CredentialRef,
    operation::{CancelReason, OperationState, OperationView},
    queries::{Query, QueryReply},
    wire::Revision,
};

#[derive(Debug, Clone)]
enum Pending {
    /// `true`: asked after a refresh that made the core read the settings again, so the answer
    /// is what the next account action may rely on.
    Status(bool),
    Operation(OperationId),
    Login(OperationId),
    Command(Sent),
}

#[derive(Debug, Clone)]
enum Sent {
    Operation(OperationId),
    Login(OperationId),
}

/// The login this screen started and what the core says of it.
struct Session {
    id: OperationId,
    /// The provider the login is for.
    profile: String,
    view: Option<LoginView>,
}

/// The first tag of the auth controller; the settings' own tags stay below it.
pub(super) const TAG_BASE: u64 = 1 << 32;

pub struct AuthFeature {
    next_tag: u64,
    pending: BTreeMap<u64, Pending>,
    status: Option<AuthStatus>,
    operations: BTreeMap<OperationId, AuthOperationView>,
    to_check: BTreeSet<OperationId>,
    /// Operations the core no longer knows (it restarted, or forgot them)
    /// while their end was not seen: what became of them is not known, and
    /// they no longer keep the screen busy.
    lost: BTreeSet<OperationId>,
    session: Option<Session>,
    /// What is being typed into the box of the login link.
    paste: String,
    /// The API key being typed, by profile.
    keys: BTreeMap<String, String>,
    message: String,
    status_stale: bool,
    status_in_flight: bool,
    login_stale: bool,
    login_in_flight: bool,
    confirm_delete: Option<String>,
    /// Whether the status is known to follow the saved settings: the core read them again
    /// (`RefreshCredentials`) after the last change and the status was read after that. A
    /// credential action waits for it, so what it asks about and what the core deletes agree.
    trusted: bool,
    /// The refresh that is to make the status trusted.
    trusting: Option<OperationId>,
    awaiting_status: bool,
    now: fn() -> u64,
}

impl Default for AuthFeature {
    fn default() -> Self {
        Self {
            next_tag: TAG_BASE,
            pending: BTreeMap::new(),
            status: None,
            operations: BTreeMap::new(),
            to_check: BTreeSet::new(),
            lost: BTreeSet::new(),
            session: None,
            paste: String::new(),
            keys: BTreeMap::new(),
            message: String::new(),
            status_stale: true,
            status_in_flight: false,
            login_stale: false,
            login_in_flight: false,
            confirm_delete: None,
            trusted: false,
            trusting: None,
            awaiting_status: false,
            now: unix_now,
        }
    }
}

#[cfg(test)]
impl AuthFeature {
    pub(super) fn with_clock(now: fn() -> u64) -> Self {
        Self {
            now,
            ..Self::default()
        }
    }
}

fn new_operation() -> OperationId {
    OperationId::new(format!("gui-auth-{}", uuid::Uuid::new_v4())).expect("a uuid fits")
}

fn method_label(method: LoginMethod) -> &'static str {
    match method {
        LoginMethod::Codex => "Codex",
        LoginMethod::Claude => "Claude",
        LoginMethod::Cursor => "Cursor",
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

fn rejection_label(reason: LoginInputRejection) -> &'static str {
    match reason {
        LoginInputRejection::UnknownLogin => "This sign-in was not found.",
        LoginInputRejection::NotWaiting => "This sign-in is not waiting for input.",
        LoginInputRejection::AlreadyUsed => "This sign-in already used its input.",
        LoginInputRejection::Expired => "The sign-in expired. Start again.",
        LoginInputRejection::StateMismatch => {
            "This input belongs to another sign-in. Start again from the link shown."
        }
        LoginInputRejection::Malformed => "Paste the authorization code or final callback URL.",
    }
}

fn failure_label(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::Busy => "Sign-in could not complete (the port is busy or unreachable).",
        ErrorCode::InvalidRequest => {
            "This is not possible (authorization was refused or the profile does not support it)."
        }
        ErrorCode::NotFound => "The profile was not found.",
        ErrorCode::StorageUnavailable => "The credential could not be saved or deleted.",
        _ => "The operation failed.",
    }
}

impl AuthFeature {
    fn tag(&mut self, pending: Pending) -> u64 {
        self.next_tag += 1;
        self.pending.insert(self.next_tag, pending);
        self.next_tag
    }

    fn running(&self) -> bool {
        self.operations.iter().any(|(id, view)| {
            !view.operation.state.is_final()
                && view.kind != AuthOperationKind::Login
                && !self.lost.contains(id)
        })
    }

    /// The core answers `not_found` for an operation whose end was not seen.
    /// Its result is unknown, not failed: the busy state is cleared, a login
    /// session of it is dropped, and the credential status is read again, which
    /// shows what the operation left behind.
    fn operation_lost(&mut self, id: &OperationId) {
        if !self.operations.contains_key(id) {
            return;
        }
        self.lost.insert(id.clone());
        if self.session.as_ref().is_some_and(|s| &s.id == id) {
            self.session = None;
            self.paste.clear();
        }
        self.status_stale = true;
        self.message =
            "The result of the operation could not be confirmed (the core may have restarted). Check the credential status.".into();
    }

    fn login_active(&self) -> bool {
        self.session.is_some()
    }

    fn operation_update(&mut self, view: AuthOperationView) {
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
        let detail = view.message.clone();
        self.operations.insert(id.clone(), view);
        self.lost.remove(&id);
        if !state.is_final() {
            return;
        }
        self.status_stale = true;
        if kind == AuthOperationKind::RefreshStatus
            && self.trusting.as_ref() == Some(&id)
            && matches!(state, OperationState::Ready)
        {
            self.awaiting_status = true;
        }
        if kind == AuthOperationKind::Login && self.session.as_ref().is_some_and(|s| s.id == id) {
            self.session = None;
            self.paste.clear();
        }
        self.message = match state {
            OperationState::Ready => match kind {
                AuthOperationKind::Login => "Signed in.".into(),
                AuthOperationKind::SaveApiKey => "API key saved.".into(),
                AuthOperationKind::DeleteCredential => "Credential deleted.".into(),
                AuthOperationKind::RefreshStatus => String::new(),
            },
            OperationState::Failed { code } => match detail {
                Some(detail) => format!("{} {detail}", failure_label(code)),
                None => failure_label(code).to_owned(),
            },
            OperationState::Cancelled { reason } => match reason {
                CancelReason::Requested => "Sign-in cancelled.".into(),
                CancelReason::Restart | CancelReason::Shutdown => {
                    "Interrupted by a restart or shutdown. Sign in again.".into()
                }
            },
            OperationState::Pending => String::new(),
        };
    }

    fn send(
        &mut self,
        ctx: &mut FeatureContext<'_>,
        id: OperationId,
        kind: AuthOperationKind,
        profile: Option<String>,
        command: AuthCommand,
    ) {
        self.operations.insert(
            id.clone(),
            AuthOperationView {
                operation: OperationView {
                    operation_id: id.clone(),
                    state: OperationState::Pending,
                    revision: Revision(0),
                    result: None,
                },
                kind,
                profile,
                message: None,
            },
        );
        let tag = self.tag(Pending::Command(Sent::Operation(id)));
        ctx.command(tag, Command::Auth(command));
    }

    pub(super) fn slot(&self, profile: &str) -> Option<&CredentialSlot> {
        self.status
            .as_ref()
            .and_then(|status| status.slots.iter().find(|slot| slot.profile == profile))
    }

    /// Whether an operation of this controller is running (a login waiting for a person is not
    /// one: the buttons stay available around it).
    pub(super) fn busy(&self) -> bool {
        self.running()
    }

    /// Whether a login can be started now: nothing runs and no other login waits.
    pub(super) fn can_start_login(&self) -> bool {
        !self.held() && !self.login_active()
    }

    /// Whether credential actions wait: an operation runs, or the status is not yet known to
    /// follow the saved settings.
    fn held(&self) -> bool {
        self.running() || !self.trusted
    }

    /// The saved providers changed (or the section was opened): the core reads them again
    /// before a credential action is offered.
    pub(super) fn distrust(&mut self) {
        self.trusted = false;
        self.trusting = None;
        self.awaiting_status = false;
        self.status_stale = true;
    }

    /// Whether a sign-in of `profile` is being prepared or waits for a person.
    pub(super) fn login_of(&self, profile: &str) -> bool {
        self.session.as_ref().is_some_and(|s| s.profile == profile)
            || self.operations.iter().any(|(id, view)| {
                view.kind == AuthOperationKind::Login
                    && !view.operation.state.is_final()
                    && !self.lost.contains(id)
                    && view.profile.as_deref() == Some(profile)
            })
    }

    /// The other providers whose credential is the very entry `slot` names.
    fn shared_with(&self, name: &str, slot: &CredentialSlot) -> Vec<String> {
        if !matches!(slot.storage, CredentialRef::Keyring { .. }) {
            return Vec::new();
        }
        self.status.as_ref().map_or_else(Vec::new, |status| {
            status
                .slots
                .iter()
                .filter(|other| other.profile != name && other.storage == slot.storage)
                .map(|other| other.profile.clone())
                .collect()
        })
    }

    /// The dot color, the sentence and whether it is fine, for the credential of `name`.
    pub(super) fn state_of(&self, name: &str) -> (egui::Color32, String, bool) {
        let slot = self.slot(name);
        let summary = summary(
            slot,
            self.status.is_some(),
            slot.and_then(|slot| slot.login),
            true,
            (self.now)(),
        );
        (summary.tone.color(), summary.text, summary.tone == Tone::Ok)
    }

    /// Forget a question about deleting a credential.
    pub(super) fn cancel_confirm(&mut self) {
        self.confirm_delete = None;
    }

    /// Forget the sentence about the last operation.
    pub(super) fn clear_message(&mut self) {
        self.message.clear();
    }

    /// Drop what was being typed for `profile` (it was deleted or renamed).
    pub(super) fn forget(&mut self, profile: &str) {
        self.keys.remove(profile);
        if self.confirm_delete.as_deref() == Some(profile) {
            self.confirm_delete = None;
        }
    }

    /// Re-read the credential store of every provider (the Refresh icon).
    pub(super) fn refresh(&mut self, ctx: &mut FeatureContext<'_>) {
        if self.running() {
            return;
        }
        let id = new_operation();
        // Whatever started it (opening, a save, the icon): the status is not relied on until the
        // core has read again and the status after that has arrived.
        self.distrust();
        self.trusting = Some(id.clone());
        self.send(
            ctx,
            id.clone(),
            AuthOperationKind::RefreshStatus,
            None,
            AuthCommand::RefreshCredentials(RefreshCredentials { operation_id: id }),
        );
    }

    /// Start the browser login of `profile`.
    pub(super) fn start_login(&mut self, ctx: &mut FeatureContext<'_>, profile: &str) {
        if self.login_active() || self.running() {
            return;
        }
        let id = new_operation();
        self.send(
            ctx,
            id.clone(),
            AuthOperationKind::Login,
            Some(profile.to_owned()),
            AuthCommand::StartLogin(StartLogin {
                operation_id: id,
                profile: profile.to_owned(),
            }),
        );
    }

    /// The Account card of one provider: what its credential is, and everything that can be
    /// done with it. Nothing typed here leaves the screen but in the one command that carries
    /// it, and nothing of it is kept afterwards.
    pub(super) fn account_card(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &mut FeatureContext<'_>,
        account: &Account<'_>,
    ) -> Option<AccountAction> {
        let slot = self.slot(account.name).cloned();
        let shared = slot
            .as_ref()
            .map(|slot| self.shared_with(account.name, slot))
            .unwrap_or_default();
        let login = slot.as_ref().and_then(|slot| slot.login).or(account.login);
        let summary = summary(
            slot.as_ref(),
            self.status.is_some(),
            login,
            account.saved,
            (self.now)(),
        );
        let mut outcome = None;
        soft_frame(palette().SURFACE_RAISED)
            .stroke(egui::Stroke::new(1.0, palette().BORDER))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    status_dot(ui, summary.tone.color());
                    let label = ui.label(medium(summary.text.as_str()));
                    if let Some(absolute) = &summary.tooltip {
                        label.on_hover_text(absolute);
                    }
                });
                if let Some(note) = summary.note {
                    ui.add(egui::Label::new(muted(note)).wrap());
                }
                if let Some(slot) = &slot {
                    let storage = match &slot.storage {
                        CredentialRef::Env { var } => {
                            format!("Read from environment variable {var}.")
                        }
                        CredentialRef::Keyring { service, account } => format!(
                            "Stored in the credential store (service \"{service}\", account {account}). Secrets are never shown."
                        ),
                    };
                    ui.add(egui::Label::new(muted(storage)).wrap());
                }
                outcome = self.account_actions(ui, ctx, account, slot.as_ref(), login, &shared);
                self.login_panel(ui, ctx, account.name);
                if !self.message.is_empty() {
                    ui.add(egui::Label::new(muted(self.message.as_str())).wrap());
                }
            });
        outcome
    }

    fn account_actions(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &mut FeatureContext<'_>,
        account: &Account<'_>,
        slot: Option<&CredentialSlot>,
        login: Option<LoginMethod>,
        shared: &[String],
    ) -> Option<AccountAction> {
        let busy = self.held();
        let name = account.name;
        let mut outcome = None;
        let Some(slot) = slot.filter(|_| account.saved) else {
            // A provider that is not saved has no credential to ask about; a browser sign-in
            // needs the saved provider, so it is saved first and signed in only after that
            // worked.
            if login.is_some() {
                ui.add(
                    egui::Label::new(muted(
                        "Sign-in needs a saved provider: this saves it first, and continues only once that worked.",
                    ))
                    .wrap(),
                );
                let enabled = account.savable && !account.saving && !busy && !self.login_active();
                let sign_in = ui.add_enabled(
                    enabled,
                    egui::Button::new(icons::with_icon(icons::SIGN_IN, "Save & sign in"))
                        .fill(palette().ACCENT),
                );
                accessible(&sign_in, "Save & sign in");
                if sign_in.clicked() {
                    outcome = Some(AccountAction::SaveAndSignIn);
                }
            } else {
                ui.add(
                    egui::Label::new(muted(
                        "Save the provider first; its API key is stored from this card afterwards.",
                    ))
                    .wrap(),
                );
            }
            return outcome;
        };
        let present = matches!(slot.state, CredentialState::Present { .. });
        let keyring = matches!(slot.storage, CredentialRef::Keyring { .. });
        let mut ask_delete = false;
        ui.horizontal_wrapped(|ui| {
            if let Some(method) = login {
                let text = if present { "Sign in again" } else { "Sign in" };
                let sign_in = ui.add_enabled(
                    !self.login_active() && !busy,
                    egui::Button::new(icons::with_icon(icons::SIGN_IN, text)),
                );
                accessible(&sign_in, &format!("Sign in to {}", method_label(method)));
                if sign_in.clicked() {
                    self.start_login(ctx, name);
                }
            }
            if keyring && present && login.is_some() {
                let out = ui.add_enabled(
                    !busy,
                    egui::Button::new(
                        egui::RichText::new(icons::with_icon(icons::SIGN_OUT, "Sign out"))
                            .color(palette().ERROR_FG),
                    ),
                );
                accessible(&out, "Sign out");
                ask_delete |= out.clicked();
            }
        });
        // A Cursor profile signs in with a dashboard API key too (the core exchanges it); the
        // other logins have no key to type.
        if keyring && (login.is_none() || login == Some(LoginMethod::Cursor)) {
            let typed = self.keys.entry(name.to_owned()).or_default();
            let hint = if present {
                API_KEY_STORED_HINT
            } else {
                API_KEY_EMPTY_HINT
            };
            let mut store = false;
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(typed)
                        .password(true)
                        .hint_text(hint)
                        .desired_width((ui.available_width() - 150.0).max(80.0)),
                );
                let empty = typed.trim().is_empty();
                let button = ui.add_enabled(
                    !empty && !busy,
                    egui::Button::new(icons::with_icon(icons::KEY, "Store key")),
                );
                accessible(&button, "Save API key");
                store = button.clicked();
            });
            if store {
                let value = SecretValue::new(std::mem::take(typed));
                let id = new_operation();
                self.send(
                    ctx,
                    id.clone(),
                    AuthOperationKind::SaveApiKey,
                    Some(name.to_owned()),
                    AuthCommand::SaveApiKey(SaveApiKey {
                        operation_id: id,
                        profile: name.to_owned(),
                        value,
                    }),
                );
            }
            if present && login.is_none() {
                let remove = ui.add_enabled(
                    !busy,
                    egui::Button::new(
                        egui::RichText::new(icons::with_icon(icons::TRASH, "Remove key"))
                            .color(palette().ERROR_FG),
                    ),
                );
                accessible(&remove, "Remove key");
                ask_delete |= remove.clicked();
            }
        }
        if ask_delete {
            self.confirm_delete = Some(name.to_owned());
        }
        if self.confirm_delete.as_deref() == Some(name) {
            self.confirm_credential_delete(ui, ctx, account, slot, login.is_some(), shared);
        }
        outcome
    }

    /// The question before a credential is deleted. A keychain entry can be named by several
    /// providers, so what else loses it is said before it is gone.
    fn confirm_credential_delete(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &mut FeatureContext<'_>,
        account: &Account<'_>,
        slot: &CredentialSlot,
        sign_in: bool,
        shared: &[String],
    ) {
        let busy = self.held();
        let name = account.name;
        let entry = match &slot.storage {
            CredentialRef::Keyring { service, account } => {
                format!("service \"{service}\", account {account}")
            }
            CredentialRef::Env { var } => format!("environment variable {var}"),
        };
        let (question, confirm) = if sign_in {
            (format!("Sign out of {name}?"), "Confirm sign out")
        } else {
            (
                format!("Remove the API key of {name}?"),
                "Confirm remove key",
            )
        };
        let mut delete = false;
        egui::Frame::new()
            .fill(palette().WARNING_SURFACE)
            .corner_radius(egui::CornerRadius::same(R_MD))
            .inner_margin(egui::Margin::same(SP_2 as i8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(medium(question).color(palette().WARNING_FG));
                ui.add(
                    egui::Label::new(muted(format!(
                        "This deletes the stored credential ({entry}); the provider stays configured."
                    )))
                    .wrap(),
                );
                if !shared.is_empty() {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(format!(
                                "Also used by {}: they lose it too.",
                                shared.join(", ")
                            ))
                            .color(palette().WARNING_FG),
                        )
                        .wrap(),
                    );
                }
                ui.horizontal(|ui| {
                    delete = ui.add_enabled(!busy, egui::Button::new(confirm)).clicked();
                    if ui.button("Keep credential").clicked() {
                        self.confirm_delete = None;
                    }
                });
            });
        if delete {
            self.confirm_delete = None;
            let id = new_operation();
            self.send(
                ctx,
                id.clone(),
                AuthOperationKind::DeleteCredential,
                Some(name.to_owned()),
                AuthCommand::DeleteCredential(DeleteCredential {
                    operation_id: id,
                    profile: name.to_owned(),
                }),
            );
        }
    }

    /// The browser login of `profile`, while one is running: its link, the box for what the
    /// browser ended on when it is not on the core's machine, and Cancel.
    fn login_panel(&mut self, ui: &mut egui::Ui, ctx: &mut FeatureContext<'_>, profile: &str) {
        let Some(session) = self.session.as_ref().filter(|s| s.profile == profile) else {
            return;
        };
        let id = session.id.clone();
        ui.separator();
        let Some(view) = session.view.clone() else {
            ui.label("Preparing browser sign-in…");
            return;
        };
        ui.label(medium(format!(
            "{} sign-in ({})",
            view.profile,
            method_label(view.method)
        )));
        match view.phase {
            LoginPhase::Completing => {
                ui.label("Completing sign-in…");
            }
            LoginPhase::Settled => {
                ui.label("Sign-in finished.");
            }
            LoginPhase::Waiting => {
                if let Some(url) = &view.authorize_url {
                    ui.label("Waiting for browser sign-in…");
                    ui.horizontal(|ui| {
                        let open =
                            ui.button(icons::with_icon(icons::ARROW_SQUARE_OUT, "Open login page"));
                        accessible(&open, "Open login page");
                        if open.clicked() {
                            ui.ctx().open_url(egui::OpenUrl::new_tab(url.clone()));
                        }
                        let copy = ui.button(icons::with_icon(icons::COPY, "Copy link"));
                        accessible(&copy, "Copy link");
                        if copy.clicked() {
                            ui.ctx().copy_text(url.clone());
                        }
                    });
                }
                if view.accepts_input {
                    ui.add(
                        egui::Label::new(muted(
                            "The browser callback completes sign-in automatically when the browser runs on the core's machine. Otherwise paste the authorization code or final callback URL below.",
                        ))
                        .wrap(),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut self.paste)
                            .password(true)
                            .hint_text("Authorization code / callback URL"),
                    );
                    if ui
                        .add_enabled(
                            !self.paste.trim().is_empty(),
                            egui::Button::new("Complete sign-in"),
                        )
                        .clicked()
                    {
                        let input = SecretValue::new(std::mem::take(&mut self.paste));
                        let tag = self.tag(Pending::Command(Sent::Login(id.clone())));
                        ctx.command(
                            tag,
                            Command::Auth(AuthCommand::SubmitLoginInput(SubmitLoginInput {
                                login_id: id.clone(),
                                input,
                            })),
                        );
                    }
                }
            }
        }
        if ui.button("Cancel sign-in").clicked() {
            let tag = self.tag(Pending::Command(Sent::Login(id.clone())));
            ctx.command(
                tag,
                Command::Auth(AuthCommand::CancelLogin(CancelLogin { login_id: id })),
            );
        }
    }
}

/// A provider as its Account card needs to know it.
pub(super) struct Account<'a> {
    pub name: &'a str,
    /// The sign-in a provider of this kind uses, when it has one: a provider that is not saved
    /// has no credential slot to tell.
    pub login: Option<LoginMethod>,
    /// Whether the provider is in the saved settings.
    pub saved: bool,
    /// Whether the form can be saved as it is.
    pub savable: bool,
    /// A provider save is running.
    pub saving: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AccountAction {
    /// Save the provider, then sign in to it once the save has worked.
    SaveAndSignIn,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tone {
    Ok,
    Warn,
    Error,
    Off,
}

impl Tone {
    fn color(self) -> egui::Color32 {
        match self {
            Self::Ok => palette().SUCCESS,
            Self::Warn => palette().WARNING,
            Self::Error => palette().ERROR,
            Self::Off => palette().TEXT_MUTED,
        }
    }
}

struct Summary {
    tone: Tone,
    text: String,
    /// The absolute expiry, for the tooltip.
    tooltip: Option<String>,
    note: Option<&'static str>,
}

/// What a provider's credential reads as: a dot's tone and a sentence.
fn summary(
    slot: Option<&CredentialSlot>,
    status_known: bool,
    login: Option<LoginMethod>,
    saved: bool,
    now: u64,
) -> Summary {
    let plain = |tone, text: String| Summary {
        tone,
        text,
        tooltip: None,
        note: None,
    };
    let Some(slot) = slot else {
        return plain(
            Tone::Off,
            match (saved, status_known, login) {
                (false, _, Some(_)) => "Not signed in",
                (false, _, None) => "No API key stored yet",
                (true, false, _) => "Checking credentials…",
                (true, true, _) => "Not checked",
            }
            .into(),
        );
    };
    match slot.state {
        CredentialState::Unknown => plain(Tone::Off, "Not checked".into()),
        CredentialState::Missing => match (&slot.storage, slot.login) {
            (CredentialRef::Env { var }, _) => plain(
                Tone::Error,
                format!("Environment variable {var} is not set"),
            ),
            (_, Some(_)) => plain(Tone::Off, "Not signed in".into()),
            _ => plain(Tone::Error, "API key missing".into()),
        },
        CredentialState::Present {
            expires_at_unix: Some(at),
        } => {
            let (text, tooltip) = token_expiry(at, now);
            let expired = at <= now;
            Summary {
                tone: if expired { Tone::Warn } else { Tone::Ok },
                text,
                tooltip,
                note: expired.then_some("Token expired; it will renew automatically when used."),
            }
        }
        CredentialState::Present {
            expires_at_unix: None,
        } if slot.login.is_some() => plain(Tone::Ok, "Signed in".into()),
        CredentialState::Present {
            expires_at_unix: None,
        } => plain(
            Tone::Ok,
            match slot.storage {
                CredentialRef::Env { .. } => "Set",
                CredentialRef::Keyring { .. } => "Stored",
            }
            .into(),
        ),
        CredentialState::Unavailable => {
            plain(Tone::Error, "Unreadable (is the keychain locked?)".into())
        }
    }
}

impl AuthFeature {
    pub(super) fn reset(&mut self) {
        self.status = None;
        self.distrust();
        self.status_in_flight = false;
        self.login_in_flight = false;
        self.login_stale = self.session.is_some();
        if let Some(session) = &mut self.session {
            session.view = None;
        }
        self.pending
            .retain(|_, pending| matches!(pending, Pending::Command(_)));
        for (id, view) in &self.operations {
            if !view.operation.state.is_final() && !self.lost.contains(id) {
                self.to_check.insert(id.clone());
            }
        }
    }

    pub(super) fn event(&mut self, event: &UiEvent) {
        match event {
            UiEvent::Auth(AuthEvent::CredentialStatusChanged { .. }) => self.status_stale = true,
            UiEvent::Auth(AuthEvent::LoginChanged { login_id }) => {
                if self.session.as_ref().is_some_and(|s| &s.id == login_id) {
                    self.login_stale = true;
                }
            }
            UiEvent::Auth(AuthEvent::AuthOperationCompleted { operation }) => {
                self.operation_update(operation.clone());
            }
            _ => {}
        }
    }

    pub(super) fn reply(&mut self, tag: u64, reply: FeatureReply) {
        let Some(pending) = self.pending.remove(&tag) else {
            return;
        };
        match (pending, reply) {
            (Pending::Status(after_refresh), FeatureReply::Query(result)) => {
                self.status_in_flight = false;
                match result {
                    Ok(QueryReply::Auth(AuthReply::Status(status))) => {
                        self.status = Some(status);
                        if after_refresh {
                            self.trusted = true;
                        }
                    }
                    Ok(_) => {}
                    Err(error) => {
                        self.message = error.message;
                        self.status_stale = true;
                        self.awaiting_status |= after_refresh;
                    }
                }
            }
            (Pending::Operation(id), FeatureReply::Query(result)) => match result {
                Ok(QueryReply::Auth(AuthReply::Operation(view))) => self.operation_update(view),
                Ok(_) => {}
                Err(error) if error.code == Some(ErrorCode::NotFound) => self.operation_lost(&id),
                Err(error) => self.message = error.message,
            },
            (Pending::Login(id), FeatureReply::Query(result)) => {
                self.login_in_flight = false;
                match result {
                    Ok(QueryReply::Auth(AuthReply::Login(view))) => {
                        let ended = view.phase == LoginPhase::Settled;
                        if let Some(session) = self.session.as_mut().filter(|s| s.id == id) {
                            session.view = Some(view);
                        }
                        if ended {
                            self.to_check.insert(id);
                        }
                    }
                    Ok(_) => {}
                    // `not_found` is not "lost": a login still being prepared has
                    // no session yet (the core announces it with `login_changed`,
                    // which asks for it again). Only the operation tells: it is
                    // asked for, and only an operation the core does not know
                    // either is lost.
                    Err(error) => {
                        if error.code != Some(ErrorCode::NotFound) {
                            self.message = error.message;
                        }
                        self.to_check.insert(id);
                    }
                }
            }
            (Pending::Command(Sent::Operation(id)), FeatureReply::Command(result)) => {
                match result {
                    Ok(reply) => {
                        if let Some(CommandResult::Auth(AuthResult::Operation(view))) =
                            reply.result.map(|boxed| *boxed)
                        {
                            let login = view.kind == AuthOperationKind::Login
                                && !view.operation.state.is_final();
                            let profile = self
                                .operations
                                .get(&id)
                                .and_then(|known| known.profile.clone())
                                .or_else(|| view.profile.clone())
                                .unwrap_or_default();
                            self.operation_update(view);
                            if login {
                                self.session = Some(Session {
                                    id,
                                    profile,
                                    view: None,
                                });
                                self.login_stale = true;
                            }
                        }
                    }
                    Err(error) => {
                        self.message = error.message.clone();
                        if error.outcome_unknown {
                            self.to_check.insert(id);
                        } else {
                            self.operations.remove(&id);
                        }
                    }
                }
            }
            (Pending::Command(Sent::Login(id)), FeatureReply::Command(result)) => match result {
                Ok(reply) => match reply.result.map(|boxed| *boxed) {
                    Some(CommandResult::Auth(AuthResult::LoginInput(
                        LoginInputOutcome::Accepted,
                    ))) => {
                        self.message = "Code accepted. Completing sign-in…".into();
                        self.login_stale = true;
                    }
                    Some(CommandResult::Auth(AuthResult::LoginInput(
                        LoginInputOutcome::Rejected { reason },
                    ))) => {
                        self.message = rejection_label(reason).into();
                        self.login_stale = true;
                    }
                    Some(CommandResult::Auth(AuthResult::Operation(view))) => {
                        self.operation_update(view);
                    }
                    _ => {}
                },
                Err(error) => {
                    self.message = error.message;
                    self.to_check.insert(id);
                }
            },
            _ => {}
        }
    }

    /// `shown`: the Providers section is open. The credential status is read for it; a login
    /// being completed and the operations whose end was not seen are looked up whatever is shown,
    /// so a sign-in does not stall behind a closed modal.
    pub(super) fn maintain(&mut self, ctx: &mut FeatureContext<'_>, shown: bool) {
        if shown && !self.trusted && self.trusting.is_none() && !self.running() {
            self.refresh(ctx);
        }
        if shown && self.status_stale && !self.status_in_flight {
            self.status_stale = false;
            self.status_in_flight = true;
            let after_refresh = std::mem::take(&mut self.awaiting_status);
            let tag = self.tag(Pending::Status(after_refresh));
            ctx.query(tag, Query::Auth(AuthQuery::GetCredentialStatus {}));
        }
        if self.login_stale && !self.login_in_flight {
            if let Some(session) = &self.session {
                let id = session.id.clone();
                self.login_stale = false;
                self.login_in_flight = true;
                let tag = self.tag(Pending::Login(id.clone()));
                ctx.query(tag, Query::Auth(AuthQuery::GetLogin { login_id: id }));
            } else {
                self.login_stale = false;
            }
        }
        for id in std::mem::take(&mut self.to_check) {
            let tag = self.tag(Pending::Operation(id.clone()));
            ctx.query(
                tag,
                Query::Auth(AuthQuery::GetAuthOperation { operation_id: id }),
            );
        }
    }
}

#[cfg(test)]
mod tests;
