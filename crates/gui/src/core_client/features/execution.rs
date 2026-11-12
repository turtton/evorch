//! Desktop feature of the `execution` area: the runs of the selected thread
//! (roots, then the children of one run), its goal, todo list and ledger.
//!
//! Everything shown is read from the core with a typed query; an event only
//! says that something changed, and the answer is asked for again, so an event
//! that arrives late never moves the screen back to an older state. A run that
//! left the runtime stays listed, with what became of it. Stop goes out for one
//! run of the shown thread only; the core refuses a run it no longer holds.
use std::collections::BTreeMap;

use super::{Availability, Feature, FeatureContext, FeatureError, FeatureId, FeatureReply};
use workbench_protocol::{
    commands::{Command, SetGoalChecksPaused, SetGoalReview, StopRun, ThreadTarget},
    events::UiEvent,
    handshake::Capability,
    ids::{PageCursor, RunId, ThreadId},
    namespaces::{
        ExecutionCommand, ExecutionEvent, ExecutionQuery, ExecutionReply,
        execution::{
            GoalPhase, GoalView, RunDetail, RunDetailsQuery, RunLifecycle, RunScope, RunSource,
            ThreadExecution, TodoStatus,
        },
    },
    queries::{PageRequest, Query, QueryReply},
};

/// What a request in flight was for.
#[derive(Debug, Clone)]
enum Pending {
    Runs {
        thread: ThreadId,
        path: Vec<RunId>,
        append: bool,
    },
    Execution {
        thread: ThreadId,
    },
    Command(&'static str),
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
    execution: Option<ThreadExecution>,
    status: String,
    runs_stale: bool,
    execution_stale: bool,
    more_requested: bool,
}

impl ExecutionFeature {
    fn tag(&mut self, pending: Pending) -> u64 {
        self.next_tag += 1;
        self.pending.insert(self.next_tag, pending);
        self.next_tag
    }

    fn switch_thread(&mut self, thread: Option<ThreadId>) {
        self.thread = thread;
        self.path.clear();
        self.runs.clear();
        self.next_cursor = None;
        self.execution = None;
        self.runs_stale = self.thread.is_some();
        self.execution_stale = self.thread.is_some();
    }

    /// Show the children of `run`: a new level below the current one.
    fn drill_into(&mut self, run: RunId) {
        self.path.push(run);
        self.runs.clear();
        self.next_cursor = None;
        self.runs_stale = true;
    }

    fn scope(&self) -> RunScope {
        match self.path.last() {
            Some(parent) => RunScope::Children {
                parent: parent.clone(),
            },
            None => RunScope::Roots,
        }
    }

    fn request_runs(&mut self, ctx: &mut FeatureContext<'_>, append: bool) {
        let Some(thread) = self.thread.clone() else {
            return;
        };
        let cursor = if append {
            self.next_cursor.clone()
        } else {
            None
        };
        let tag = self.tag(Pending::Runs {
            thread: thread.clone(),
            path: self.path.clone(),
            append,
        });
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
            if ui.button("ルート").clicked() && !self.path.is_empty() {
                self.path.clear();
                self.runs.clear();
                self.next_cursor = None;
                self.runs_stale = true;
            }
            for index in 0..self.path.len() {
                let id = self.path[index].clone();
                ui.label("›");
                if ui.button(id.as_str()).clicked() && index + 1 < self.path.len() {
                    self.path.truncate(index + 1);
                    self.runs.clear();
                    self.next_cursor = None;
                    self.runs_stale = true;
                    break;
                }
            }
        });
        if self.runs.is_empty() && !self.runs_stale {
            ui.label("この階層に run はありません。");
        }
        let mut drill = None;
        let mut stop = None;
        for run in &self.runs {
            ui.push_id(run.summary.id.as_str(), |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.strong(&run.name);
                    ui.label(format!("{:?}", run.summary.role));
                    ui.colored_label(
                        lifecycle_color(run.lifecycle),
                        lifecycle_label(run.lifecycle, run.source),
                    );
                    ui.label(format!("{:?}", run.summary.phase));
                    if let Some(model) = &run.model {
                        ui.label(model);
                    }
                    if run.child_count > 0
                        && ui.button(format!("子 run ({})", run.child_count)).clicked()
                    {
                        drill = Some(run.summary.id.clone());
                    }
                    if is_stoppable(run) && ui.button("停止").clicked() {
                        stop = Some(run.summary.id.clone());
                    }
                });
            });
        }
        if let Some(run) = drill {
            self.drill_into(run);
        }
        if let Some(run) = stop {
            self.command(ctx, "run の停止", stop_run(thread, run));
        }
        if self.next_cursor.is_some() && ui.button("さらに読み込む").clicked() {
            self.more_requested = true;
        }
    }

    fn draw_goal(&mut self, ui: &mut egui::Ui, ctx: &mut FeatureContext<'_>, thread: &ThreadId) {
        let Some(goal) = self.execution.as_ref().and_then(|e| e.goal.clone()) else {
            ui.label("ゴールは設定されていません。");
            return;
        };
        ui.strong(format!("ゴール: {}", goal.objective));
        ui.label(format!(
            "状態: {} / レビュー {}/{} 回{}{}",
            goal_phase_label(goal.phase),
            goal.review_round,
            goal.max_review_rounds,
            if goal.checks_paused {
                " / チェック一時停止中"
            } else {
                ""
            },
            if goal.work_stopped {
                " / 作業停止済み"
            } else {
                ""
            },
        ));
        if let Some(reason) = &goal.reason {
            ui.label(format!("理由: {reason}"));
        }
        for (index, criterion) in goal.criteria.iter().enumerate() {
            let met = goal
                .checks
                .iter()
                .find(|check| usize::try_from(check.criterion).is_ok_and(|c| c == index))
                .map(|check| check.met);
            let mark = match met {
                Some(true) => "✔",
                Some(false) => "✘",
                None => "·",
            };
            ui.label(format!("{mark} {criterion}"));
        }
        for finding in &goal.findings {
            ui.label(format!("指摘: {finding}"));
        }
        let mut review = goal.review_enabled;
        if ui.checkbox(&mut review, "ゴールレビュー").changed() {
            self.command(
                ctx,
                "ゴールレビューの切替",
                goal_review(thread, &goal, review),
            );
        }
        let mut paused = goal.checks_paused;
        if ui.checkbox(&mut paused, "チェックを一時停止").changed() {
            self.command(
                ctx,
                "ゴールチェックの一時停止",
                goal_checks_paused(thread, &goal, paused),
            );
        }
    }

    fn draw_todo_and_ledger(&self, ui: &mut egui::Ui) {
        let Some(execution) = &self.execution else {
            return;
        };
        if let Some(todo) = &execution.todo {
            ui.strong("Todo");
            for item in &todo.items {
                ui.label(format!("{} {}", todo_mark(item.status), item.content));
            }
        }
        if let Some(ledger) = &execution.ledger {
            ui.collapsing("台帳 (ledger)", |ui| {
                ui.label(&ledger.body);
                if ledger.truncated {
                    ui.label("（長いため先頭のみ表示）");
                }
            });
        }
    }
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

/// Whether Stop is offered: only a run the runtime still holds and that is
/// active. Anything else cannot be stopped from here.
pub(crate) fn is_stoppable(run: &RunDetail) -> bool {
    run.source == RunSource::Live && run.lifecycle == RunLifecycle::Active
}

/// The four ends a run can be in, and whether only its record remains.
pub(crate) fn lifecycle_label(lifecycle: RunLifecycle, source: RunSource) -> String {
    let state = match lifecycle {
        RunLifecycle::Active => "実行中",
        RunLifecycle::Terminal => "終了",
        RunLifecycle::Stopped => "停止・中止",
        RunLifecycle::Recovering => "復旧待ち",
    };
    match source {
        RunSource::Live => state.to_owned(),
        RunSource::HistoryOnly => format!("{state}（履歴のみ）"),
    }
}

fn lifecycle_color(lifecycle: RunLifecycle) -> egui::Color32 {
    match lifecycle {
        RunLifecycle::Active => egui::Color32::LIGHT_GREEN,
        RunLifecycle::Terminal => egui::Color32::GRAY,
        RunLifecycle::Stopped => egui::Color32::LIGHT_RED,
        RunLifecycle::Recovering => egui::Color32::YELLOW,
    }
}

pub(crate) fn goal_phase_label(phase: GoalPhase) -> &'static str {
    match phase {
        GoalPhase::Working => "作業中",
        GoalPhase::Checking => "チェック中",
        GoalPhase::Reviewing => "レビュー中",
        GoalPhase::Repairing => "修正中",
        GoalPhase::Complete => "完了",
        GoalPhase::Blocked => "ブロック中",
    }
}

fn todo_mark(status: TodoStatus) -> &'static str {
    match status {
        TodoStatus::Pending => "☐",
        TodoStatus::InProgress => "▶",
        TodoStatus::Completed => "☑",
        TodoStatus::Interrupted => "⏸",
    }
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
        let thread = self.thread.clone();
        let path = std::mem::take(&mut self.path);
        self.runs.clear();
        self.next_cursor = None;
        self.execution = None;
        self.runs_stale = thread.is_some();
        self.execution_stale = thread.is_some();
        self.path = path;
    }

    fn event(&mut self, event: &UiEvent) {
        let UiEvent::Execution(event) = event else {
            return;
        };
        let changed = match event {
            ExecutionEvent::RunChanged { run } => Some(&run.thread),
            ExecutionEvent::ThreadExecutionChanged { thread, .. } => Some(thread),
        };
        if changed == self.thread.as_ref() {
            // Ask again instead of patching: the core's answer is current.
            match event {
                ExecutionEvent::RunChanged { run } => {
                    self.runs_stale = true;
                    // The ledger, goal and todo shown belong to the thread's current root:
                    // a root that appears or ends may have changed which one that is.
                    if run.parent_run.is_none() {
                        self.execution_stale = true;
                    }
                }
                ExecutionEvent::ThreadExecutionChanged { .. } => self.execution_stale = true,
            }
        }
    }

    fn reply(&mut self, tag: u64, reply: FeatureReply) {
        let Some(pending) = self.pending.remove(&tag) else {
            return;
        };
        match (pending, reply) {
            (
                Pending::Runs {
                    thread,
                    path,
                    append,
                },
                FeatureReply::Query(Ok(QueryReply::Execution(ExecutionReply::RunDetails(page)))),
            ) => {
                if self.thread.as_ref() != Some(&thread) || self.path != path {
                    return;
                }
                if append {
                    for run in page.items {
                        if !self.runs.iter().any(|r| r.summary.id == run.summary.id) {
                            self.runs.push(run);
                        }
                    }
                } else {
                    self.runs = page.items;
                }
                self.next_cursor = page.next_cursor;
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
            (Pending::Runs { .. } | Pending::Execution { .. }, FeatureReply::Query(Err(error))) => {
                self.status = error.message;
            }
            (Pending::Command(label), FeatureReply::Command(Ok(_))) => {
                self.status = format!("{label}: 受け付けました");
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

    fn render(&mut self, ui: &mut egui::Ui, ctx: &mut FeatureContext<'_>) {
        let selected = ctx.selection().thread.clone();
        if selected != self.thread {
            self.switch_thread(selected);
        }
        let Some(thread) = self.thread.clone() else {
            ui.label("会話を選択すると run・ゴール・Todo を表示します。");
            return;
        };
        if self.runs_stale {
            self.runs_stale = false;
            self.request_runs(ctx, false);
        }
        if self.more_requested {
            self.more_requested = false;
            self.request_runs(ctx, true);
        }
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
        if !self.status.is_empty() {
            ui.label(&self.status);
        }
        self.draw_runs(ui, ctx, &thread);
        ui.separator();
        self.draw_goal(ui, ctx, &thread);
        self.draw_todo_and_ledger(ui);
    }
}

fn command_failure(label: &str, error: &FeatureError) -> String {
    if error.outcome_unknown {
        format!("{label}: 結果不明（再送していません）— {}", error.message)
    } else {
        format!("{label}: {}", error.message)
    }
}

#[cfg(test)]
pub(crate) mod tests;
