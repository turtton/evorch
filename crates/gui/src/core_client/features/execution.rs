//! Desktop feature of the `execution` area: the runs of the selected thread
//! (roots, then the children of one run) as cards of the Subagents column, and
//! the working note of its root.
//!
//! Everything shown is read from the core with a typed query; an event only
//! says that something changed, and the answer is asked for again, so an event
//! that arrives late never moves the screen back to an older state. A run that
//! left the runtime stays listed, with what became of it. Stop goes out for one
//! run of the shown thread only; the core refuses a run it no longer holds.
//!
//! The goal and the plan of the selected thread are read in `maintain`, while
//! the tab is hidden too, because the conversation's composer band shows them
//! ([`ExecutionFeature::band`]); this tab does not draw the goal. What is
//! pressed in the band or the Plan tab is queued here and leaves in the next
//! `maintain`.
use std::collections::{BTreeMap, HashSet};

mod card;

use super::layout::main_paged_list;
use super::{
    Availability, Feature, FeatureContext, FeatureError, FeatureId, FeatureReply, Visibility,
};
use crate::core_client::run_tabs::RunPane;
use crate::model::conversation_plan::{PlanState, ReadFailure, ReadKind};
use crate::panes::conversation_plan::{PlanBand, PlanPanelAction};
use crate::panes::goal_widgets::GoalAction;
use crate::panes::run_card_widgets::{NoteRevision, working_note};
use crate::theme::{text::muted, widgets::empty_state};
use workbench_protocol::{
    commands::{Command, SetGoalChecksPaused, SetGoalReview, StopRun, ThreadTarget},
    errors::ErrorCode,
    events::UiEvent,
    handshake::Capability,
    ids::{PageCursor, RunId, ThreadId},
    namespaces::{
        ExecutionCommand, ExecutionEvent, ExecutionQuery, ExecutionReply,
        execution::{GoalView, RunDetail, RunDetailsQuery, RunScope, ThreadExecution},
    },
    queries::{PageRequest, Query, QueryReply},
};

/// What a request in flight was for.
#[derive(Debug, Clone)]
enum Pending {
    Runs { thread: ThreadId, path: Vec<RunId> },
    Execution { thread: ThreadId },
    Command(&'static str),
    Plan { kind: ReadKind, generation: u64 },
}

/// A cursor chain is read serially and published as one prefix. Keeping the old
/// prefix visible avoids losing a row (or its pointer target) between pages.
#[derive(Default)]
struct RunsRead {
    tag: Option<u64>,
    items: Vec<RunDetail>,
    ids: HashSet<RunId>,
    pages: usize,
    target_pages: usize,
    next_cursor: Option<PageCursor>,
}

#[derive(Default)]
pub struct ExecutionFeature {
    next_tag: u64,
    /// Ties a tag to its meaning. Kept across `reset`.
    pending: BTreeMap<u64, Pending>,
    /// The thread the data below belongs to.
    thread: Option<ThreadId>,
    /// The runs drilled into, outermost first; empty shows the roots.
    path: Vec<RunId>,
    runs: Vec<RunDetail>,
    next_cursor: Option<PageCursor>,
    loaded_pages: usize,
    runs_read: Option<RunsRead>,
    execution: Option<ThreadExecution>,
    status: String,
    runs_stale: bool,
    execution_stale: bool,
    more_requested: bool,
    plan: PlanState,
    /// The Plan tab was drawn since the last `maintain`.
    plan_drawn: bool,
    /// The Plan tab was drawn before the last `maintain`: its reads are due.
    plan_shown: bool,
    goal_actions: Vec<GoalAction>,
    plan_actions: Vec<PlanPanelAction>,
    /// The row whose Open pane was pressed last, until the shell opens its tab.
    open_pane: Option<RunPane>,
}

#[derive(Debug, Clone, Copy)]
pub struct ExecutionBand<'a> {
    pub thread: &'a ThreadId,
    pub goal: Option<&'a GoalView>,
    pub plan: PlanBand<'a>,
}

impl ExecutionFeature {
    pub fn band(&self) -> Option<ExecutionBand<'_>> {
        let thread = self.thread.as_ref()?;
        Some(ExecutionBand {
            thread,
            goal: self
                .execution
                .as_ref()
                .filter(|execution| &execution.thread == thread)
                .and_then(|execution| execution.goal.as_ref()),
            plan: PlanBand {
                summary: self
                    .plan
                    .summary()
                    .filter(|summary| &summary.thread == thread),
            },
        })
    }

    pub fn plan_state(&self) -> &PlanState {
        &self.plan
    }

    /// The Plan tab was drawn this frame. Returns whether it was hidden
    /// before, so its reads wait for one more frame.
    pub fn plan_drawn(&mut self) -> bool {
        self.plan_drawn = true;
        !self.plan_shown
    }

    pub fn take_open_pane(&mut self) -> Option<RunPane> {
        self.open_pane.take()
    }

    pub fn enqueue_goal(&mut self, action: GoalAction) {
        self.goal_actions.push(action);
    }

    pub fn enqueue_plan(&mut self, action: PlanPanelAction) {
        self.plan_actions.push(action);
    }

    fn send_goal_actions(&mut self, ctx: &mut FeatureContext<'_>, thread: &ThreadId) {
        for action in std::mem::take(&mut self.goal_actions) {
            let Some(goal) = self.execution.as_ref().and_then(|e| e.goal.as_ref()) else {
                continue;
            };
            let (label, command) = match action {
                GoalAction::Review {
                    thread_id,
                    goal_id,
                    enabled,
                } if thread_id == thread.as_str() && goal_id == goal.goal.as_str() => {
                    ("Goal review", goal_review(thread, goal, enabled))
                }
                GoalAction::ChecksPaused {
                    thread_id,
                    goal_id,
                    paused,
                } if thread_id == thread.as_str() && goal_id == goal.goal.as_str() => {
                    ("Goal checks", goal_checks_paused(thread, goal, paused))
                }
                _ => continue,
            };
            self.command(ctx, label, command);
        }
    }

    fn tag(&mut self, pending: Pending) -> u64 {
        self.next_tag += 1;
        self.pending.insert(self.next_tag, pending);
        self.next_tag
    }

    fn switch_thread(&mut self, thread: Option<ThreadId>) {
        self.thread = thread;
        self.path.clear();
        self.clear_runs();
        self.execution = None;
        self.execution_stale = self.thread.is_some();
        self.plan.set_thread(self.thread.clone());
        self.plan_actions.clear();
        self.goal_actions.clear();
        self.open_pane = None;
    }

    /// Show the children of `run`: a new level below the current one.
    fn drill_into(&mut self, run: RunId) {
        self.path.push(run);
        self.clear_runs();
    }

    fn clear_runs(&mut self) {
        self.runs.clear();
        self.next_cursor = None;
        self.loaded_pages = 0;
        self.runs_read = None;
        self.more_requested = false;
        self.runs_stale = self.thread.is_some();
    }

    fn scope(&self) -> RunScope {
        match self.path.last() {
            Some(parent) => RunScope::Children {
                parent: parent.clone(),
            },
            None => RunScope::Roots,
        }
    }

    fn request_runs(&mut self, ctx: &mut FeatureContext<'_>) {
        let Some(thread) = self.thread.clone() else {
            return;
        };
        if self.runs_read.is_none() {
            let more = self.more_requested && self.next_cursor.is_some();
            self.more_requested = false;
            if self.runs_stale {
                self.runs_stale = false;
                self.runs_read = Some(RunsRead {
                    target_pages: self.loaded_pages.max(1) + usize::from(more),
                    ..Default::default()
                });
            } else if more {
                self.runs_read = Some(RunsRead {
                    items: self.runs.clone(),
                    ids: self.runs.iter().map(|run| run.summary.id.clone()).collect(),
                    pages: self.loaded_pages,
                    target_pages: self.loaded_pages + 1,
                    next_cursor: self.next_cursor.clone(),
                    ..Default::default()
                });
            }
        }
        let Some(read) = &self.runs_read else {
            return;
        };
        if read.tag.is_some() {
            return;
        }
        let cursor = read.next_cursor.clone();
        let tag = self.tag(Pending::Runs {
            thread: thread.clone(),
            path: self.path.clone(),
        });
        self.runs_read.as_mut().unwrap().tag = Some(tag);
        ctx.query(
            tag,
            Query::Execution(ExecutionQuery::ListRunDetails(RunDetailsQuery {
                thread,
                scope: self.scope(),
                page: PageRequest {
                    cursor,
                    ..PageRequest::default()
                },
            })),
        );
    }

    fn command(&mut self, ctx: &mut FeatureContext<'_>, label: &'static str, command: Command) {
        let tag = self.tag(Pending::Command(label));
        ctx.command(tag, command);
    }

    fn draw_runs(&mut self, ui: &mut egui::Ui, ctx: &mut FeatureContext<'_>, thread: &ThreadId) {
        ui.horizontal_wrapped(|ui| {
            if ui.button("Root").clicked() && !self.path.is_empty() {
                self.path.clear();
                self.clear_runs();
            }
            for index in 0..self.path.len() {
                let id = self.path[index].clone();
                ui.label("›");
                if ui.button(id.as_str()).clicked() && index + 1 < self.path.len() {
                    self.path.truncate(index + 1);
                    self.clear_runs();
                    break;
                }
            }
        });
        if self.runs.is_empty() && !self.runs_stale && self.runs_read.is_none() {
            if self.path.is_empty() {
                no_runs(ui);
            } else {
                ui.label(muted("No subagents of this run"));
            }
        }
        let mut drill = None;
        let mut stop = None;
        let mut open = None;
        let runs = &self.runs;
        let more = main_paged_list(
            ui,
            "execution-runs",
            self.next_cursor.is_some().then_some("Load more runs"),
            |ui| {
                for run in runs {
                    match card::run_card(ui, run) {
                        Some(card::CardAction::Drill) => drill = Some(run.summary.id.clone()),
                        Some(card::CardAction::Stop) => stop = Some(run.summary.id.clone()),
                        Some(card::CardAction::Open) => {
                            open = Some(RunPane {
                                thread: thread.clone(),
                                run: run.summary.id.clone(),
                                name: run.name.clone(),
                                child: run.summary.parent_run.is_some(),
                            });
                        }
                        None => {}
                    }
                }
            },
        );
        if let Some(run) = drill {
            self.drill_into(run);
        }
        if open.is_some() {
            self.open_pane = open;
        }
        if let Some(run) = stop {
            self.command(ctx, "Stop run", stop_run(thread, run));
        }
        if more
            && !self
                .runs_read
                .as_ref()
                .is_some_and(|read| read.target_pages > self.loaded_pages)
        {
            self.more_requested = true;
        }
    }

    fn draw_working_note(&self, ui: &mut egui::Ui) {
        let Some(ledger) = self.execution.as_ref().and_then(|e| e.ledger.as_ref()) else {
            return;
        };
        working_note(
            ui,
            ledger.run.as_str(),
            &[NoteRevision {
                seq: ledger.seq.get(),
                body: &ledger.body,
            }],
            ledger.truncated,
        );
    }
}

fn no_runs(ui: &mut egui::Ui) {
    empty_state(
        ui,
        "No runs in this thread",
        "Start a conversation to see its agent runs.",
        None,
    );
}

fn stop_run(thread: &ThreadId, run: RunId) -> Command {
    Command::Execution(ExecutionCommand::StopRun(StopRun {
        thread: thread.clone(),
        run,
    }))
}

fn goal_review(thread: &ThreadId, goal: &GoalView, enabled: bool) -> Command {
    Command::Execution(ExecutionCommand::SetGoalReview(SetGoalReview {
        thread: thread.clone(),
        goal: goal.goal.clone(),
        enabled,
    }))
}

fn goal_checks_paused(thread: &ThreadId, goal: &GoalView, paused: bool) -> Command {
    Command::Execution(ExecutionCommand::SetGoalChecksPaused(SetGoalChecksPaused {
        thread: thread.clone(),
        goal: goal.goal.clone(),
        paused,
    }))
}

impl Feature for ExecutionFeature {
    fn id(&self) -> FeatureId {
        FeatureId::Execution
    }

    fn title(&self) -> &'static str {
        "Execution"
    }

    fn availability(&self) -> Availability {
        Availability::Implemented {
            required: &[Capability::Queries, Capability::Commands],
        }
    }

    fn reset(&mut self) {
        // What came from the core is dropped and asked for again; `pending`
        // stays, because a tag may still be answered.
        let loaded_pages = self.loaded_pages;
        self.clear_runs();
        self.loaded_pages = loaded_pages;
        self.execution = None;
        self.execution_stale = self.thread.is_some();
        self.plan.restart_reads();
        self.open_pane = None;
    }

    fn event(&mut self, event: &UiEvent) {
        let UiEvent::Execution(event) = event else {
            return;
        };
        let changed = match event {
            ExecutionEvent::RunChanged { run } => Some(&run.thread),
            ExecutionEvent::ThreadExecutionChanged { thread, .. } => Some(thread),
            ExecutionEvent::ConversationPlanChanged { thread, .. } => Some(thread),
        };
        if changed == self.thread.as_ref() {
            // Ask again instead of patching: the core's answer is current.
            match event {
                ExecutionEvent::RunChanged { run } => {
                    self.runs_stale = true;
                    // The ledger and goal shown belong to the thread's current root:
                    // a root that appears or ends may have changed which one that is.
                    if run.parent_run.is_none() {
                        self.execution_stale = true;
                    }
                }
                ExecutionEvent::ThreadExecutionChanged { .. } => self.execution_stale = true,
                ExecutionEvent::ConversationPlanChanged { thread, revision } => {
                    self.plan.invalidate(thread, *revision);
                }
            }
        }
    }

    fn reply(&mut self, tag: u64, reply: FeatureReply) {
        let Some(pending) = self.pending.remove(&tag) else {
            return;
        };
        if let Pending::Runs { thread, path } = &pending {
            // A navigation or reset invalidates the read, even when the user has
            // since returned to the same thread and breadcrumb.
            if self.thread.as_ref() != Some(thread)
                || self.path != *path
                || self.runs_read.as_ref().and_then(|read| read.tag) != Some(tag)
            {
                return;
            }
        }
        match (pending, reply) {
            (
                Pending::Runs { .. },
                FeatureReply::Query(Ok(QueryReply::Execution(ExecutionReply::RunDetails(page)))),
            ) => {
                let read = self.runs_read.as_mut().unwrap();
                read.tag = None;
                for run in page.items {
                    if read.ids.insert(run.summary.id.clone()) {
                        read.items.push(run);
                    }
                }
                read.pages += 1;
                read.next_cursor = page.next_cursor;
                if read.pages >= read.target_pages || read.next_cursor.is_none() {
                    let read = self.runs_read.take().unwrap();
                    self.runs = read.items;
                    self.loaded_pages = read.pages;
                    self.next_cursor = read.next_cursor;
                }
            }
            (
                Pending::Execution { thread },
                FeatureReply::Query(Ok(QueryReply::Execution(ExecutionReply::ThreadExecution(
                    execution,
                )))),
            ) => {
                if self.thread.as_ref() == Some(&thread) {
                    self.execution = Some(*execution);
                }
            }
            (Pending::Runs { .. }, FeatureReply::Query(Err(error))) => {
                let read = self.runs_read.take().unwrap();
                if error.code == Some(ErrorCode::Conflict) && read.next_cursor.is_some() {
                    // Checkpoints can invalidate a cursor without a RunChanged.
                    // Preserve the display and requested depth, but read a fresh
                    // snapshot from the start instead of resending that cursor.
                    self.runs_read = Some(RunsRead {
                        target_pages: read.target_pages,
                        ..Default::default()
                    });
                } else {
                    self.status = error.message;
                }
            }
            (Pending::Plan { kind, generation }, reply) => {
                let answer = match reply {
                    FeatureReply::Query(Ok(QueryReply::Execution(reply))) => Ok(reply),
                    FeatureReply::Query(Err(error)) => Err(read_failure(&error)),
                    _ => Err(ReadFailure::Other("unexpected reply".into())),
                };
                self.plan.apply(kind, generation, answer);
            }
            (Pending::Execution { .. }, FeatureReply::Query(Err(error))) => {
                self.status = error.message;
            }
            (Pending::Command(label), FeatureReply::Command(Ok(_))) => {
                self.status = format!("{label}: accepted");
                self.runs_stale = true;
                self.execution_stale = true;
            }
            (Pending::Command(label), FeatureReply::Command(Err(error))) => {
                self.status = command_failure(label, &error);
                if error.outcome_unknown {
                    // The resolved reply for this tag follows after the reconnect.
                    self.pending.insert(tag, Pending::Command(label));
                }
                self.runs_stale = true;
                self.execution_stale = true;
            }
            _ => {}
        }
    }

    fn maintain(&mut self, ctx: &mut FeatureContext<'_>, visibility: Visibility) {
        let selected = ctx.selection().thread.clone();
        if selected != self.thread {
            self.switch_thread(selected);
        }
        let drawn = std::mem::take(&mut self.plan_drawn);
        if drawn && !self.plan_shown {
            // A tab shown again may have missed changes while it was hidden.
            self.plan.refresh();
        }
        self.plan_shown = drawn;
        for action in std::mem::take(&mut self.plan_actions) {
            match action {
                PlanPanelAction::Select(task) => self.plan.select(&task),
                PlanPanelAction::HistoryPage(page) => self.plan.set_history_page(page),
            }
        }
        let Some(thread) = self.thread.clone().filter(|_| visibility.available) else {
            self.goal_actions.clear();
            return;
        };
        if self.execution_stale {
            self.execution_stale = false;
            let tag = self.tag(Pending::Execution {
                thread: thread.clone(),
            });
            ctx.query(
                tag,
                Query::Execution(ExecutionQuery::GetThreadExecution(ThreadTarget {
                    thread: thread.clone(),
                })),
            );
        }
        for request in self.plan.requests(self.plan_shown) {
            let tag = self.tag(Pending::Plan {
                kind: request.kind,
                generation: request.generation,
            });
            ctx.query(tag, Query::Execution(request.query));
        }
        self.send_goal_actions(ctx, &thread);
    }

    fn render(&mut self, ui: &mut egui::Ui, ctx: &mut FeatureContext<'_>) {
        let Some(thread) = self.thread.clone() else {
            no_runs(ui);
            return;
        };
        self.request_runs(ctx);
        if !self.status.is_empty() {
            ui.label(&self.status);
        }
        self.draw_runs(ui, ctx, &thread);
        self.draw_working_note(ui);
    }

    fn as_execution(&self) -> Option<&ExecutionFeature> {
        Some(self)
    }

    fn as_execution_mut(&mut self) -> Option<&mut ExecutionFeature> {
        Some(self)
    }
}

fn read_failure(error: &FeatureError) -> ReadFailure {
    match error.code {
        Some(ErrorCode::Conflict) => ReadFailure::Conflict,
        Some(ErrorCode::NotFound) => ReadFailure::NotFound,
        _ => ReadFailure::Other(error.message.clone()),
    }
}

fn command_failure(label: &str, error: &FeatureError) -> String {
    if error.outcome_unknown {
        format!("{label}: outcome unknown (not resent) — {}", error.message)
    } else {
        format!("{label}: {}", error.message)
    }
}

#[cfg(test)]
pub(crate) mod tests;
