//! Desktop feature of the `auth` area: the credential of every provider, API
//! keys typed in, and browser logins.
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

use super::layout::bounded_list;
use super::{Availability, Feature, FeatureContext, FeatureId, FeatureReply};
use crate::format::token_expiry;
use crate::panes::provider_widgets::{
    API_KEY_EMPTY_HINT, API_KEY_STORED_HINT, provider_kind_label, provider_type_badge,
};
use workbench_protocol::{
    commands::{Command, CommandResult},
    errors::ErrorCode,
    events::UiEvent,
    handshake::Capability,
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
    Status,
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
    view: Option<LoginView>,
}

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
    now: fn() -> u64,
}

impl Default for AuthFeature {
    fn default() -> Self {
        Self {
            next_tag: 0,
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
            now: unix_now,
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

/// What a slot's state reads as: the label, the absolute expiry for its tooltip, and a note
/// under an expired sign-in.
fn state_label(slot: &CredentialSlot, now: u64) -> (String, Option<String>, Option<&'static str>) {
    match slot.state {
        CredentialState::Unknown => ("Not checked".into(), None, None),
        CredentialState::Missing => ("Missing".into(), None, None),
        CredentialState::Present {
            expires_at_unix: Some(at),
        } => {
            let (label, absolute) = token_expiry(at, now);
            let note =
                (at <= now).then_some("Token expired; it will renew automatically when used.");
            (label, absolute, note)
        }
        CredentialState::Present {
            expires_at_unix: None,
        } if slot.login.is_some() => ("Signed in".into(), None, None),
        CredentialState::Present {
            expires_at_unix: None,
        } => ("Stored".into(), None, None),
        CredentialState::Unavailable => ("Unreadable (is the keychain locked?)".into(), None, None),
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
                profile: None,
                message: None,
            },
        );
        let tag = self.tag(Pending::Command(Sent::Operation(id)));
        ctx.command(tag, Command::Auth(command));
    }

    fn slot_row(&mut self, ui: &mut egui::Ui, ctx: &mut FeatureContext<'_>, slot: &CredentialSlot) {
        let busy = self.running();
        let (state, absolute, note) = state_label(slot, (self.now)());
        ui.horizontal_wrapped(|ui| {
            ui.strong(&slot.profile);
            provider_type_badge(ui, provider_kind_label(slot.kind));
            let expiry = matches!(
                slot.state,
                CredentialState::Present {
                    expires_at_unix: Some(_)
                }
            );
            let label = if expiry {
                ui.label(crate::theme::text::muted(state))
            } else {
                ui.label(state)
            };
            if let Some(absolute) = absolute {
                label.on_hover_text(absolute);
            }
        });
        if let Some(note) = note {
            ui.label(crate::theme::text::muted(note));
        }
        let mut delete = false;
        ui.horizontal_wrapped(|ui| {
            if let Some(method) = slot.login {
                let label = format!("Sign in to {}", method_label(method));
                if ui
                    .add_enabled(!self.login_active() && !busy, egui::Button::new(label))
                    .clicked()
                {
                    let id = new_operation();
                    self.send(
                        ctx,
                        id.clone(),
                        AuthOperationKind::Login,
                        AuthCommand::StartLogin(StartLogin {
                            operation_id: id,
                            profile: slot.profile.clone(),
                        }),
                    );
                }
            }
            match &slot.storage {
                CredentialRef::Env { var } => {
                    ui.label(format!("Read from environment variable {var}."));
                }
                CredentialRef::Keyring { .. } => {
                    let present = matches!(slot.state, CredentialState::Present { .. });
                    // A Cursor profile signs in with a dashboard API key too (the
                    // core exchanges it); the other logins have no key to type.
                    if slot.login.is_none() || slot.login == Some(LoginMethod::Cursor) {
                        let typed = self.keys.entry(slot.profile.clone()).or_default();
                        let hint = if present {
                            API_KEY_STORED_HINT
                        } else {
                            API_KEY_EMPTY_HINT
                        };
                        ui.add(
                            egui::TextEdit::singleline(typed)
                                .password(true)
                                .hint_text(hint),
                        );
                        let empty = typed.trim().is_empty();
                        if ui
                            .add_enabled(!empty && !busy, egui::Button::new("Save API key"))
                            .clicked()
                        {
                            let value = SecretValue::new(std::mem::take(typed));
                            let id = new_operation();
                            self.send(
                                ctx,
                                id.clone(),
                                AuthOperationKind::SaveApiKey,
                                AuthCommand::SaveApiKey(SaveApiKey {
                                    operation_id: id,
                                    profile: slot.profile.clone(),
                                    value,
                                }),
                            );
                        }
                    }
                    if ui
                        .add_enabled(present && !busy, egui::Button::new("Delete credential"))
                        .clicked()
                    {
                        self.confirm_delete = Some(slot.profile.clone());
                    }
                }
            }
        });
        if self.confirm_delete.as_deref() == Some(&slot.profile) {
            ui.horizontal(|ui| {
                ui.label(format!("Delete the credential of {}?", slot.profile));
                delete = ui
                    .add_enabled(!busy, egui::Button::new("Confirm delete"))
                    .clicked();
                if ui.button("Keep credential").clicked() {
                    self.confirm_delete = None;
                }
            });
        }
        if delete {
            self.confirm_delete = None;
            let id = new_operation();
            self.send(
                ctx,
                id.clone(),
                AuthOperationKind::DeleteCredential,
                AuthCommand::DeleteCredential(DeleteCredential {
                    operation_id: id,
                    profile: slot.profile.clone(),
                }),
            );
        }
    }

    fn login_panel(&mut self, ui: &mut egui::Ui, ctx: &mut FeatureContext<'_>) {
        let Some(session) = &self.session else {
            return;
        };
        let id = session.id.clone();
        ui.separator();
        let Some(view) = session.view.clone() else {
            ui.label("Preparing browser sign-in…");
            return;
        };
        ui.strong(format!(
            "{} sign-in ({})",
            view.profile,
            method_label(view.method)
        ));
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
                    ui.hyperlink_to("Open login page", url);
                    if ui.button("Copy link").clicked() {
                        ui.ctx().copy_text(url.clone());
                    }
                }
                if view.accepts_input {
                    ui.label(
                        "The browser callback completes sign-in automatically when the browser runs on the core's machine. Otherwise paste the authorization code or final callback URL below.",
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

impl Feature for AuthFeature {
    fn id(&self) -> FeatureId {
        FeatureId::Auth
    }

    fn title(&self) -> &'static str {
        "Authentication"
    }

    fn availability(&self) -> Availability {
        Availability::Implemented {
            required: &[Capability::Commands, Capability::Queries],
        }
    }

    fn reset(&mut self) {
        self.status = None;
        self.status_stale = true;
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

    fn event(&mut self, event: &UiEvent) {
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

    fn reply(&mut self, tag: u64, reply: FeatureReply) {
        let Some(pending) = self.pending.remove(&tag) else {
            return;
        };
        match (pending, reply) {
            (Pending::Status, FeatureReply::Query(result)) => {
                self.status_in_flight = false;
                match result {
                    Ok(QueryReply::Auth(AuthReply::Status(status))) => self.status = Some(status),
                    Ok(_) => {}
                    Err(error) => {
                        self.message = error.message;
                        self.status_stale = true;
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
                            self.operation_update(view);
                            if login {
                                self.session = Some(Session { id, view: None });
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

    fn render(&mut self, ui: &mut egui::Ui, ctx: &mut FeatureContext<'_>) {
        if self.status_stale && !self.status_in_flight {
            self.status_stale = false;
            self.status_in_flight = true;
            let tag = self.tag(Pending::Status);
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
        if !self.message.is_empty() {
            ui.label(&self.message);
        }
        if ui
            .add_enabled(!self.running(), egui::Button::new("Refresh credentials"))
            .clicked()
        {
            let id = new_operation();
            self.send(
                ctx,
                id.clone(),
                AuthOperationKind::RefreshStatus,
                AuthCommand::RefreshCredentials(RefreshCredentials { operation_id: id }),
            );
        }
        let Some(status) = self.status.clone() else {
            ui.label("Loading credentials…");
            self.login_panel(ui, ctx);
            return;
        };
        if status.slots.is_empty() {
            ui.label("No providers configured.");
        }
        bounded_list(ui, "auth-slots", |ui| {
            for slot in &status.slots {
                ui.push_id(&slot.profile, |ui| self.slot_row(ui, ctx, slot));
            }
        });
        self.login_panel(ui, ctx);
    }
}

#[cfg(test)]
mod tests;
