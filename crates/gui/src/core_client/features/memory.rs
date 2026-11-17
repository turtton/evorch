//! Desktop feature of the `memory` area: the lessons of the selected project,
//! filtered by status, favorite and words, one lesson opened for its text and
//! evidence (PR 13), and the updates of a lesson (PR 14): favorite, validate,
//! promote, reject.
//!
//! An update is sent with the revision the shown lesson had. What the core
//! refuses (a stale revision, a lesson that is gone, an action the state does
//! not allow) is worded and the lesson is read again; it is never resent. An
//! update whose outcome is unknown (the reply was lost, the core restarted) is
//! asked for by its operation id, not sent again, and is not counted as done.
//!
//! The reads are operations ([`super::diagnostics::read`]). An answer for a project
//! or a lesson that is no longer the one shown is dropped, and a lesson's body is
//! only what the core cut for the wire.
use std::collections::{BTreeMap, BTreeSet};

use super::diagnostics::read::{Phase, Read, phase_line};
use super::layout::paged_list;
use super::{Availability, Feature, FeatureContext, FeatureId, FeatureReply};
use workbench_protocol::{
    commands::{Command, CommandResult},
    errors::ErrorCode,
    events::UiEvent,
    handshake::Capability,
    ids::{MemoryId, OperationId, PageCursor, ProjectId},
    management::{
        ActionAvailability, ActionState, DenialReason, MemoryAction, MemoryRef, MemoryState,
        MemoryStatus,
    },
    namespaces::{
        diagnostics::{BoundedText, OperationTarget},
        memory::{
            GetMemory, ListMemory, MemoryCommand, MemoryDetail, MemoryEvent, MemoryQuery,
            MemoryReply, MemoryResult, MemoryScope, MemorySummary, MemoryUpdateView, UpdateMemory,
        },
    },
    operation::OperationState,
    queries::{PageRequest, Query, QueryReply},
};

const STATUSES: [(Option<MemoryStatus>, &str); 5] = [
    (None, "すべて"),
    (Some(MemoryStatus::Candidate), "候補"),
    (Some(MemoryStatus::Validated), "検証済み"),
    (Some(MemoryStatus::Promoted), "昇格済み"),
    (Some(MemoryStatus::Rejected), "却下"),
];

#[derive(Debug, Clone)]
enum Pending {
    List {
        generation: u64,
        append: bool,
    },
    Detail {
        generation: u64,
    },
    /// An update sent; its answer is the operation, not the end.
    Command(OperationId),
    /// An update asked for by its operation id.
    Update(OperationId),
}

#[derive(Default)]
pub struct MemoryFeature {
    next_tag: u64,
    pending: BTreeMap<u64, Pending>,
    project: Option<ProjectId>,
    status: Option<MemoryStatus>,
    favorite_only: bool,
    words: String,
    list_read: Read,
    list_request: Option<ListMemory>,
    items: Vec<MemorySummary>,
    next_cursor: Option<PageCursor>,
    list_due: bool,
    more_requested: bool,
    selected: Option<MemoryId>,
    detail_read: Read,
    detail_request: Option<GetMemory>,
    detail: Option<MemoryDetail>,
    detail_due: bool,
    /// The updates sent whose end is not known yet.
    in_flight: BTreeSet<OperationId>,
    /// The updates whose outcome is unknown, asked for at the next frame.
    to_check: BTreeSet<OperationId>,
    update_line: String,
}

fn new_operation() -> OperationId {
    OperationId::new(format!("gui-memory-{}", uuid::Uuid::new_v4())).expect("a uuid fits")
}

fn action_label(action: MemoryAction) -> &'static str {
    match action {
        MemoryAction::SetFavorite => "お気に入りにする",
        MemoryAction::ClearFavorite => "お気に入りを外す",
        MemoryAction::Validate => "検証済みにする",
        MemoryAction::Promote => "昇格する",
        MemoryAction::Reject => "却下する",
    }
}

/// Why an action is not offered, in words. Shared with the improvement area.
pub(super) fn denial_label(reason: DenialReason) -> &'static str {
    match reason {
        DenialReason::NotApplicableInState => "今の状態ではできません。",
        DenialReason::FavoriteMustBeCleared => {
            "お気に入りのままは却下できません。先にお気に入りを外してください。"
        }
        DenialReason::AlreadyResolved => "すでに決定済みです。",
        DenialReason::OperationInProgress => "他の操作が実行中です。",
        DenialReason::ReadOnly => "読み取り専用です。",
    }
}

/// The end of an update in words: what the core did, or what the user should do
/// next. A stale revision and a lost target both ask to read again; neither
/// resends. Shared with the improvement area.
pub(super) fn update_line(
    state: OperationState,
    changed: Option<bool>,
    denial: Option<DenialReason>,
    message: Option<&str>,
) -> String {
    match state {
        OperationState::Pending => "更新中…".to_owned(),
        OperationState::Ready if changed == Some(false) => {
            "すでにその状態でした（変更はありません）。".to_owned()
        }
        OperationState::Ready => "更新しました。".to_owned(),
        OperationState::Cancelled { .. } => {
            "更新は取り消されました。状態を再読み込みして確認してください。".to_owned()
        }
        OperationState::Failed { code } => match (code, denial) {
            (_, Some(reason)) => denial_label(reason).to_owned(),
            (ErrorCode::StaleRevision, _) => {
                "他の場所で更新されていました。読み込み直したので、内容を確認してもう一度操作してください。"
                    .to_owned()
            }
            (ErrorCode::NotFound, _) => "対象が見つかりません。".to_owned(),
            _ => match message {
                Some(message) => format!("更新できませんでした: {message}"),
                None => "更新できませんでした。".to_owned(),
            },
        },
    }
}

/// The line for an update whose outcome cannot be known: it is not "done".
pub(super) const UNKNOWN_LINE: &str = "更新の結果が分かりません（再接続やコアの再起動など）。成功とは扱いません。再読み込みして状態を確認してください。";

fn status_label(status: MemoryStatus) -> &'static str {
    STATUSES
        .iter()
        .find(|(candidate, _)| *candidate == Some(status))
        .map_or("", |(_, label)| label)
}

fn scope_label(scope: MemoryScope) -> &'static str {
    match scope {
        MemoryScope::Project => "プロジェクト",
        MemoryScope::Harness => "ハーネス",
        MemoryScope::User => "ユーザー",
    }
}

fn body(ui: &mut egui::Ui, heading: &str, text: &BoundedText) {
    ui.strong(heading);
    ui.label(&text.text);
    if text.truncated {
        ui.label(format!(
            "(先頭 {} バイトのみ表示 / 全体 {} バイト)",
            text.text.len(),
            text.total_bytes
        ));
    }
}

impl MemoryFeature {
    fn tag(&mut self, pending: Pending) -> u64 {
        self.next_tag += 1;
        self.pending.insert(self.next_tag, pending);
        self.next_tag
    }

    /// An update ended or changed. Ours is worded; anyone's makes the shown
    /// project's lessons stale.
    fn update_arrived(&mut self, view: &MemoryUpdateView) {
        if !view.operation.state.is_final() {
            return;
        }
        let id = &view.operation.operation_id;
        let ours = self.in_flight.remove(id);
        self.to_check.remove(id);
        if ours {
            self.update_line = update_line(
                view.operation.state,
                view.changed,
                view.denial,
                view.message.as_deref(),
            );
        }
        if self.project.as_ref() == Some(&view.project) {
            self.list_due = true;
            self.detail_due = self.selected.is_some();
        }
    }

    /// The outcome of an update is not known: asked for, never resent.
    fn outcome_lost(&mut self, id: &OperationId) {
        self.in_flight.remove(id);
        self.to_check.remove(id);
        UNKNOWN_LINE.clone_into(&mut self.update_line);
        self.list_due = true;
        self.detail_due = self.selected.is_some();
    }

    fn send(
        &mut self,
        ctx: &mut FeatureContext<'_>,
        project: ProjectId,
        state: &MemoryState,
        action: MemoryAction,
    ) {
        let update = UpdateMemory {
            operation_id: new_operation(),
            project,
            target: MemoryRef {
                id: state.id.clone(),
                // What the read said, as it said it.
                expected_revision: state.revision,
            },
        };
        let id = update.operation_id.clone();
        let command = match action {
            MemoryAction::SetFavorite => MemoryCommand::SetMemoryFavorite(update),
            MemoryAction::ClearFavorite => MemoryCommand::ClearMemoryFavorite(update),
            MemoryAction::Validate => MemoryCommand::ValidateMemory(update),
            MemoryAction::Promote => MemoryCommand::PromoteMemory(update),
            MemoryAction::Reject => MemoryCommand::RejectMemory(update),
        };
        self.in_flight.insert(id.clone());
        "更新中…".clone_into(&mut self.update_line);
        let tag = self.tag(Pending::Command(id));
        ctx.command(tag, Command::Memory(command));
    }

    fn switch_project(&mut self, project: Option<ProjectId>) {
        self.project = project;
        self.list_read.forget();
        self.detail_read.forget();
        self.list_request = None;
        self.detail_request = None;
        self.items.clear();
        self.next_cursor = None;
        self.selected = None;
        self.detail = None;
        self.list_due = self.project.is_some();
        self.detail_due = false;
    }

    fn begin_list(&mut self, ctx: &mut FeatureContext<'_>, project: ProjectId) {
        self.items.clear();
        self.next_cursor = None;
        let request = ListMemory {
            operation_id: self.list_read.begin("memory-list"),
            project,
            status: self.status,
            favorite: self.favorite_only.then_some(true),
            text: Some(self.words.trim().to_owned()).filter(|words| !words.is_empty()),
            page: PageRequest::default(),
        };
        self.ask_list(ctx, request, None);
    }

    fn ask_list(
        &mut self,
        ctx: &mut FeatureContext<'_>,
        mut request: ListMemory,
        cursor: Option<PageCursor>,
    ) {
        let append = cursor.is_some();
        request.page = PageRequest {
            cursor,
            ..PageRequest::default()
        };
        let tag = self.tag(Pending::List {
            generation: self.list_read.generation(),
            append,
        });
        ctx.query(tag, Query::Memory(MemoryQuery::ListMemory(request.clone())));
        request.page = PageRequest::default();
        self.list_request = Some(request);
    }

    fn begin_detail(&mut self, ctx: &mut FeatureContext<'_>, project: ProjectId, id: MemoryId) {
        self.detail = None;
        let request = GetMemory {
            operation_id: self.detail_read.begin("memory-detail"),
            project,
            id,
        };
        self.ask_detail(ctx, request);
    }

    fn ask_detail(&mut self, ctx: &mut FeatureContext<'_>, request: GetMemory) {
        let tag = self.tag(Pending::Detail {
            generation: self.detail_read.generation(),
        });
        ctx.query(tag, Query::Memory(MemoryQuery::GetMemory(request.clone())));
        self.detail_request = Some(request);
    }
}

impl Feature for MemoryFeature {
    fn id(&self) -> FeatureId {
        FeatureId::Memory
    }

    fn title(&self) -> &'static str {
        "Memory"
    }

    fn availability(&self) -> Availability {
        Availability::Implemented {
            required: &[Capability::Queries],
        }
    }

    fn reset(&mut self) {
        // A command in flight keeps its tag: its reply, or the loss of it, says
        // what became of the update. The others are asked for by operation id.
        self.pending
            .retain(|_, pending| matches!(pending, Pending::Command(_)));
        for id in &self.in_flight {
            let sent = self
                .pending
                .values()
                .any(|pending| matches!(pending, Pending::Command(sent) if sent == id));
            if !sent {
                self.to_check.insert(id.clone());
            }
        }
        self.list_read.forget();
        self.detail_read.forget();
        self.items.clear();
        self.next_cursor = None;
        self.detail = None;
        self.list_due = self.project.is_some();
        self.detail_due = self.selected.is_some();
    }

    fn event(&mut self, event: &UiEvent) {
        match event {
            UiEvent::Memory(MemoryEvent::MemoryOperationChanged { view }) => {
                self.list_read.event(view);
                self.detail_read.event(view);
            }
            UiEvent::Memory(MemoryEvent::MemoryUpdateCompleted { update }) => {
                self.update_arrived(update);
            }
            _ => {}
        }
    }

    fn reply(&mut self, tag: u64, reply: FeatureReply) {
        let Some(pending) = self.pending.remove(&tag) else {
            return;
        };
        let result = match reply {
            FeatureReply::Command(result) => {
                let Pending::Command(id) = pending else {
                    return;
                };
                match result {
                    Ok(reply) => {
                        if let Some(CommandResult::Memory(MemoryResult::Operation(view))) =
                            reply.result.map(|boxed| *boxed)
                        {
                            self.update_arrived(&view);
                        }
                    }
                    Err(error) if error.outcome_unknown => {
                        // It may have been applied: asked for, not sent again.
                        "更新の結果を確認しています…".clone_into(&mut self.update_line);
                        self.to_check.insert(id);
                    }
                    Err(error) => {
                        self.in_flight.remove(&id);
                        self.update_line = if error.code == Some(ErrorCode::StaleRevision) {
                            update_line(
                                OperationState::Failed {
                                    code: ErrorCode::StaleRevision,
                                },
                                None,
                                None,
                                None,
                            )
                        } else {
                            format!("更新できませんでした: {}", error.message)
                        };
                        self.list_due = true;
                        self.detail_due = self.selected.is_some();
                    }
                }
                return;
            }
            FeatureReply::Query(result) => result,
        };
        if let Pending::Update(id) = &pending {
            match result {
                Ok(QueryReply::Memory(MemoryReply::Update(view))) => self.update_arrived(&view),
                Ok(_) => {}
                // The core does not know it: it restarted, or never took it.
                Err(error) if error.code == Some(ErrorCode::NotFound) => self.outcome_lost(id),
                Err(error) => {
                    self.outcome_lost(id);
                    self.update_line = format!("{} ({})", self.update_line, error.message);
                }
            }
            return;
        }
        let reply = match result {
            Ok(QueryReply::Memory(reply)) => reply,
            Ok(_) => return,
            Err(error) => {
                match pending {
                    Pending::List { generation, .. } => {
                        self.list_read.fail(generation, error.message);
                    }
                    Pending::Detail { generation } => {
                        self.detail_read.fail(generation, error.message);
                    }
                    Pending::Command(_) | Pending::Update(_) => {}
                }
                return;
            }
        };
        match (pending, reply) {
            (Pending::List { generation, append }, MemoryReply::List(outcome)) => {
                if let Some(page) =
                    self.list_read
                        .accept(generation, &outcome.operation, outcome.data)
                {
                    if append {
                        for item in page.items {
                            if !self
                                .items
                                .iter()
                                .any(|known| known.state.id == item.state.id)
                            {
                                self.items.push(item);
                            }
                        }
                    } else {
                        self.items = page.items;
                    }
                    self.next_cursor = page.next_cursor;
                }
            }
            (Pending::Detail { generation }, MemoryReply::Detail(outcome)) => {
                if let Some(detail) =
                    self.detail_read
                        .accept(generation, &outcome.operation, outcome.data)
                {
                    self.detail = Some(detail);
                }
            }
            _ => {}
        }
    }

    fn render(&mut self, ui: &mut egui::Ui, ctx: &mut FeatureContext<'_>) {
        for id in std::mem::take(&mut self.to_check) {
            let tag = self.tag(Pending::Update(id.clone()));
            ctx.query(
                tag,
                Query::Memory(MemoryQuery::GetMemoryUpdate(OperationTarget {
                    operation_id: id,
                })),
            );
        }
        let selected_project = ctx.selection().project.clone();
        if selected_project != self.project {
            self.switch_project(selected_project);
        }
        let Some(project) = self.project.clone() else {
            ui.label("プロジェクトを選択すると教訓を表示します。");
            return;
        };

        ui.horizontal_wrapped(|ui| {
            let before = (self.status, self.favorite_only);
            egui::ComboBox::from_id_salt("memory-status")
                .selected_text(
                    STATUSES
                        .iter()
                        .find(|(status, _)| *status == self.status)
                        .map_or("", |(_, label)| label),
                )
                .show_ui(ui, |ui| {
                    for (status, label) in STATUSES {
                        ui.selectable_value(&mut self.status, status, label);
                    }
                });
            ui.checkbox(&mut self.favorite_only, "お気に入りのみ");
            if before != (self.status, self.favorite_only) {
                self.list_due = true;
            }
            // Closed until asked for: every available feature is stacked in one column,
            // so an input drawn all the time would share the screen with every other one.
            egui::CollapsingHeader::new("検索語で絞り込む")
                .default_open(false)
                .show(ui, |ui| {
                    let response =
                        ui.add(egui::TextEdit::singleline(&mut self.words).hint_text("検索語"));
                    let submitted = response.lost_focus()
                        && ui.input(|input| input.key_pressed(egui::Key::Enter));
                    if ui.button("検索").clicked() || submitted {
                        self.list_due = true;
                    }
                });
        });

        if std::mem::take(&mut self.list_due) {
            self.begin_list(ctx, project.clone());
        } else if self.list_read.take_ask_again()
            && let Some(request) = self.list_request.clone()
        {
            self.ask_list(ctx, request, None);
        }
        if std::mem::take(&mut self.more_requested)
            && let Some(request) = self.list_request.clone()
        {
            let cursor = self.next_cursor.clone();
            self.ask_list(ctx, request, cursor);
        }
        if std::mem::take(&mut self.detail_due)
            && let Some(id) = self.selected.clone()
        {
            self.begin_detail(ctx, project.clone(), id);
        } else if self.detail_read.take_ask_again()
            && let Some(request) = self.detail_request.clone()
        {
            self.ask_detail(ctx, request);
        }

        if let Some(line) = phase_line(self.list_read.phase()) {
            ui.label(line);
        }
        if self.list_read.phase() == &Phase::Ready && self.items.is_empty() {
            ui.label("条件に合う教訓はありません。");
        }
        let mut open = None;
        let items = &self.items;
        let more = paged_list(
            ui,
            "memory-list",
            self.next_cursor.is_some().then_some("さらに読み込む"),
            |ui| {
                for item in items {
                    let star = if item.state.favorite { "★ " } else { "" };
                    let label = format!(
                        "{star}{} · {} · {}",
                        item.state.id.as_str(),
                        status_label(item.state.status),
                        item.preview
                    );
                    if ui.button(label).clicked() {
                        open = Some(item.state.id.clone());
                    }
                }
            },
        );
        if more {
            self.more_requested = true;
        }
        if let Some(id) = open {
            self.selected = Some(id);
            self.detail_due = true;
        }

        if self.selected.is_some() {
            ui.separator();
            if let Some(line) = phase_line(self.detail_read.phase()) {
                ui.label(line);
            }
            if let Some(detail) = &self.detail {
                ui.strong(format!(
                    "{} · {} · {}{}",
                    detail.state.id.as_str(),
                    status_label(detail.state.status),
                    scope_label(detail.scope),
                    if detail.state.favorite {
                        " · お気に入り"
                    } else {
                        ""
                    }
                ));
                ui.label(format!("タスク: {}", detail.task_id));
                body(ui, "内容", &detail.content);
                body(ui, "根拠", &detail.evidence);
                let busy = !self.in_flight.is_empty();
                let mut chosen = None;
                ui.horizontal_wrapped(|ui| {
                    for ActionAvailability { action, state } in &detail.state.actions {
                        match state {
                            ActionState::Allowed => {
                                if ui
                                    .add_enabled(!busy, egui::Button::new(action_label(*action)))
                                    .clicked()
                                {
                                    chosen = Some(*action);
                                }
                            }
                            ActionState::Denied { reason } => {
                                ui.add_enabled(false, egui::Button::new(action_label(*action)))
                                    .on_disabled_hover_text(denial_label(*reason));
                            }
                        }
                    }
                });
                if let Some(action) = chosen {
                    let state = detail.state.clone();
                    self.send(ctx, project.clone(), &state, action);
                }
            }
        }
        if !self.update_line.is_empty() {
            ui.label(&self.update_line);
        }
    }
}

#[cfg(test)]
mod tests;
