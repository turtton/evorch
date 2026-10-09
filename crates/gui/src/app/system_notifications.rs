use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use event_bus::{AgentRunPhase, Event, EventKind, LifecycleEvent, ToolEvent};

use super::WorkbenchState;
use crate::model::system_notifications::{SystemNotification, SystemNotificationSink};
use crate::model::tasks::AgentRunSource;

#[derive(Default)]
pub(super) struct SystemNotifications {
    sink: Option<Arc<dyn SystemNotificationSink>>,
    questions: BTreeSet<String>,
    // context_len counts retained raw non-system messages, not the compacted
    // provider projection, so completed turn boundaries remain distinct.
    boundaries: BTreeSet<(String, u64)>,
    /// TurnCompleted and Done can both describe the same completed work.
    completed: BTreeMap<String, bool>,
    pending: Vec<SystemNotification>,
}

impl SystemNotifications {
    /// Record identity even when delivery is suppressed by focus or history replay.
    fn observe(&mut self, event: &Event) -> bool {
        match &event.kind {
            EventKind::Tool(ToolEvent::UserQuestionUpdated { question }) => {
                let unseen = self.questions.insert(question.id.clone());
                unseen && question.answer.is_none() && question.is_user_visible()
            }
            EventKind::Lifecycle(LifecycleEvent::TurnCompleted {
                run_id,
                context_len,
            }) => {
                self.completed.insert(run_id.clone(), true);
                self.boundaries.insert((run_id.clone(), *context_len))
            }
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                run_id,
                to,
                reason,
                ..
            }) => match to {
                AgentRunPhase::Running => {
                    self.completed.insert(run_id.clone(), false);
                    false
                }
                AgentRunPhase::Done => {
                    let prior = self.completed.insert(run_id.clone(), true).unwrap_or(false);
                    !prior && reason.as_deref() != Some("escalated")
                }
                _ => false,
            },
            _ => false,
        }
    }
}

impl<S: AgentRunSource> WorkbenchState<S> {
    /// Enable delivery explicitly. Fixtures and headless workbenches have no OS sink.
    pub fn with_system_notifications(mut self, sink: Arc<dyn SystemNotificationSink>) -> Self {
        self.system_notifications.sink = Some(sink);
        self
    }

    pub(super) fn seed_system_notification(&mut self, event: &Event) {
        self.system_notifications.observe(event);
    }

    pub(super) fn seed_pending_question_notifications(&mut self) {
        self.system_notifications
            .questions
            .extend(self.user_questions.keys().cloned());
    }

    pub(super) fn fold_system_notification(&mut self, event: &Event) {
        if !self.system_notifications.observe(event) {
            return;
        }
        let notification = match &event.kind {
            EventKind::Tool(ToolEvent::UserQuestionUpdated { question }) => SystemNotification {
                title: "evorch: 回答が必要です".into(),
                body: bounded_text(&question.title),
            },
            EventKind::Lifecycle(
                LifecycleEvent::TurnCompleted { run_id, .. }
                | LifecycleEvent::AgentRunStateChanged {
                    run_id,
                    to: AgentRunPhase::Done,
                    ..
                },
            ) => {
                if !self.is_conversation_run(run_id)
                    || self.user_questions.values().any(|question| {
                        question.is_user_visible() && question.root_run_id == *run_id
                    })
                {
                    return;
                }
                let Some(thread) = self
                    .sidebar
                    .threads
                    .iter()
                    .find(|thread| thread.run_ids.iter().any(|run| run == run_id))
                else {
                    return;
                };
                if self
                    .thread_goals
                    .get(&thread.id.to_string())
                    .is_some_and(|goal| {
                        goal.phase != event_bus::ThreadGoalPhase::Complete
                            || goal.checks_paused
                            || goal.work_stopped
                    })
                {
                    return;
                }
                SystemNotification {
                    title: "evorch: 作業が完了しました".into(),
                    body: bounded_text(&thread.title),
                }
            }
            _ => return,
        };
        if self.system_notifications.sink.is_some() && self.system_notifications.pending.len() < 32
        {
            self.system_notifications.pending.push(notification);
        }
    }

    pub(super) fn dispatch_system_notifications(&mut self, ctx: &egui::Context) {
        let inactive = ctx.input(|input| {
            // run_logic updates raw focus without refreshing InputState::focused.
            !input.raw.focused
                && !input.raw.viewports.is_empty()
                && input
                    .raw
                    .viewports
                    .values()
                    .all(|viewport| viewport.focused == Some(false))
        });
        // Consume focused/unknown deliveries too: changing focus never replays them.
        for notification in self.system_notifications.pending.drain(..) {
            if inactive && let Some(sink) = &self.system_notifications.sink {
                sink.send(notification);
            }
        }
    }
}

fn bounded_text(text: &str) -> String {
    let mut output = String::new();
    let mut count = 0;
    for word in text.split_whitespace() {
        if !output.is_empty() {
            output.push(' ');
            count += 1;
        }
        for character in word.chars().filter(|character| !character.is_control()) {
            if count == 240 {
                return output;
            }
            output.push(character);
            count += 1;
        }
        if count == 240 {
            break;
        }
    }
    output
}
