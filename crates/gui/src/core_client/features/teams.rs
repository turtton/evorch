//! Desktop feature of the `teams` area: the board of the selected thread, its
//! members and its tasks, with one task opened for its detail. Read only: the
//! board is configured elsewhere.
//!
//! As in the other areas, an event only says the board changed; the board and
//! the tasks are then asked for again.
use std::collections::BTreeMap;

use super::layout::{bounded_list, paged_list};
use super::{Availability, Feature, FeatureContext, FeatureId, FeatureReply};
use workbench_protocol::{
    commands::ThreadTarget,
    events::UiEvent,
    handshake::Capability,
    ids::{PageCursor, ThreadId},
    namespaces::teams::{
        TeamOverview, TeamSource, TeamTaskRef, TeamTaskState, TeamTaskView, TeamsEvent, TeamsQuery,
        TeamsReply,
    },
    queries::{PageRequest, Query, QueryReply, ThreadQuery},
};

#[derive(Debug, Clone)]
enum Pending {
    Team(ThreadId),
    Tasks { thread: ThreadId, append: bool },
    Task { thread: ThreadId, task_id: String },
}

#[derive(Default)]
pub struct TeamsFeature {
    next_tag: u64,
    pending: BTreeMap<u64, Pending>,
    thread: Option<ThreadId>,
    team: Option<TeamOverview>,
    tasks: Vec<TeamTaskView>,
    next_cursor: Option<PageCursor>,
    /// The task opened for its detail, with the detail once it arrived.
    opened: Option<String>,
    detail: Option<TeamTaskView>,
    status: String,
    stale: bool,
    detail_stale: bool,
    more_requested: bool,
}

impl TeamsFeature {
    fn tag(&mut self, pending: Pending) -> u64 {
        self.next_tag += 1;
        self.pending.insert(self.next_tag, pending);
        self.next_tag
    }

    fn switch_thread(&mut self, thread: Option<ThreadId>) {
        self.thread = thread;
        self.team = None;
        self.tasks.clear();
        self.next_cursor = None;
        self.opened = None;
        self.detail = None;
        self.status.clear();
        self.stale = self.thread.is_some();
    }
}

pub(crate) fn task_state_label(state: TeamTaskState) -> &'static str {
    match state {
        TeamTaskState::Ready => "待機",
        TeamTaskState::Claimed => "担当中",
        TeamTaskState::Complete => "完了",
    }
}

fn source_label(source: TeamSource) -> &'static str {
    match source {
        TeamSource::Live => "実行中のボード",
        TeamSource::Persisted => "保存済みのボード（履歴のみ）",
    }
}

impl Feature for TeamsFeature {
    fn id(&self) -> FeatureId {
        FeatureId::Teams
    }

    fn title(&self) -> &'static str {
        "Teams"
    }

    fn availability(&self) -> Availability {
        Availability::Implemented {
            required: &[Capability::Queries],
        }
    }

    fn reset(&mut self) {
        self.team = None;
        self.tasks.clear();
        self.next_cursor = None;
        self.detail = None;
        self.stale = self.thread.is_some();
        self.detail_stale = self.opened.is_some();
    }

    fn event(&mut self, event: &UiEvent) {
        let UiEvent::Teams(TeamsEvent::TeamChanged { team }) = event else {
            return;
        };
        if self.thread.as_ref() == Some(&team.thread) {
            self.stale = true;
            self.detail_stale = self.opened.is_some();
        }
    }

    fn reply(&mut self, tag: u64, reply: FeatureReply) {
        let Some(pending) = self.pending.remove(&tag) else {
            return;
        };
        let FeatureReply::Query(result) = reply else {
            return;
        };
        let reply = match result {
            Ok(QueryReply::Teams(reply)) => reply,
            Ok(_) => return,
            Err(error) => {
                self.status = error.message;
                return;
            }
        };
        match (pending, reply) {
            (Pending::Team(thread), TeamsReply::Team(team)) => {
                if self.thread.as_ref() == Some(&thread) {
                    self.team = Some(team);
                    self.status.clear();
                }
            }
            (Pending::Tasks { thread, append }, TeamsReply::Tasks(page)) => {
                if self.thread.as_ref() != Some(&thread) {
                    return;
                }
                if append {
                    for task in page.items {
                        if !self.tasks.iter().any(|t| t.task_id == task.task_id) {
                            self.tasks.push(task);
                        }
                    }
                } else {
                    self.tasks = page.items;
                }
                self.next_cursor = page.next_cursor;
            }
            (Pending::Task { thread, task_id }, TeamsReply::Task(task))
                if self.thread.as_ref() == Some(&thread)
                    && self.opened.as_ref() == Some(&task_id) =>
            {
                self.detail = Some(task);
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
            ui.label("会話を選択するとチームボードを表示します。");
            return;
        };
        if self.stale {
            self.stale = false;
            let tag = self.tag(Pending::Team(thread.clone()));
            ctx.query(
                tag,
                Query::Teams(TeamsQuery::GetTeam(ThreadTarget {
                    thread: thread.clone(),
                })),
            );
            let tag = self.tag(Pending::Tasks {
                thread: thread.clone(),
                append: false,
            });
            ctx.query(
                tag,
                Query::Teams(TeamsQuery::ListTeamTasks(ThreadQuery {
                    thread: thread.clone(),
                    page: PageRequest::default(),
                })),
            );
        }
        if self.more_requested {
            self.more_requested = false;
            let tag = self.tag(Pending::Tasks {
                thread: thread.clone(),
                append: true,
            });
            ctx.query(
                tag,
                Query::Teams(TeamsQuery::ListTeamTasks(ThreadQuery {
                    thread: thread.clone(),
                    page: PageRequest {
                        cursor: self.next_cursor.clone(),
                        ..PageRequest::default()
                    },
                })),
            );
        }
        if self.detail_stale {
            self.detail_stale = false;
            if let Some(task_id) = self.opened.clone() {
                let tag = self.tag(Pending::Task {
                    thread: thread.clone(),
                    task_id: task_id.clone(),
                });
                ctx.query(
                    tag,
                    Query::Teams(TeamsQuery::GetTeamTask(TeamTaskRef {
                        thread: thread.clone(),
                        task_id,
                    })),
                );
            }
        }
        if !self.status.is_empty() {
            ui.label(&self.status);
        }
        let Some(team) = &self.team else {
            ui.label("このスレッドにチームボードはありません。");
            return;
        };
        ui.label(format!(
            "{} — 待機 {} / 担当中 {} / 完了 {}",
            source_label(team.source),
            team.ready,
            team.claimed,
            team.complete
        ));
        bounded_list(ui, "teams-members", |ui| {
            for member in &team.members {
                ui.label(format!(
                    "メンバー {} ({} 件担当){}",
                    member.owner,
                    member.claimed_tasks,
                    member
                        .run
                        .as_ref()
                        .map_or(String::new(), |run| format!(" run {}", run.as_str()))
                ));
            }
        });
        ui.separator();
        let mut open = None;
        let tasks = &self.tasks;
        let more = paged_list(
            ui,
            "teams-tasks",
            self.next_cursor.is_some().then_some("さらに読み込む"),
            |ui| {
                for task in tasks {
                    ui.push_id(&task.task_id, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            if ui.button(&task.task_id).clicked() {
                                open = Some(task.task_id.clone());
                            }
                            ui.label(task_state_label(task.state));
                            if let Some(owner) = &task.owner {
                                ui.label(owner);
                            }
                        });
                    });
                }
            },
        );
        if more {
            self.more_requested = true;
        }
        if let Some(task_id) = open {
            self.opened = Some(task_id);
            self.detail = None;
            self.detail_stale = true;
        }
        if let Some(task) = &self.detail {
            ui.separator();
            ui.strong(format!("タスク {}", task.task_id));
            ui.label(format!(
                "{} / claim 世代 {}",
                task_state_label(task.state),
                task.claim_generation.0
            ));
            if let Some(owner) = &task.owner {
                ui.label(format!("担当: {owner}"));
            }
            if let Some(run) = &task.owner_run {
                ui.label(format!("担当 run: {}", run.as_str()));
            }
            bounded_list(ui, "teams-task-paths", |ui| {
                for path in &task.paths {
                    ui.label(path);
                }
            });
        }
    }
}

#[cfg(test)]
mod tests;
