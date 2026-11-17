//! Desktop feature of the `improvement` area: the self-improvement candidates of
//! the selected project, filtered by status, one candidate opened for its
//! evidence (PR 13), and its updates (PR 14): reviewed, dismiss, triage.
//!
//! Shows whether the feature is switched on in the settings, and names a draft by
//! its file name only: the directory it lives in stays in the core. A triage
//! only makes a draft and names it; nothing is sent. An update is sent with the
//! revision the shown candidate had; a refusal is worded and the candidate is
//! read again, never resent, and an update of unknown outcome is asked for by
//! its operation id (see the memory feature, which words them the same way).
use std::collections::{BTreeMap, BTreeSet};

use super::diagnostics::read::{Phase, Read, phase_line};
use super::layout::paged_list;
use super::memory::{UNKNOWN_LINE, denial_label, update_line};
use super::{Availability, Feature, FeatureContext, FeatureId, FeatureReply};
use workbench_protocol::{
    commands::{Command, CommandResult},
    errors::ErrorCode,
    events::UiEvent,
    handshake::Capability,
    ids::{ImprovementCandidateId, OperationId, PageCursor, ProjectId},
    management::{
        ActionAvailability, ActionState, ImprovementAction, ImprovementState, ImprovementStatus,
        TriageSubject,
    },
    namespaces::{
        diagnostics::OperationTarget,
        improvement::{
            GetImprovement, ImprovementCommand, ImprovementDetail, ImprovementEvent,
            ImprovementQuery, ImprovementReply, ImprovementResult, ImprovementSeverity,
            ImprovementSource, ImprovementSummary, ImprovementUpdateView, ListImprovement,
            TriageImprovement, UpdateImprovement,
        },
    },
    operation::OperationState,
    queries::{PageRequest, Query, QueryReply},
};

const STATUSES: [(Option<ImprovementStatus>, &str); 4] = [
    (None, "すべて"),
    (Some(ImprovementStatus::New), "未確認"),
    (Some(ImprovementStatus::Reviewed), "確認済み"),
    (Some(ImprovementStatus::Dismissed), "却下"),
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
pub struct ImprovementFeature {
    next_tag: u64,
    pending: BTreeMap<u64, Pending>,
    project: Option<ProjectId>,
    status: Option<ImprovementStatus>,
    list_read: Read,
    list_request: Option<ListImprovement>,
    enabled: Option<bool>,
    items: Vec<ImprovementSummary>,
    next_cursor: Option<PageCursor>,
    list_due: bool,
    more_requested: bool,
    selected: Option<ImprovementCandidateId>,
    detail_read: Read,
    detail_request: Option<GetImprovement>,
    detail: Option<ImprovementDetail>,
    detail_due: bool,
    /// The updates sent whose end is not known yet.
    in_flight: BTreeSet<OperationId>,
    /// The updates whose outcome is unknown, asked for at the next frame.
    to_check: BTreeSet<OperationId>,
    update_line: String,
}

fn new_operation() -> OperationId {
    OperationId::new(format!("gui-improvement-{}", uuid::Uuid::new_v4())).expect("a uuid fits")
}

fn action_label(action: ImprovementAction) -> &'static str {
    match action {
        ImprovementAction::MarkReviewed => "確認済みにする",
        ImprovementAction::Dismiss => "却下する",
        ImprovementAction::Triage => "下書きを作る",
    }
}

fn status_label(status: ImprovementStatus) -> &'static str {
    STATUSES
        .iter()
        .find(|(candidate, _)| *candidate == Some(status))
        .map_or("", |(_, label)| label)
}

fn severity_label(severity: ImprovementSeverity) -> &'static str {
    match severity {
        ImprovementSeverity::Info => "情報",
        ImprovementSeverity::Warning => "警告",
        ImprovementSeverity::Error => "エラー",
    }
}

fn source_label(source: ImprovementSource) -> &'static str {
    match source {
        ImprovementSource::Diagnostic => "診断",
        ImprovementSource::Lesson => "教訓",
    }
}

impl ImprovementFeature {
    fn tag(&mut self, pending: Pending) -> u64 {
        self.next_tag += 1;
        self.pending.insert(self.next_tag, pending);
        self.next_tag
    }

    /// An update ended or changed. Ours is worded (a triage names its draft by
    /// file); anyone's makes the shown project's candidates stale.
    fn update_arrived(&mut self, view: &ImprovementUpdateView) {
        if !view.operation.state.is_final() {
            return;
        }
        let id = &view.operation.operation_id;
        let ours = self.in_flight.remove(id);
        self.to_check.remove(id);
        if ours {
            let mut line = update_line(
                view.operation.state,
                view.changed,
                view.denial,
                view.message.as_deref(),
            );
            if view.operation.state == OperationState::Ready
                && let Some(draft) = &view.draft_file
            {
                line = format!("{line} 下書き: {draft}（送信はされません）");
            }
            self.update_line = line;
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
        state: &ImprovementState,
        action: ImprovementAction,
    ) {
        let operation_id = new_operation();
        let update = || UpdateImprovement {
            operation_id: operation_id.clone(),
            project: project.clone(),
            candidate: state.id.clone(),
            // What the read said, as it said it.
            expected_revision: state.revision,
        };
        let command = match action {
            ImprovementAction::MarkReviewed => {
                ImprovementCommand::MarkImprovementReviewed(update())
            }
            ImprovementAction::Dismiss => ImprovementCommand::DismissImprovement(update()),
            ImprovementAction::Triage => ImprovementCommand::TriageImprovement(TriageImprovement {
                operation_id: operation_id.clone(),
                project: project.clone(),
                subject: TriageSubject {
                    candidate: state.id.clone(),
                    expected_revision: state.revision,
                },
            }),
        };
        self.in_flight.insert(operation_id.clone());
        "更新中…".clone_into(&mut self.update_line);
        let tag = self.tag(Pending::Command(operation_id));
        ctx.command(tag, Command::Improvement(command));
    }

    fn switch_project(&mut self, project: Option<ProjectId>) {
        self.project = project;
        self.list_read.forget();
        self.detail_read.forget();
        self.list_request = None;
        self.detail_request = None;
        self.enabled = None;
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
        let request = ListImprovement {
            operation_id: self.list_read.begin("improvement-list"),
            project,
            status: self.status,
            page: PageRequest::default(),
        };
        self.ask_list(ctx, request, None);
    }

    fn ask_list(
        &mut self,
        ctx: &mut FeatureContext<'_>,
        mut request: ListImprovement,
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
        ctx.query(
            tag,
            Query::Improvement(ImprovementQuery::ListImprovement(request.clone())),
        );
        request.page = PageRequest::default();
        self.list_request = Some(request);
    }

    fn begin_detail(
        &mut self,
        ctx: &mut FeatureContext<'_>,
        project: ProjectId,
        candidate: ImprovementCandidateId,
    ) {
        self.detail = None;
        let request = GetImprovement {
            operation_id: self.detail_read.begin("improvement-detail"),
            project,
            candidate,
        };
        self.ask_detail(ctx, request);
    }

    fn ask_detail(&mut self, ctx: &mut FeatureContext<'_>, request: GetImprovement) {
        let tag = self.tag(Pending::Detail {
            generation: self.detail_read.generation(),
        });
        ctx.query(
            tag,
            Query::Improvement(ImprovementQuery::GetImprovement(request.clone())),
        );
        self.detail_request = Some(request);
    }
}

impl Feature for ImprovementFeature {
    fn id(&self) -> FeatureId {
        FeatureId::Improvement
    }

    fn title(&self) -> &'static str {
        "Improvement"
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
        self.enabled = None;
        self.items.clear();
        self.next_cursor = None;
        self.detail = None;
        self.list_due = self.project.is_some();
        self.detail_due = self.selected.is_some();
    }

    fn event(&mut self, event: &UiEvent) {
        match event {
            UiEvent::Improvement(ImprovementEvent::ImprovementOperationChanged { view }) => {
                self.list_read.event(view);
                self.detail_read.event(view);
            }
            UiEvent::Improvement(ImprovementEvent::ImprovementUpdateCompleted { update }) => {
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
                        if let Some(CommandResult::Improvement(ImprovementResult::Operation(
                            view,
                        ))) = reply.result.map(|boxed| *boxed)
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
                Ok(QueryReply::Improvement(ImprovementReply::Update(view))) => {
                    self.update_arrived(&view);
                }
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
            Ok(QueryReply::Improvement(reply)) => reply,
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
            (Pending::List { generation, append }, ImprovementReply::List(outcome)) => {
                if let Some(list) =
                    self.list_read
                        .accept(generation, &outcome.operation, outcome.data)
                {
                    self.enabled = Some(list.enabled);
                    if append {
                        for item in list.candidates.items {
                            if !self
                                .items
                                .iter()
                                .any(|known| known.state.id == item.state.id)
                            {
                                self.items.push(item);
                            }
                        }
                    } else {
                        self.items = list.candidates.items;
                    }
                    self.next_cursor = list.candidates.next_cursor;
                }
            }
            (Pending::Detail { generation }, ImprovementReply::Detail(outcome)) => {
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
                Query::Improvement(ImprovementQuery::GetImprovementUpdate(OperationTarget {
                    operation_id: id,
                })),
            );
        }
        let selected_project = ctx.selection().project.clone();
        if selected_project != self.project {
            self.switch_project(selected_project);
        }
        let Some(project) = self.project.clone() else {
            ui.label("プロジェクトを選択すると改善候補を表示します。");
            return;
        };

        ui.horizontal(|ui| {
            let before = self.status;
            egui::ComboBox::from_id_salt("improvement-status")
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
            if before != self.status || ui.button("再読み込み").clicked() {
                self.list_due = true;
            }
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

        if self.enabled == Some(false) {
            ui.label("自己改善は設定で無効です。記録済みの候補だけを表示します。");
        }
        if let Some(line) = phase_line(self.list_read.phase()) {
            ui.label(line);
        }
        if self.list_read.phase() == &Phase::Ready && self.items.is_empty() {
            ui.label("改善候補はありません。");
        }
        let mut open = None;
        let items = &self.items;
        let more = paged_list(
            ui,
            "improvement-list",
            self.next_cursor.is_some().then_some("さらに読み込む"),
            |ui| {
                for item in items {
                    let label = format!(
                        "{} · {} · {} · {} 回 · {}",
                        item.state.id.as_str(),
                        status_label(item.state.status),
                        severity_label(item.severity),
                        item.occurrences,
                        item.title
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
                let summary = &detail.summary;
                ui.strong(format!(
                    "{} · {} · {} · {}",
                    summary.state.id.as_str(),
                    source_label(summary.source),
                    summary.code,
                    summary.title
                ));
                if detail.reopened_count > 0 {
                    ui.label(format!("再オープン {} 回", detail.reopened_count));
                }
                if let Some(draft) = &detail.draft_file {
                    ui.label(format!("下書き: {draft}"));
                }
                if let Some(run) = &detail.run {
                    ui.label(format!("最初の run: {}", run.as_str()));
                }
                ui.strong("根拠");
                ui.label(&detail.evidence.text);
                if detail.evidence.truncated {
                    ui.label(format!(
                        "(先頭 {} バイトのみ表示 / 全体 {} バイト)",
                        detail.evidence.text.len(),
                        detail.evidence.total_bytes
                    ));
                }
                let busy = !self.in_flight.is_empty();
                let mut chosen = None;
                ui.horizontal_wrapped(|ui| {
                    for ActionAvailability { action, state } in &summary.state.actions {
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
                    let state = summary.state.clone();
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
