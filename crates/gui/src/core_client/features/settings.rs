//! Desktop feature of the `settings` area: the configuration as the core holds
//! it, the providers in it, an editor for the whole document, and the reload
//! that makes the running core follow a save.
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

use super::layout::bounded_list;
use super::{Availability, Feature, FeatureContext, FeatureId, FeatureReply};
use workbench_protocol::{
    commands::{Command, CommandResult, SetAgentWebToolsEnabled},
    errors::ErrorCode,
    events::UiEvent,
    handshake::Capability,
    ids::OperationId,
    namespaces::ExecutionCommand,
    namespaces::settings::{
        DeleteProvider, ProviderView, RefreshSettings, ReloadModels, SaveDocument, SaveProvider,
        SettingsCommand, SettingsEvent, SettingsOperationKind, SettingsOperationView,
        SettingsProblem, SettingsQuery, SettingsReply, SettingsResult, SettingsRevision,
        SettingsSnapshot,
    },
    operation::{CancelReason, OperationState},
    queries::{Query, QueryReply},
};

mod provider_form;
use provider_form::{FormAction, ProviderForm};

#[derive(Debug, Clone)]
enum Pending {
    Snapshot,
    Operation(OperationId),
    Command(OperationId),
    /// A change of the Agent web tools of the running core (an execution command).
    WebTools(bool),
    /// The same change after its reply was lost: kept for the result the core recorded for it,
    /// which is shown when it arrives; it no longer disables the buttons (nothing is resent).
    WebToolsUnknown(bool),
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
    web_tools_status: String,
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
            web_tools_status: String::new(),
        }
    }
}

fn new_operation() -> OperationId {
    OperationId::new(format!("gui-settings-{}", uuid::Uuid::new_v4())).expect("a uuid fits")
}

fn problem_label(problem: SettingsProblem) -> &'static str {
    match problem {
        SettingsProblem::Unreadable => "設定ファイルを読めません。",
        SettingsProblem::InvalidConfig => "設定ファイルの内容が不正です。",
        SettingsProblem::DocumentTooLarge => "設定ファイルが大きすぎて編集できません。",
    }
}

fn failure_label(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::StaleRevision => {
            "他の場所で設定が変更されました。読み込み直してから保存してください。"
        }
        ErrorCode::PermissionDenied => {
            "再起動対象（core.restart.targets）はこの画面から変更できません。コアを動かしているマシンの設定ファイルで変更してください。"
        }
        ErrorCode::InvalidRequest => "設定を保存できませんでした（内容が不正です）。",
        ErrorCode::StorageUnavailable => "設定ファイルに書き込めません。",
        _ => "操作に失敗しました。",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowAction {
    None,
    Edit,
    Delete,
}

fn provider_row(ui: &mut egui::Ui, provider: &ProviderView) -> RowAction {
    let mut action = RowAction::None;
    ui.horizontal_wrapped(|ui| {
        ui.strong(&provider.name);
        ui.label(format!(
            "{} / {} / モデル {} 件 / 既定 {}",
            provider.base_url,
            format!("{:?}", provider.kind).to_lowercase(),
            provider.models.len(),
            provider.default_model
        ));
        let editable = ProviderForm::from_view(provider).is_some();
        if ui
            .add_enabled(editable, egui::Button::new("編集"))
            .on_disabled_hover_text("この種類のプロバイダーは設定ファイルで編集してください。")
            .clicked()
        {
            action = RowAction::Edit;
        }
        if ui.button("削除").clicked() {
            action = RowAction::Delete;
        }
    });
    action
}

impl SettingsFeature {
    fn tag(&mut self, pending: Pending) -> u64 {
        self.next_tag += 1;
        self.pending.insert(self.next_tag, pending);
        self.next_tag
    }

    fn running(&self) -> bool {
        self.operations
            .iter()
            .any(|(id, view)| !view.operation.state.is_final() && !self.lost.contains(id))
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
        self.saving.remove(id);
        if self.provider_saving.as_ref() == Some(id) {
            self.provider_saving = None;
        }
        self.stale = true;
        self.status =
            "操作の結果を確認できませんでした（コアが再起動した可能性があります）。保存済みの内容を確認してください。編集中の内容は残してあります。"
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
        if state.is_final() && !matches!(state, OperationState::Ready) {
            self.saving.remove(&id);
        }
        if state.is_final() && self.provider_saving.as_ref() == Some(&id) {
            self.provider_saving = None;
            // A saved provider closes the form; a refused one keeps what was typed.
            let saved_form = self.saving_form.take();
            if matches!(state, OperationState::Ready)
                && self.form.as_ref().map(|form| form.serial) == saved_form
            {
                self.form = None;
            }
        }
        match state {
            OperationState::Pending => {}
            OperationState::Ready => {
                self.status = match kind {
                    SettingsOperationKind::ReloadModels => {
                        "モデルを再読込しました。以降のリクエストから反映されます（送信済みの内容は変わりません）。".into()
                    }
                    SettingsOperationKind::Refresh => String::new(),
                    _ => "保存しました。反映するにはモデルを再読込してください。".into(),
                };
                if kind == SettingsOperationKind::SaveDocument {
                    self.saved_document(&id, revision);
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
                self.stale = true;
            }
            OperationState::Cancelled { reason } => {
                self.status = match reason {
                    CancelReason::Requested => "操作を取り消しました。",
                    CancelReason::Restart | CancelReason::Shutdown => {
                        "再起動または終了のため操作は行われませんでした。"
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
                "他の場所で設定が変更されました。編集中の内容を保存するには読み込み直してください。"
                    .into();
        }
        self.snapshot = Some(snapshot);
    }

    fn send(&mut self, ctx: &mut FeatureContext<'_>, command: SettingsCommand) {
        let id = command.operation_id().clone();
        let kind = command.kind();
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
}

impl Feature for SettingsFeature {
    fn id(&self) -> FeatureId {
        FeatureId::Settings
    }

    fn title(&self) -> &'static str {
        "Settings"
    }

    fn availability(&self) -> Availability {
        Availability::Implemented {
            required: &[Capability::Commands, Capability::Queries],
        }
    }

    fn reset(&mut self) {
        self.snapshot = None;
        self.snapshot_in_flight = false;
        self.stale = true;
        // What was asked before the snapshot is answered, if at all, by a reply
        // that is no longer wanted; the operations are asked for again.
        // A web tools change whose reply was lost is kept: the resolved reply for its tag
        // follows the reconnect, and is shown.
        self.pending.retain(|_, pending| {
            matches!(
                pending,
                Pending::Command(_) | Pending::WebTools(_) | Pending::WebToolsUnknown(_)
            )
        });
        for (id, view) in &self.operations {
            if !view.operation.state.is_final() && !self.lost.contains(id) {
                self.to_check.insert(id.clone());
            }
        }
    }

    fn event(&mut self, event: &UiEvent) {
        match event {
            UiEvent::Settings(SettingsEvent::SettingsSnapshotChanged { .. }) => {
                self.stale = true;
            }
            UiEvent::Settings(SettingsEvent::SettingsOperationCompleted { operation }) => {
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
            (Pending::Operation(id), FeatureReply::Query(result)) => match result {
                Ok(QueryReply::Settings(SettingsReply::Operation(view))) => {
                    self.operation_update(view);
                }
                Ok(_) => {}
                Err(error) if error.code == Some(ErrorCode::NotFound) => self.operation_lost(&id),
                Err(error) => self.status = error.message,
            },
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
                    if error.outcome_unknown {
                        // The command may have been applied: its operation is
                        // asked for, not sent again.
                        self.to_check.insert(id);
                    } else {
                        self.operations.remove(&id);
                        self.saving.remove(&id);
                        if self.provider_saving.as_ref() == Some(&id) {
                            self.provider_saving = None;
                        }
                    }
                }
            },
            (
                Pending::WebTools(enabled) | Pending::WebToolsUnknown(enabled),
                FeatureReply::Command(result),
            ) => {
                let word = if enabled { "有効" } else { "無効" };
                self.web_tools_status = match result {
                    Ok(_) => format!(
                        "Agent の Web ツールを{word}にしました（実行中のコアへの設定です。すでに開始した run が送ったツール定義は変わりません）。"
                    ),
                    Err(error) if error.outcome_unknown => {
                        // The core's recorded result for this tag follows after a reconnect, if it
                        // has one; until then the buttons work again and the
                        // command is not sent again by this screen.
                        self.pending.insert(tag, Pending::WebToolsUnknown(enabled));
                        format!(
                            "Agent の Web ツールの切り替えは結果不明です（再送していません）。必要なら選び直してください — {}",
                            error.message
                        )
                    }
                    Err(error) => format!(
                        "Agent の Web ツールを{word}にできませんでした: {}",
                        error.message
                    ),
                };
            }
            _ => {}
        }
    }

    fn render(&mut self, ui: &mut egui::Ui, ctx: &mut FeatureContext<'_>) {
        if self.stale && !self.snapshot_in_flight {
            self.stale = false;
            self.snapshot_in_flight = true;
            let tag = self.tag(Pending::Snapshot);
            ctx.query(tag, Query::Settings(SettingsQuery::GetSettingsSnapshot {}));
        }
        for id in std::mem::take(&mut self.to_check) {
            let tag = self.tag(Pending::Operation(id.clone()));
            ctx.query(
                tag,
                Query::Settings(SettingsQuery::GetSettingsOperation { operation_id: id }),
            );
        }
        if !self.status.is_empty() {
            ui.label(&self.status);
        }
        let Some(snapshot) = self.snapshot.clone() else {
            ui.label("設定を読み込んでいます。");
            return;
        };
        if let Some(problem) = snapshot.problem {
            ui.colored_label(ui.visuals().warn_fg_color, problem_label(problem));
        }
        let busy = self.running();
        ui.horizontal_wrapped(|ui| {
            // Unknown (a reload failed part-way) is not "applied", whatever the
            // saved file looks like: the models may be a mixture.
            ui.label(if snapshot.applied_revision.is_none() {
                match &snapshot.apply_failure {
                    Some(reason) => format!(
                        "適用状態が不明です（直近の再読込が失敗しました: {reason}）。モデルを再読込してください。"
                    ),
                    None => "適用状態が不明です。モデルを再読込してください。".to_owned(),
                }
            } else if snapshot.pending_apply() {
                "保存済みの設定は、まだ実行中のコアに適用されていません。".to_owned()
            } else {
                "保存済みの設定は適用済みです。".to_owned()
            });
            if ui
                .add_enabled(!busy, egui::Button::new("モデルを再読込"))
                .clicked()
            {
                let operation_id = new_operation();
                self.send(
                    ctx,
                    SettingsCommand::ReloadModels(ReloadModels { operation_id }),
                );
            }
            if ui
                .add_enabled(!busy, egui::Button::new("読み直す"))
                .clicked()
            {
                self.draft = None;
                self.stale = true;
                let operation_id = new_operation();
                self.send(
                    ctx,
                    SettingsCommand::RefreshSettings(RefreshSettings { operation_id }),
                );
            }
        });
        if busy {
            ui.label("実行中…");
        }
        ui.separator();
        ui.strong("プロバイダー");
        let mut delete = None;
        let mut edit = None;
        bounded_list(ui, "settings-providers", |ui| {
            if snapshot.providers.is_empty() {
                ui.label("プロバイダーはありません。");
            }
            for provider in &snapshot.providers {
                ui.push_id(&provider.name, |ui| match provider_row(ui, provider) {
                    RowAction::None => {}
                    RowAction::Edit => edit = Some(provider.clone()),
                    RowAction::Delete => delete = Some(provider.name.clone()),
                });
            }
        });
        if let Some(name) = delete {
            let operation_id = new_operation();
            self.send(
                ctx,
                SettingsCommand::DeleteProvider(DeleteProvider {
                    operation_id,
                    expected_revision: snapshot.revision.clone(),
                    name,
                }),
            );
        }
        // The form is opened from the revision this snapshot has; the save quotes that one.
        // A form is not replaced while its save runs.
        if let Some(provider) = edit.filter(|_| self.provider_saving.is_none()) {
            self.form_serial += 1;
            self.form = ProviderForm::from_view(&provider).map(|mut form| {
                form.based_on = Some(snapshot.revision.clone());
                form.serial = self.form_serial;
                form
            });
        }
        if self.form.is_none()
            && ui
                .add_enabled(!busy, egui::Button::new("プロバイダーを追加"))
                .clicked()
        {
            self.form_serial += 1;
            self.form = Some(ProviderForm {
                based_on: Some(snapshot.revision.clone()),
                serial: self.form_serial,
                ..ProviderForm::default()
            });
        }
        let mut reread = false;
        if let Some(form) = &self.form
            && form
                .based_on
                .as_ref()
                .is_some_and(|based| based != &snapshot.revision)
        {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                "設定ファイルがこのフォームを開いた後に更新されています。保存は拒否されます。",
            );
            reread = ui.button("最新の内容で開き直す").clicked();
        }
        if reread {
            let current = self.form.take();
            self.form_serial += 1;
            self.form = current.and_then(|old| {
                let mut form = match &old.original {
                    Some(name) => snapshot
                        .providers
                        .iter()
                        .find(|provider| &provider.name == name)
                        .and_then(ProviderForm::from_view)?,
                    None => ProviderForm {
                        based_on: None,
                        ..old
                    },
                };
                form.based_on = Some(snapshot.revision.clone());
                form.serial = self.form_serial;
                Some(form)
            });
        }
        let mut form_action = FormAction::None;
        if let Some(form) = &mut self.form {
            form_action = form.draw(ui, busy);
        }
        match form_action {
            FormAction::None => {}
            FormAction::Cancel => self.form = None,
            FormAction::Save => {
                if let Some(form) = &self.form {
                    match form.to_input() {
                        Ok(provider) => {
                            let operation_id = new_operation();
                            self.provider_saving = Some(operation_id.clone());
                            self.saving_form = Some(form.serial);
                            let expected_revision = form
                                .based_on
                                .clone()
                                .unwrap_or_else(|| snapshot.revision.clone());
                            let original_name = form.original.clone();
                            self.send(
                                ctx,
                                SettingsCommand::SaveProvider(Box::new(SaveProvider {
                                    operation_id,
                                    expected_revision,
                                    original_name,
                                    provider,
                                })),
                            );
                        }
                        Err(missing) => self.status = missing,
                    }
                }
            }
        }
        ui.separator();
        ui.strong("Agent の Web ツール");
        ui.label("実行中のコアに設定します。設定ファイルは変更せず、コアを再起動すると設定ファイルの値に戻ります。現在の値はこの画面では読み取れません。");
        let web_waiting = self
            .pending
            .values()
            .any(|pending| matches!(pending, Pending::WebTools(_)));
        let mut web_tools = None;
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!web_waiting, egui::Button::new("Web ツールを有効にする"))
                .clicked()
            {
                web_tools = Some(true);
            }
            if ui
                .add_enabled(!web_waiting, egui::Button::new("Web ツールを無効にする"))
                .clicked()
            {
                web_tools = Some(false);
            }
        });
        if !self.web_tools_status.is_empty() {
            ui.label(&self.web_tools_status);
        }
        if let Some(enabled) = web_tools {
            let tag = self.tag(Pending::WebTools(enabled));
            ctx.command(
                tag,
                Command::Execution(ExecutionCommand::SetAgentWebToolsEnabled(
                    SetAgentWebToolsEnabled { enabled },
                )),
            );
        }
        ui.separator();
        ui.strong("設定ファイル");
        let mut save = None;
        let mut discard = false;
        if let Some(draft) = &mut self.draft {
            egui::ScrollArea::vertical()
                .id_salt("settings-document")
                .max_height(320.0)
                .show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(&mut draft.text)
                            .code_editor()
                            .desired_width(f32::INFINITY),
                    );
                });
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(draft.dirty() && !busy, egui::Button::new("保存"))
                    .clicked()
                {
                    save = Some((draft.text.clone(), draft.based_on.clone()));
                }
                if ui
                    .add_enabled(draft.dirty(), egui::Button::new("変更を破棄"))
                    .clicked()
                {
                    discard = true;
                }
            });
        }
        if discard {
            self.draft = None;
            self.stale = true;
        }
        if let Some((text, expected_revision)) = save {
            let operation_id = new_operation();
            self.saving.insert(operation_id.clone(), text.clone());
            self.send(
                ctx,
                SettingsCommand::SaveDocument(SaveDocument {
                    operation_id,
                    expected_revision,
                    text,
                }),
            );
        }
    }
}

#[cfg(test)]
mod tests;
