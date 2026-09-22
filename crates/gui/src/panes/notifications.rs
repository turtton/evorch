use egui::{RichText, Ui};

use crate::model::notifications::{NotificationKind, NotificationsModel};
use crate::theme::tokens::{FONT_BADGE, R_SM, STATUS_STROKE, palette};
use crate::theme::widgets::empty_state;

#[derive(Debug, PartialEq, Eq)]
pub enum NotificationsAction {
    OpenRun(String),
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
                NotificationKind::QuestionPending { .. } => ("question", palette().INFO),
                NotificationKind::RunCompleted => ("completed", palette().SUCCESS),
                NotificationKind::RunFailed { .. } => ("failed", palette().ERROR),
                NotificationKind::ApprovalPending { .. } => ("approval", palette().INFO),
                NotificationKind::MergeApprovalPending { .. } => ("merge approval", palette().INFO),
            };
            let row = ui
                .push_id(notification.id, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        egui::Frame::new()
                            .fill(if unread {
                                accent
                            } else {
                                egui::Color32::TRANSPARENT
                            })
                            .stroke(egui::Stroke::new(STATUS_STROKE, accent))
                            .corner_radius(egui::CornerRadius::same(R_SM))
                            .inner_margin(egui::Margin::symmetric(4, 0))
                            .show(ui, |ui| {
                                ui.label(RichText::new(label).size(FONT_BADGE).color(if unread {
                                    palette().CANVAS
                                } else {
                                    accent
                                }));
                            });
                        ui.label(crate::theme::text::muted(if unread {
                            "Unread"
                        } else {
                            "Read"
                        }));
                        let summary = RichText::new(&notification.summary);
                        let summary = if unread {
                            summary.strong().color(palette().TEXT)
                        } else {
                            summary.color(palette().TEXT_MUTED)
                        };
                        match &notification.run_id {
                            Some(run_id) => {
                                if ui
                                    .add(egui::Button::new(summary).frame(false).wrap())
                                    .clicked()
                                {
                                    action = Some(NotificationsAction::OpenRun(run_id.clone()));
                                }
                            }
                            None => {
                                ui.label(summary);
                            }
                        }
                    })
                    .response
                })
                .inner;
            if ui.is_rect_visible(row.rect) {
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
