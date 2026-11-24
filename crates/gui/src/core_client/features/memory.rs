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

use super::diagnostics::read::{Phase, Read, cut_note, phase_label};
use super::layout::paged_list;
use super::{Availability, Feature, FeatureContext, FeatureId, FeatureReply};
use crate::panes::memory_widgets::{
    ALL_STATUSES, APPROVAL_FILTERS, EMPTY, EVIDENCE, NO_PROJECT, REFRESH, approval_action,
    approval_badge, approval_filter_label, reject_controls, scope_word, status_word,
};
use crate::theme::text::{badge, muted};
use crate::theme::tokens::palette;
use crate::theme::widgets::{compact_row, reason_label};
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
            MemoryReply, MemoryResult, MemorySummary, MemoryUpdateView, UpdateMemory,
        },
    },
    operation::OperationState,
    queries::{PageRequest, Query, QueryReply},
};

const STATUSES: [MemoryStatus; 4] = [
    MemoryStatus::Candidate,
    MemoryStatus::Validated,
    MemoryStatus::Promoted,
    MemoryStatus::Rejected,
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
    /// `Some(true)` asks for approved lessons only, `Some(false)` for unconfirmed ones.
    approval: Option<bool>,
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
    update_failed: bool,
    /// The lesson whose Reject waits for its confirmation.
    confirm_reject: Option<String>,
}

fn new_operation() -> OperationId {
    OperationId::new(format!("gui-memory-{}", uuid::Uuid::new_v4())).expect("a uuid fits")
}

fn action_label(action: MemoryAction) -> (&'static str, Option<&'static str>) {
    match action {
        MemoryAction::SetFavorite => {
            let (label, tip) = approval_action(false);
            (label, Some(tip))
        }
        MemoryAction::ClearFavorite => {
            let (label, tip) = approval_action(true);
            (label, Some(tip))
        }
        MemoryAction::Validate => ("Validate", None),
        MemoryAction::Promote => ("Promote", None),
        MemoryAction::Reject => ("Reject", None),
    }
}

/// The legacy pane's order: the approval first, rejecting last.
fn action_rank(action: MemoryAction) -> u8 {
    match action {
        MemoryAction::SetFavorite | MemoryAction::ClearFavorite => 0,
        MemoryAction::Validate => 1,
        MemoryAction::Promote => 2,
        MemoryAction::Reject => 3,
    }
}

/// Why an action is not offered, in words. Shared with the improvement area.
pub(super) fn denial_label(reason: DenialReason) -> &'static str {
    match reason {
        DenialReason::NotApplicableInState => "Not available in the current state.",
        DenialReason::FavoriteMustBeCleared => {
            "An approved lesson cannot be rejected. Clear the approval first."
        }
        DenialReason::AlreadyResolved => "Already decided.",
        DenialReason::OperationInProgress => "Another update is in progress.",
        DenialReason::ReadOnly => "Read-only.",
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
        OperationState::Pending => UPDATING.to_owned(),
        OperationState::Ready if changed == Some(false) => {
            "Already in that state (nothing changed).".to_owned()
        }
        OperationState::Ready => "Updated.".to_owned(),
        OperationState::Cancelled { .. } => {
            "The update was cancelled. Refresh to check the current state.".to_owned()
        }
        OperationState::Failed { code } => match (code, denial) {
            (_, Some(reason)) => denial_label(reason).to_owned(),
            (ErrorCode::StaleRevision, _) => {
                "It was changed elsewhere and has been read again. Check it and try again."
                    .to_owned()
            }
            (ErrorCode::NotFound, _) => "It no longer exists.".to_owned(),
            _ => match message {
                Some(message) => format!("Could not update: {message}"),
                None => "Could not update.".to_owned(),
            },
        },
    }
}

/// Whether an update that ended in `state` is shown as a failure.
pub(super) fn update_failed(state: OperationState) -> bool {
    matches!(
        state,
        OperationState::Failed { .. } | OperationState::Cancelled { .. }
    )
}

pub(super) const UPDATING: &str = "Updating…";

pub(super) const CHECKING: &str = "Checking the outcome of the update…";

/// The line for an update whose outcome cannot be known: it is not "done".
pub(super) const UNKNOWN_LINE: &str = "The outcome of the update is unknown (a reconnect or a core restart). It is not counted as done; refresh to check the state.";

/// The update line: muted, or in the error colour when the update failed.
pub(super) fn draw_update_line(ui: &mut egui::Ui, line: &str, failed: bool) {
    if line.is_empty() {
        return;
    }
    if failed {
        reason_label(ui, palette().ERROR_FG, line);
    } else {
        ui.label(muted(line));
    }
}

fn status_selected(status: Option<MemoryStatus>) -> &'static str {
    status.map_or(ALL_STATUSES, status_word)
}

fn bounded_text(ui: &mut egui::Ui, text: &BoundedText) {
    ui.label(&text.text);
    if text.truncated {
        ui.label(muted(cut_note(text.text.len(), text.total_bytes)));
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
        UPDATING.clone_into(&mut self.update_line);
        self.update_failed = false;
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
        self.confirm_reject = None;
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
            favorite: self.approval,
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
            ui.label(NO_PROJECT);
            return;
        };

        self.toolbar(ui);

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
                ui.label(muted(format!("{} lessons{more}", self.items.len())));
            }
        }
        if let Some(id) = self.lesson_list(ui) {
            self.selected = Some(id);
            self.confirm_reject = None;
            self.detail_due = true;
        }

        if self.selected.is_some() {
            ui.separator();
            phase_label(ui, self.detail_read.phase());
            if let Some((state, action)) = self.lesson_detail(ui) {
                self.confirm_reject = None;
                self.send(ctx, project.clone(), &state, action);
            }
        }
        draw_update_line(ui, &self.update_line, self.update_failed);
    }
}

impl MemoryFeature {
    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            let before = (self.status, self.approval);
            let status = egui::ComboBox::from_id_salt("memory-status")
                .selected_text(status_selected(self.status))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.status, None, ALL_STATUSES);
                    for status in STATUSES {
                        ui.selectable_value(&mut self.status, Some(status), status_word(status));
                    }
                });
            status.response.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::ComboBox, true, "Lesson status")
            });
            let approval = egui::ComboBox::from_id_salt("memory-approval")
                .selected_text(approval_filter_label(self.approval))
                .show_ui(ui, |ui| {
                    for (value, label) in APPROVAL_FILTERS {
                        ui.selectable_value(&mut self.approval, value, label);
                    }
                });
            approval.response.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::ComboBox, true, "Approval")
            });
            // Drawn every frame, also when a filter just changed.
            let refresh = ui.button(REFRESH).clicked();
            if refresh || before != (self.status, self.approval) {
                self.list_due = true;
            }
        });
        // Closed until asked for: every available feature is stacked in one column,
        // so an input drawn all the time would share the screen with every other one.
        egui::CollapsingHeader::new("Filter by words")
            .default_open(false)
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    let response =
                        ui.add(egui::TextEdit::singleline(&mut self.words).hint_text("Words"));
                    let submitted = response.lost_focus()
                        && ui.input(|input| input.key_pressed(egui::Key::Enter));
                    if ui.button("Search").clicked() || submitted {
                        self.list_due = true;
                    }
                });
            });
    }

    /// The lessons, one row each; returns the lesson whose id was clicked.
    fn lesson_list(&mut self, ui: &mut egui::Ui) -> Option<MemoryId> {
        let mut open = None;
        let items = &self.items;
        let selected = self.selected.as_ref();
        let more = paged_list(
            ui,
            "memory-list",
            self.next_cursor.is_some().then_some("Load more lessons"),
            |ui| {
                for item in items {
                    ui.push_id(item.state.id.as_str(), |ui| {
                        compact_row(ui, selected == Some(&item.state.id), |ui| {
                            let id = egui::RichText::new(item.state.id.as_str()).monospace();
                            if ui.add(egui::Button::new(id).frame(false)).clicked() {
                                open = Some(item.state.id.clone());
                            }
                            ui.label(status_word(item.state.status));
                            ui.label(scope_word(item.scope));
                            approval_badge(ui, item.state.favorite);
                            ui.add(egui::Label::new(muted(&item.preview)).truncate());
                        });
                    });
                }
            },
        );
        if more {
            self.more_requested = true;
        }
        open
    }

    /// The opened lesson as the legacy pane draws an entry; returns the update chosen.
    fn lesson_detail(&mut self, ui: &mut egui::Ui) -> Option<(MemoryState, MemoryAction)> {
        let detail = self.detail.as_ref()?;
        ui.horizontal_wrapped(|ui| {
            ui.monospace(detail.state.id.as_str());
            ui.label(status_word(detail.state.status));
            ui.label(scope_word(detail.scope));
            approval_badge(ui, detail.state.favorite);
        });
        bounded_text(ui, &detail.content);
        ui.collapsing(badge(EVIDENCE), |ui| {
            bounded_text(ui, &detail.evidence);
            ui.monospace(&detail.task_id);
        });
        let busy = !self.in_flight.is_empty();
        let mut actions = detail.state.actions.clone();
        actions.sort_by_key(|available| action_rank(available.action));
        let mut chosen = None;
        let mut rejected = None;
        ui.horizontal_wrapped(|ui| {
            for ActionAvailability { action, state } in &actions {
                let (label, tip) = action_label(*action);
                match state {
                    ActionState::Allowed if *action == MemoryAction::Reject && !busy => {
                        reject_controls(
                            ui,
                            detail.state.id.as_str(),
                            &mut self.confirm_reject,
                            &mut rejected,
                        );
                    }
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
        if rejected.is_some() {
            chosen = Some(MemoryAction::Reject);
        }
        chosen.map(|action| (detail.state.clone(), action))
    }
}

#[cfg(test)]
mod tests;
