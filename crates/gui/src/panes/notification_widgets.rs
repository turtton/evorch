//! Notification rows drawn from plain text, shared by the embedded workbench and the
//! core client: neither the event bus nor the core's protocol reaches them.
use egui::{Color32, RichText, Ui};

use crate::theme::text::muted;
use crate::theme::tokens::{FONT_BADGE, R_SM, STATUS_STROKE, palette};

/// The kind badge of a notification: filled with `accent` while unread, outlined once read.
pub fn status_pill(ui: &mut Ui, label: &str, accent: Color32, unread: bool) {
    egui::Frame::new()
        .fill(if unread { accent } else { Color32::TRANSPARENT })
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
}

pub struct NotificationRowView<'a> {
    pub label: &'a str,
    pub accent: Color32,
    pub unread: bool,
    pub summary: &'a str,
    /// The summary is a button that opens what the notification is about.
    pub clickable: bool,
}

pub struct NotificationRowClicks<R> {
    pub summary: bool,
    pub rect: egui::Rect,
    pub trailing: R,
}

/// One notification: the badge, "Unread" or "Read", the summary, then whatever
/// `trailing` adds on the same wrapped line.
pub fn notification_row<R>(
    ui: &mut Ui,
    view: NotificationRowView<'_>,
    trailing: impl FnOnce(&mut Ui) -> R,
) -> NotificationRowClicks<R> {
    let inner = ui.horizontal_wrapped(|ui| {
        status_pill(ui, view.label, view.accent, view.unread);
        ui.label(muted(if view.unread { "Unread" } else { "Read" }));
        let summary = RichText::new(view.summary);
        let summary = if view.unread {
            summary.strong().color(palette().TEXT)
        } else {
            summary.color(palette().TEXT_MUTED)
        };
        let clicked = if view.clickable {
            ui.add(egui::Button::new(summary).frame(false).wrap())
                .clicked()
        } else {
            ui.label(summary);
            false
        };
        (clicked, trailing(ui))
    });
    let (summary, trailing) = inner.inner;
    NotificationRowClicks {
        summary,
        rect: inner.response.rect,
        trailing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::{
        Harness,
        kittest::{NodeT, Queryable},
    };

    fn row(unread: bool, clickable: bool) -> Harness<'static, Option<bool>> {
        Harness::new_ui_state(
            move |ui, clicked: &mut Option<bool>| {
                let clicks = notification_row(
                    ui,
                    NotificationRowView {
                        label: "Approval",
                        accent: palette().WARNING_FG,
                        unread,
                        summary: "Approval requested: shell",
                        clickable,
                    },
                    |_| (),
                );
                if clicks.summary {
                    *clicked = Some(true);
                }
            },
            None,
        )
    }

    // Given: an unread notification whose summary opens its thread
    // When: the summary is clicked
    // Then: the row says it is unread and reports the click
    #[test]
    fn a_clickable_row_reports_its_summary_click() {
        let mut harness = row(true, true);
        harness.get_by_label("Approval");
        harness.get_by_label("Unread");
        harness.get_by_label("Approval requested: shell").click();
        harness.run();
        assert_eq!(*harness.state(), Some(true));
    }

    // Given: a read notification that opens nothing
    // When: it is drawn
    // Then: it says it was read and offers no button
    #[test]
    fn a_plain_row_is_text_only() {
        let harness = row(false, false);
        harness.get_by_label("Read");
        let summary = harness.get_by_label("Approval requested: shell");
        assert_ne!(
            summary.accesskit_node().role(),
            egui::accesskit::Role::Button
        );
    }
}
