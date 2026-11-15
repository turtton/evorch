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

fn state_label(state: CredentialState) -> String {
    match state {
        CredentialState::Unknown => "未確認".into(),
        CredentialState::Missing => "未設定".into(),
        CredentialState::Present {
            expires_at_unix: Some(at),
        } => format!("設定済み（期限 UNIX 秒 {at}）"),
        CredentialState::Present {
            expires_at_unix: None,
        } => "設定済み".into(),
        CredentialState::Unavailable => {
            "読み取れません（キーチェーンがロックされていませんか）".into()
        }
    }
}

fn rejection_label(reason: LoginInputRejection) -> &'static str {
    match reason {
        LoginInputRejection::UnknownLogin => "このログインは見つかりません。",
        LoginInputRejection::NotWaiting => "このログインは入力を待っていません。",
        LoginInputRejection::AlreadyUsed => "このログインでは既に入力が使われています。",
        LoginInputRejection::Expired => "ログインの有効期限が切れました。もう一度始めてください。",
        LoginInputRejection::StateMismatch => {
            "このログインのものではない入力です。表示中のリンクからやり直してください。"
        }
        LoginInputRejection::Malformed => {
            "リダイレクト先のアドレス、または表示されたコードを貼り付けてください。"
        }
    }
}

fn failure_label(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::Busy => {
            "ログインを完了できませんでした（ポートが使用中、または接続できません）。"
        }
        ErrorCode::InvalidRequest => {
            "この操作はできません（認証が拒否されたか、対象外のプロファイルです）。"
        }
        ErrorCode::NotFound => "プロファイルが見つかりません。",
        ErrorCode::StorageUnavailable => "資格情報を保存または削除できません。",
        _ => "操作に失敗しました。",
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
            "操作の結果を確認できませんでした（コアが再起動した可能性があります）。資格情報の状態を確認してください。".into();
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
                AuthOperationKind::Login => "ログインしました。".into(),
                AuthOperationKind::SaveApiKey => "API キーを保存しました。".into(),
                AuthOperationKind::DeleteCredential => "資格情報を削除しました。".into(),
                AuthOperationKind::RefreshStatus => String::new(),
            },
            OperationState::Failed { code } => match detail {
                Some(detail) => format!("{} {detail}", failure_label(code)),
                None => failure_label(code).to_owned(),
            },
            OperationState::Cancelled { reason } => match reason {
                CancelReason::Requested => "ログインを取り消しました。".into(),
                CancelReason::Restart | CancelReason::Shutdown => {
                    "再起動または終了のため中断しました。ログインはやり直してください。".into()
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
        ui.horizontal_wrapped(|ui| {
            ui.strong(&slot.profile);
            ui.label(state_label(slot.state));
        });
        ui.horizontal_wrapped(|ui| {
            if let Some(method) = slot.login {
                let label = format!("{} でログイン", method_label(method));
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
                    ui.label(format!("環境変数 {var} から読み込みます。"));
                }
                CredentialRef::Keyring { .. } => {
                    // A Cursor profile signs in with a dashboard API key too (the
                    // core exchanges it); the other logins have no key to type.
                    if slot.login.is_none() || slot.login == Some(LoginMethod::Cursor) {
                        let typed = self.keys.entry(slot.profile.clone()).or_default();
                        ui.add(
                            egui::TextEdit::singleline(typed)
                                .password(true)
                                .hint_text("API キー"),
                        );
                        let empty = typed.trim().is_empty();
                        if ui
                            .add_enabled(!empty && !busy, egui::Button::new("キーを保存"))
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
                    let present = matches!(slot.state, CredentialState::Present { .. });
                    if ui
                        .add_enabled(present && !busy, egui::Button::new("削除"))
                        .clicked()
                    {
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
            }
        });
    }

    fn login_panel(&mut self, ui: &mut egui::Ui, ctx: &mut FeatureContext<'_>) {
        let Some(session) = &self.session else {
            return;
        };
        let id = session.id.clone();
        ui.separator();
        let Some(view) = session.view.clone() else {
            ui.label("ログインを準備しています。");
            return;
        };
        ui.strong(format!(
            "{} のログイン（{}）",
            view.profile,
            method_label(view.method)
        ));
        match view.phase {
            LoginPhase::Completing => {
                ui.label("認証情報を取得して保存しています…");
            }
            LoginPhase::Settled => {
                ui.label("ログインは終了しました。");
            }
            LoginPhase::Waiting => {
                if let Some(url) = &view.authorize_url {
                    ui.label("次のリンクをブラウザで開き、ログインしてください。");
                    ui.hyperlink_to("ログインページを開く", url);
                    if ui.button("リンクをコピー").clicked() {
                        ui.ctx().copy_text(url.clone());
                    }
                }
                if view.accepts_input {
                    ui.label(
                        "ブラウザがこのコアと別のマシンで動いている場合は、ログイン後に表示されたアドレス（またはコード）をここに貼り付けてください。",
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut self.paste)
                            .password(true)
                            .hint_text("リダイレクト先のアドレスまたはコード"),
                    );
                    if ui
                        .add_enabled(!self.paste.trim().is_empty(), egui::Button::new("送信"))
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
        if ui.button("ログインを取り消す").clicked() {
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
                    Err(error) if error.code == Some(ErrorCode::NotFound) => {
                        self.operation_lost(&id);
                    }
                    Err(error) => {
                        self.message = error.message;
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
                        self.message = "入力を受け付けました。ログインを完了しています…".into();
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
            .add_enabled(!self.running(), egui::Button::new("資格情報を読み直す"))
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
            ui.label("資格情報を読み込んでいます。");
            self.login_panel(ui, ctx);
            return;
        };
        if status.slots.is_empty() {
            ui.label("プロバイダーが設定されていません。");
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
