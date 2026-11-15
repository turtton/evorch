//! Desktop feature of the `settings` area: the configuration as the core holds
//! it, the providers in it, an editor for the whole document, and the reload
//! that makes the running core follow a save.
//!
//! The screen never writes a file. It sends commands that quote the revision
//! the draft was made from, so a save over a newer file is refused by the core
//! (`stale_revision`) and the draft is kept. *Saved* and *applied* are shown
//! apart: a save does not change a run that already sent a request, and only
//! `reload_models` makes later runs use it.
//!
//! As in the other areas an event only says something changed; the snapshot or
//! the operation is then asked for again, and so is every operation whose end
//! was not seen (after a reconnect, or when a reply was lost).
use std::collections::{BTreeMap, BTreeSet};

use super::layout::bounded_list;
use super::{Availability, Feature, FeatureContext, FeatureId, FeatureReply};
use workbench_protocol::{
    commands::{Command, CommandResult},
    errors::ErrorCode,
    events::UiEvent,
    handshake::Capability,
    ids::OperationId,
    namespaces::settings::{
        DeleteProvider, ProviderView, RefreshSettings, ReloadModels, SaveDocument, SettingsCommand,
        SettingsEvent, SettingsOperationKind, SettingsOperationView, SettingsProblem,
        SettingsQuery, SettingsReply, SettingsResult, SettingsRevision, SettingsSnapshot,
    },
    operation::{CancelReason, OperationState},
    queries::{Query, QueryReply},
};

#[derive(Debug, Clone)]
enum Pending {
    Snapshot,
    Operation,
    Command(OperationId),
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
    status: String,
    stale: bool,
    snapshot_in_flight: bool,
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
            status: String::new(),
            stale: true,
            snapshot_in_flight: false,
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

fn provider_row(ui: &mut egui::Ui, provider: &ProviderView) -> bool {
    let mut delete = false;
    ui.horizontal_wrapped(|ui| {
        ui.strong(&provider.name);
        ui.label(format!(
            "{} / {} / モデル {} 件 / 既定 {}",
            provider.base_url,
            format!("{:?}", provider.kind).to_lowercase(),
            provider.models.len(),
            provider.default_model
        ));
        delete = ui.button("削除").clicked();
    });
    delete
}

impl SettingsFeature {
    fn tag(&mut self, pending: Pending) -> u64 {
        self.next_tag += 1;
        self.pending.insert(self.next_tag, pending);
        self.next_tag
    }

    fn running(&self) -> bool {
        self.operations
            .values()
            .any(|view| !view.operation.state.is_final())
    }

    /// Record what the core says of an operation of this screen.
    fn operation_update(&mut self, view: SettingsOperationView) {
        let id = view.operation.operation_id.clone();
        if !self.operations.contains_key(&id) {
            return;
        }
        let state = view.operation.state;
        let kind = view.kind;
        let message = view.message.clone();
        self.operations.insert(id, view);
        match state {
            OperationState::Pending => {}
            OperationState::Ready => {
                self.status = match kind {
                    SettingsOperationKind::ReloadModels => {
                        "モデルを再読込しました。以降に始まる実行から反映されます。".into()
                    }
                    SettingsOperationKind::Refresh => String::new(),
                    _ => "保存しました。反映するにはモデルを再読込してください。".into(),
                };
                if kind == SettingsOperationKind::SaveDocument {
                    self.draft = None;
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
        self.pending
            .retain(|_, pending| matches!(pending, Pending::Command(_)));
        for (id, view) in &self.operations {
            if !view.operation.state.is_final() {
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
            (Pending::Operation, FeatureReply::Query(result)) => match result {
                Ok(QueryReply::Settings(SettingsReply::Operation(view))) => {
                    self.operation_update(view);
                }
                Ok(_) => {}
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
                    }
                }
            },
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
            let tag = self.tag(Pending::Operation);
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
        let unapplied = snapshot.revision != snapshot.applied_revision;
        ui.horizontal_wrapped(|ui| {
            ui.label(if unapplied {
                "保存済みの設定は、まだ実行中のコアに適用されていません。"
            } else {
                "保存済みの設定は適用済みです。"
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
        bounded_list(ui, "settings-providers", |ui| {
            if snapshot.providers.is_empty() {
                ui.label("プロバイダーはありません。");
            }
            for provider in &snapshot.providers {
                ui.push_id(&provider.name, |ui| {
                    if provider_row(ui, provider) {
                        delete = Some(provider.name.clone());
                    }
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
