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

use std::time::SystemTime;

use super::diagnostics::read::{Phase, Read, cut_note, phase_label};
use super::layout::paged_list;
use super::memory::{
    CHECKING, UNKNOWN_LINE, UPDATING, denial_label, draw_update_line, update_failed, update_line,
};
use super::{Availability, Feature, FeatureContext, FeatureId, FeatureReply};
use crate::panes::improvement_widgets::{
    ALL, DISABLED, DISMISS, EMPTY, EVIDENCE, MARK_REVIEWED, NO_PROJECT, STATUS_ACCESS,
    STATUS_LABEL, evidence_runs, kind_badges, reopened_badge, seen_line, severity_word,
    source_word, status_mark, status_word, when,
};
use crate::theme::text::{badge, muted};
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
            ImprovementQuery, ImprovementReply, ImprovementResult, ImprovementSummary,
            ImprovementUpdateView, ListImprovement, TriageImprovement, UpdateImprovement,
        },
    },
    operation::OperationState,
    queries::{PageRequest, Query, QueryReply},
};

const STATUSES: [ImprovementStatus; 3] = [
    ImprovementStatus::New,
    ImprovementStatus::Reviewed,
    ImprovementStatus::Dismissed,
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
    update_failed: bool,
}

fn new_operation() -> OperationId {
    OperationId::new(format!("gui-improvement-{}", uuid::Uuid::new_v4())).expect("a uuid fits")
}

fn action_label(action: ImprovementAction) -> (&'static str, Option<&'static str>) {
    match action {
        ImprovementAction::MarkReviewed => (MARK_REVIEWED, None),
        ImprovementAction::Dismiss => (DISMISS, None),
        ImprovementAction::Triage => (
            "Create draft",
            Some("Writes an issue draft for this candidate. Nothing is sent."),
        ),
    }
}

/// The core's milliseconds as the legacy pane's relative and absolute text. The
/// wire allows any u64: a value chrono cannot hold reads as unknown, never panics.
fn when_ms(ms: u64, now: SystemTime, unknown: &str) -> (String, String) {
    match i64::try_from(ms)
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
    {
        Some(at) => when(at.into(), now),
        None => ("unknown time".into(), unknown.into()),
    }
}

fn badges(ui: &mut egui::Ui, summary: &ImprovementSummary) {
    kind_badges(
        ui,
        source_word(summary.source),
        severity_word(summary.severity),
        &summary.code,
    );
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
                line = format!("{line} Draft: {draft} (nothing is sent).");
            }
            self.update_line = line;
            self.update_failed = update_failed(view.operation.state);
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
        self.update_failed = true;
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
        UPDATING.clone_into(&mut self.update_line);
        self.update_failed = false;
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
        "Self-improvement drafts"
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
                        CHECKING.clone_into(&mut self.update_line);
                        self.update_failed = false;
                        self.to_check.insert(id);
                    }
                    Err(error) => {
                        self.in_flight.remove(&id);
                        self.update_failed = true;
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
                            format!("Could not update: {}", error.message)
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
            ui.label(NO_PROJECT);
            return;
        };

        ui.horizontal_wrapped(|ui| {
            let before = self.status;
            ui.label(STATUS_LABEL);
            let status = egui::ComboBox::from_id_salt("improvement-status")
                .selected_text(self.status.map_or(ALL, status_word))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.status, None, ALL);
                    for status in STATUSES {
                        ui.selectable_value(&mut self.status, Some(status), status_word(status));
                    }
                });
            status.response.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::ComboBox, true, STATUS_ACCESS)
            });
            // Drawn every frame, also when the filter just changed.
            let refresh = ui.button("Refresh").clicked();
            if refresh || before != self.status {
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
            // The core still lists what was collected before it was switched off.
            ui.label(muted(format!(
                "{DISABLED} Earlier candidates are still listed."
            )));
        }
        phase_label(ui, self.list_read.phase());
        if self.list_read.phase() == &Phase::Ready {
            if self.items.is_empty() {
                ui.label(EMPTY);
            } else {
                let more = if self.next_cursor.is_some() {
                    " (more available)"
                } else {
                    ""
                };
                ui.label(muted(format!("{} candidates{more}", self.items.len())));
            }
        }
        let now = SystemTime::now();
        if let Some(id) = self.candidate_list(ui, now) {
            self.selected = Some(id);
            self.detail_due = true;
        }

        if self.selected.is_some() {
            ui.separator();
            phase_label(ui, self.detail_read.phase());
            if let Some((state, action)) = self.candidate_detail(ui, now) {
                self.send(ctx, project.clone(), &state, action);
            }
        }
        draw_update_line(ui, &self.update_line, self.update_failed);
    }
}

impl ImprovementFeature {
    /// The candidates as the legacy pane lists them; returns the one whose id was clicked.
    fn candidate_list(
        &mut self,
        ui: &mut egui::Ui,
        now: SystemTime,
    ) -> Option<ImprovementCandidateId> {
        let mut open = None;
        let items = &self.items;
        let more = paged_list(
            ui,
            "improvement-list",
            self.next_cursor.is_some().then_some("Load more candidates"),
            |ui| {
                for item in items {
                    ui.push_id(item.state.id.as_str(), |ui| {
                        ui.horizontal_wrapped(|ui| {
                            status_mark(ui, item.state.status);
                            let id = egui::RichText::new(item.state.id.as_str()).monospace();
                            if ui.add(egui::Button::new(id).frame(false)).clicked() {
                                open = Some(item.state.id.clone());
                            }
                            badges(ui, item);
                        });
                        ui.label(&item.title);
                        if item.occurrences > 1 {
                            let (seen, absolute) =
                                when_ms(item.last_seen_ms, now, "Unknown last-seen time");
                            ui.label(muted(seen_line(item.occurrences, &seen)))
                                .on_hover_text(absolute);
                        }
                        ui.separator();
                    });
                }
            },
        );
        if more {
            self.more_requested = true;
        }
        open
    }

    /// The opened candidate as the legacy pane draws an entry; returns the update chosen.
    fn candidate_detail(
        &self,
        ui: &mut egui::Ui,
        now: SystemTime,
    ) -> Option<(ImprovementState, ImprovementAction)> {
        let detail = self.detail.as_ref()?;
        let summary = &detail.summary;
        ui.horizontal_wrapped(|ui| {
            status_mark(ui, summary.state.status);
            if detail.reopened_count > 0 {
                reopened_badge(ui, detail.reopened_count);
            }
            ui.monospace(summary.state.id.as_str());
            let (created, absolute) = when_ms(detail.created_ms, now, "Unknown creation time");
            ui.label(muted(created)).on_hover_text(absolute);
        });
        ui.horizontal_wrapped(|ui| badges(ui, summary));
        ui.label(&summary.title);
        ui.collapsing(badge(EVIDENCE), |ui| {
            ui.monospace(&detail.evidence.text);
            if detail.evidence.truncated {
                ui.label(muted(cut_note(
                    detail.evidence.text.len(),
                    detail.evidence.total_bytes,
                )));
            }
            evidence_runs(
                ui,
                detail.run.as_ref().map(|run| run.as_str()),
                &detail
                    .recent_runs
                    .iter()
                    .map(|run| run.as_str())
                    .collect::<Vec<_>>(),
            );
        });
        if let Some(draft) = &detail.draft_file {
            ui.label(format!("Draft: {draft}"));
        }
        let busy = !self.in_flight.is_empty();
        let mut chosen = None;
        ui.horizontal_wrapped(|ui| {
            for ActionAvailability { action, state } in &summary.state.actions {
                let (label, tip) = action_label(*action);
                match state {
                    ActionState::Allowed => {
                        let response = ui.add_enabled(!busy, egui::Button::new(label));
                        let response = match tip {
                            Some(tip) => response.on_hover_text(tip),
                            None => response,
                        };
                        if response.clicked() {
                            chosen = Some(*action);
                        }
                    }
                    ActionState::Denied { reason } => {
                        ui.add_enabled(false, egui::Button::new(label))
                            .on_disabled_hover_text(denial_label(*reason));
                    }
                }
            }
        });
        chosen.map(|action| (summary.state.clone(), action))
    }
}

#[cfg(test)]
mod tests;
