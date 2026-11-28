//! The Notifications tab of the core client: what this window saw happen in the core's
//! projection since it opened. It is client-local and session-only.
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use egui::{Color32, Ui};
use workbench_protocol::ids::{ProjectId, ThreadId};
use workbench_protocol::views::{ApprovalState, QuestionTarget, RunPhase};
use workbench_protocol::wire::InstanceEpoch;
use workspace_ui::ThreadRunPhase;

use super::cache::Cache;
use crate::panes::notification_widgets::{NotificationRowView, notification_row};
use crate::theme::tokens::{
    CANCELLED_LABEL, attention_color, cancelled_color, palette, phase_color, phase_label,
};
use crate::theme::widgets::empty_state;

pub const MAX_NOTIFICATIONS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationKind {
    Question,
    Approval,
    Completed,
    Failed,
    Cancelled,
}

impl NotificationKind {
    fn badge(self) -> (&'static str, Color32) {
        match self {
            Self::Question => ("Question", attention_color()),
            Self::Approval => ("Approval", attention_color()),
            Self::Completed => terminal(ThreadRunPhase::Done),
            Self::Failed => (phase_label(ThreadRunPhase::Error), palette().ERROR_FG),
            Self::Cancelled => (CANCELLED_LABEL, cancelled_color()),
        }
    }
}

fn terminal(phase: ThreadRunPhase) -> (&'static str, Color32) {
    (phase_label(phase), phase_color(phase))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub id: u64,
    pub kind: NotificationKind,
    pub summary: String,
    pub thread: ThreadId,
    pub project: Option<ProjectId>,
    pub unread: bool,
}

#[derive(Default)]
pub struct NotificationLog {
    items: VecDeque<Notification>,
    next_id: u64,
    questions: BTreeSet<String>,
    approvals: BTreeSet<String>,
    phases: BTreeMap<String, RunPhase>,
    /// The epoch the run phases were primed for; `None` until the first observation.
    primed: Option<Option<InstanceEpoch>>,
}

const fn finished(phase: RunPhase) -> bool {
    matches!(
        phase,
        RunPhase::Done | RunPhase::Failed | RunPhase::Cancelled
    )
}

impl NotificationLog {
    /// Lists what changed in `cache` since the last call. Question and approval ids are
    /// remembered across snapshots, so a resync never lists one twice. Run phases are
    /// taken in silently the first time and whenever the core's epoch changes, so a new
    /// snapshot never reads as runs that just ended. After that, a root run new to this
    /// log started since the last call, so it is listed if it has already ended.
    ///
    /// The cache keeps no time order across items, so what one call lists is ordered by
    /// kind (questions, approvals, then run outcomes) and by id within a kind; when one
    /// call lists more than [`MAX_NOTIFICATIONS`], the cap keeps the ones listed last.
    pub fn observe(&mut self, cache: &Cache) {
        let project_of = |thread: &ThreadId| {
            cache
                .threads
                .get(thread.as_str())
                .map(|summary| summary.project.clone())
        };
        for question in cache.questions.values() {
            if question.is_pending()
                && question.target == QuestionTarget::User
                && self.questions.insert(question.id.to_string())
            {
                let thread = question.requester.thread.clone();
                self.push(
                    NotificationKind::Question,
                    format!("Question: {}", question.title),
                    project_of(&thread),
                    thread,
                );
            }
        }
        for approval in cache.approvals.values() {
            if approval.state == ApprovalState::Pending
                && self.approvals.insert(approval.id.to_string())
            {
                self.push(
                    NotificationKind::Approval,
                    format!("Approval requested: {}", approval.tool_name),
                    project_of(&approval.thread),
                    approval.thread.clone(),
                );
            }
        }
        let epoch = cache.cursor().map(|cursor| cursor.core_epoch);
        if self.primed != Some(epoch) {
            self.primed = Some(epoch);
            self.phases = cache
                .runs
                .iter()
                .map(|(id, run)| (id.clone(), run.phase))
                .collect();
            return;
        }
        for (id, run) in &cache.runs {
            let before = self.phases.insert(id.clone(), run.phase);
            if run.parent_run.is_some() || !finished(run.phase) || before.is_some_and(finished) {
                continue;
            }
            let title = cache
                .threads
                .get(run.thread.as_str())
                .map_or_else(|| run.thread.to_string(), |thread| thread.title.clone());
            let (kind, summary) = match run.phase {
                RunPhase::Done => (NotificationKind::Completed, format!("{title} completed")),
                RunPhase::Failed => (NotificationKind::Failed, format!("{title} failed")),
                _ => (NotificationKind::Cancelled, format!("{title} cancelled")),
            };
            self.push(kind, summary, project_of(&run.thread), run.thread.clone());
        }
    }

    fn push(
        &mut self,
        kind: NotificationKind,
        summary: String,
        project: Option<ProjectId>,
        thread: ThreadId,
    ) {
        let id = self.next_id;
        self.next_id += 1;
        self.items.push_back(Notification {
            id,
            kind,
            summary,
            thread,
            project,
            unread: true,
        });
        if self.items.len() > MAX_NOTIFICATIONS {
            self.items.pop_front();
        }
    }

    pub fn items(&self) -> impl Iterator<Item = &Notification> {
        self.items.iter()
    }

    pub fn unread_count(&self) -> usize {
        self.items.iter().filter(|item| item.unread).count()
    }
}

/// Draws the log, oldest first. A row drawn in view while the window has focus is read.
/// Returns the thread whose notification was clicked, with its project when known.
pub fn show(
    ui: &mut Ui,
    log: &mut NotificationLog,
    focused: bool,
) -> Option<(Option<ProjectId>, ThreadId)> {
    let mut opened = None;
    let mut seen = Vec::new();
    egui::ScrollArea::vertical()
        .id_salt("core-notifications")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if log.items.is_empty() {
                empty_state(
                    ui,
                    "No notifications yet",
                    "Run completions, failures and approval requests will appear here.",
                    None,
                );
            }
            for item in &log.items {
                let (label, accent) = item.kind.badge();
                let clicks = ui
                    .push_id(("core-notification", item.id), |ui| {
                        notification_row(
                            ui,
                            NotificationRowView {
                                label,
                                accent,
                                unread: item.unread,
                                summary: &item.summary,
                                clickable: true,
                            },
                        )
                    })
                    .inner;
                if clicks.summary {
                    opened = Some((item.project.clone(), item.thread.clone()));
                }
                if item.unread && ui.is_rect_visible(clicks.rect) {
                    seen.push(item.id);
                }
            }
        });
    if focused && !seen.is_empty() {
        for item in log.items.iter_mut().filter(|item| seen.contains(&item.id)) {
            item.unread = false;
        }
        ui.ctx().request_repaint();
    }
    opened
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::{Harness, kittest::Queryable};
    use serde_json::json;
    use workbench_protocol::events::EventCursor;
    use workbench_protocol::views::{ApprovalView, QuestionView, RunSummary, ThreadSummary};
    use workbench_protocol::wire::Sequence;

    fn view<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> T {
        serde_json::from_value(value).unwrap()
    }

    fn cache(epoch: u64) -> Cache {
        let mut cache = Cache::default();
        cache.at(EventCursor {
            core_epoch: InstanceEpoch(epoch),
            sequence: Sequence(0),
        });
        let thread: ThreadSummary = view(json!({
            "id": "thr-1", "project": "proj-1", "title": "Fix login redirect",
            "state": "running", "revision": "1", "updated_at": "1",
        }));
        cache.threads.insert("thr-1".into(), thread);
        cache
    }

    fn run(cache: &mut Cache, id: &str, parent: Option<&str>, phase: &str) {
        let run: RunSummary = view(json!({
            "id": id, "thread": "thr-1", "parent_run": parent, "role": "orchestrator",
            "phase": phase, "revision": "1",
        }));
        cache.runs.insert(id.into(), run);
    }

    fn pending(cache: &mut Cache) {
        let question: QuestionView = view(json!({
            "id": "q-1",
            "requester": {"run": "run-1", "thread": "thr-1", "root_run": "run-1"},
            "target": {"type": "user"},
            "management_revision": "1", "routing_revision": "1", "blocking": true,
            "title": "Which redirect?", "options": [], "revision": "1",
            "raised_by": {"actor": "agent", "agent_run_id": "run-1", "surface": "none"},
        }));
        cache.questions.insert("q-1".into(), question);
        let approval: ApprovalView = view(json!({
            "id": "ap-1", "core_epoch": "7", "revision": "1", "thread": "thr-1",
            "run": "run-1", "tool_name": "shell", "summary": "cargo test",
            "summary_truncated": false, "input_bytes": "10", "state": "pending",
        }));
        cache.approvals.insert("ap-1".into(), approval);
    }

    fn summaries(log: &NotificationLog) -> Vec<&str> {
        log.items().map(|item| item.summary.as_str()).collect()
    }

    // Given: a pending question and approval, already listed
    // When: a resync from a restarted core brings the same ones back
    // Then: each is listed once
    #[test]
    fn a_pending_question_and_approval_are_listed_once_across_a_resync() {
        let mut log = NotificationLog::default();
        let mut first = cache(7);
        pending(&mut first);
        log.observe(&first);
        log.observe(&first);
        let mut resync = cache(8);
        pending(&mut resync);
        log.observe(&resync);
        assert_eq!(
            summaries(&log),
            ["Question: Which redirect?", "Approval requested: shell"]
        );
        let item = log.items().next().unwrap();
        assert_eq!(item.thread.as_str(), "thr-1");
        assert_eq!(item.project.as_ref().map(ProjectId::as_str), Some("proj-1"));
    }

    // Given: a root run and a child run, first seen running or already done
    // When: they end, and a new core's snapshot brings finished runs
    // Then: only the root run's end is listed
    #[test]
    fn a_root_run_ending_lists_its_outcome_but_child_runs_and_snapshots_do_not() {
        let mut log = NotificationLog::default();
        let mut state = cache(7);
        run(&mut state, "run-1", None, "running");
        run(&mut state, "run-2", Some("run-1"), "running");
        run(&mut state, "run-0", None, "done");
        log.observe(&state);
        assert!(
            summaries(&log).is_empty(),
            "the first observation lists nothing"
        );

        run(&mut state, "run-2", Some("run-1"), "failed");
        run(&mut state, "run-1", None, "done");
        log.observe(&state);
        log.observe(&state);
        assert_eq!(summaries(&log), ["Fix login redirect completed"]);

        run(&mut state, "run-1", None, "running");
        log.observe(&state);
        run(&mut state, "run-1", None, "cancelled");
        log.observe(&state);
        run(&mut state, "run-3", None, "running");
        log.observe(&state);
        run(&mut state, "run-3", None, "failed");
        log.observe(&state);
        assert_eq!(
            summaries(&log),
            [
                "Fix login redirect completed",
                "Fix login redirect cancelled",
                "Fix login redirect failed",
            ]
        );

        let mut restarted = cache(8);
        run(&mut restarted, "run-4", None, "done");
        log.observe(&restarted);
        assert_eq!(
            log.items().count(),
            3,
            "a new core's snapshot lists nothing"
        );
    }

    // Given: the log primed on a snapshot
    // When: root runs it has never seen are first observed already done, failed or
    //       cancelled, as when they start and end between two frames
    // Then: each outcome is listed, but a child run that did the same is not
    #[test]
    fn a_root_run_that_starts_and_ends_between_observations_is_listed() {
        let mut log = NotificationLog::default();
        let mut state = cache(7);
        run(&mut state, "run-0", None, "done");
        log.observe(&state);

        run(&mut state, "run-1", None, "done");
        run(&mut state, "run-2", None, "failed");
        run(&mut state, "run-3", None, "cancelled");
        run(&mut state, "run-4", Some("run-1"), "failed");
        log.observe(&state);
        log.observe(&state);
        assert_eq!(
            summaries(&log),
            [
                "Fix login redirect completed",
                "Fix login redirect failed",
                "Fix login redirect cancelled",
            ]
        );
    }

    // Given: more pending approvals than the log keeps, all new in one observation
    // When: they are observed
    // Then: only the 64 listed last (by id within one observation) remain
    #[test]
    fn the_log_keeps_the_last_64_listed() {
        let mut log = NotificationLog::default();
        let mut state = cache(7);
        for n in 0..MAX_NOTIFICATIONS + 6 {
            let approval: ApprovalView = view(json!({
                "id": format!("ap-{n:03}"), "core_epoch": "7", "revision": "1",
                "thread": "thr-1", "run": "run-1", "tool_name": format!("tool-{n:03}"),
                "summary": "", "summary_truncated": false, "input_bytes": "0",
                "state": "pending",
            }));
            state.approvals.insert(format!("ap-{n:03}"), approval);
        }
        log.observe(&state);
        assert_eq!(log.items().count(), MAX_NOTIFICATIONS);
        assert_eq!(
            log.items().next().unwrap().summary,
            "Approval requested: tool-006"
        );
    }

    fn harness(log: NotificationLog, focused: bool) -> Harness<'static, NotificationLog> {
        Harness::new_ui_state(
            move |ui, log: &mut NotificationLog| {
                show(ui, log, focused);
            },
            log,
        )
    }

    // Given: two unread notifications
    // When: the tab draws them while the window is not focused, then while it is
    // Then: they stay unread at first and are read once seen with focus
    #[test]
    fn drawing_while_focused_marks_entries_read() {
        let mut log = NotificationLog::default();
        let mut state = cache(7);
        pending(&mut state);
        log.observe(&state);

        let mut unfocused = harness(log, false);
        unfocused.run();
        assert_eq!(unfocused.state().unread_count(), 2);
        assert_eq!(unfocused.get_all_by_label("Unread").count(), 2);

        let log = std::mem::take(unfocused.state_mut());
        let mut focused = harness(log, true);
        focused.run();
        assert_eq!(focused.state().unread_count(), 0);
        assert_eq!(focused.get_all_by_label("Read").count(), 2);
    }

    // Given: a notification about thr-1
    // When: its summary is clicked
    // Then: the thread and its project are returned to open
    #[test]
    fn a_summary_click_opens_its_thread() {
        let mut log = NotificationLog::default();
        let mut state = cache(7);
        pending(&mut state);
        log.observe(&state);
        let mut harness = Harness::new_ui_state(
            |ui, (log, opened): &mut (NotificationLog, Option<(Option<ProjectId>, ThreadId)>)| {
                if let Some(target) = show(ui, log, true) {
                    *opened = Some(target);
                }
            },
            (log, None),
        );
        harness.run();
        harness.get_by_label("Approval requested: shell").click();
        harness.run();
        let (project, thread) = harness.state().1.clone().expect("a click opens a thread");
        assert_eq!(project.as_ref().map(ProjectId::as_str), Some("proj-1"));
        assert_eq!(thread.as_str(), "thr-1");
    }
}
