use egui::Ui;
use event_bus::AgentRunPhase;
use workspace_ui::{ThreadId, ThreadRunPhase};

use crate::model::notifications::{NotificationKind, NotificationsModel};
use crate::panes::notification_widgets::{NotificationRowView, notification_row};
use crate::theme::tokens::{
    CANCELLED_LABEL, attention_color, cancelled_color, palette, phase_color, phase_label,
};
use crate::theme::widgets::empty_state;

#[derive(Debug, PartialEq, Eq)]
pub enum NotificationsAction {
    OpenRun(String),
    OpenConversation(String),
    OpenThread(ThreadId),
    /// Show the Evaluations panel, read afresh.
    OpenEvaluations,
}

pub fn notifications_pane(
    ui: &mut Ui,
    model: &mut NotificationsModel,
    outer_focused: Option<bool>,
) -> Option<NotificationsAction> {
    let mut action = None;
    let mut displayed = Vec::new();
    egui::ScrollArea::vertical().show(ui, |ui| {
        if model.items().next().is_none() {
            empty_state(
                ui,
                "No notifications yet",
                "Run completions, failures and approval requests will appear here.",
                None,
            );
        }
        for notification in model.items() {
            let revision = model.revision(notification.id);
            let unread = model.is_unread(notification.id);
            let (label, accent) = match &notification.kind {
                NotificationKind::QuestionPending { .. } => ("Question", attention_color()),
                NotificationKind::ApprovalPending { .. } => ("Approval", attention_color()),
                NotificationKind::RunStopped => terminal(ThreadRunPhase::Stopped),
                NotificationKind::RunCompleted => terminal(ThreadRunPhase::Done),
                NotificationKind::RunFailed { reason }
                    if AgentRunPhase::Error.is_cancellation(reason.as_deref()) =>
                {
                    (CANCELLED_LABEL, cancelled_color())
                }
                NotificationKind::RunFailed { .. } => terminal(ThreadRunPhase::Error),
                NotificationKind::RetentionStartup => ("Storage", terminal(ThreadRunPhase::Done).1),
                NotificationKind::RetentionDisabled { .. } => ("Storage", attention_color()),
                NotificationKind::HistoryGap => ("History", attention_color()),
                NotificationKind::EvaluationFinished { all_reported: true } => {
                    ("Evaluation", terminal(ThreadRunPhase::Done).1)
                }
                NotificationKind::EvaluationFinished {
                    all_reported: false,
                } => ("Evaluation", attention_color()),
                NotificationKind::PullRequestChecksFailed { .. } => {
                    ("CI failed", palette().ERROR_FG)
                }
                NotificationKind::PullRequestMerged { .. } => ("Merged", palette().MERGED_FG),
                NotificationKind::PullRequestAutoFixLimit { .. } => ("Auto fix", attention_color()),
            };
            let pull_request_thread = match &notification.kind {
                NotificationKind::PullRequestChecksFailed { thread_id }
                | NotificationKind::PullRequestAutoFixLimit { thread_id }
                | NotificationKind::PullRequestMerged { thread_id } => Some(thread_id.clone()),
                _ => None,
            };
            let evaluation = matches!(
                notification.kind,
                NotificationKind::EvaluationFinished { .. }
            );
            let clicks = ui
                .push_id(notification.id, |ui| {
                    notification_row(
                        ui,
                        NotificationRowView {
                            label,
                            accent,
                            unread,
                            summary: &notification.summary,
                            clickable: pull_request_thread.is_some()
                                || evaluation
                                || notification.run_id.is_some(),
                        },
                        |ui| {
                            let mut action = None;
                            if evaluation && ui.button("Open evaluations").clicked() {
                                action = Some(NotificationsAction::OpenEvaluations);
                            }
                            if let NotificationKind::QuestionPending { thread_id, .. } =
                                &notification.kind
                                && ui.button("Open thread").clicked()
                            {
                                action = thread_id
                                    .clone()
                                    .map(NotificationsAction::OpenThread)
                                    .or_else(|| {
                                        notification
                                            .run_id
                                            .clone()
                                            .map(NotificationsAction::OpenConversation)
                                    });
                            }
                            action
                        },
                    )
                })
                .inner;
            let summary = if clicks.summary {
                match (&pull_request_thread, &notification.run_id) {
                    (Some(thread_id), _) => {
                        Some(NotificationsAction::OpenThread(thread_id.clone()))
                    }
                    _ if evaluation => Some(NotificationsAction::OpenEvaluations),
                    (None, Some(run_id)) => Some(match &notification.kind {
                        NotificationKind::QuestionPending {
                            thread_id: Some(thread_id),
                            ..
                        } => NotificationsAction::OpenThread(thread_id.clone()),
                        NotificationKind::ApprovalPending { .. }
                        | NotificationKind::QuestionPending { .. } => {
                            NotificationsAction::OpenConversation(run_id.clone())
                        }
                        _ => NotificationsAction::OpenRun(run_id.clone()),
                    }),
                    (None, None) => None,
                }
            } else {
                None
            };
            if summary.is_some() {
                action = summary;
            }
            if let Some(clicked) = clicks.trailing {
                action = Some(clicked);
            }
            if ui.is_rect_visible(clicks.rect) {
                displayed.push((notification.id, revision, unread));
            }
        }
    });
    for (id, revision, unread) in displayed {
        if model.acknowledge(id, revision.as_ref(), outer_focused) && unread {
            ui.ctx().request_repaint();
        }
    }
    action
}

fn terminal(phase: ThreadRunPhase) -> (&'static str, egui::Color32) {
    (phase_label(phase), phase_color(phase))
}
