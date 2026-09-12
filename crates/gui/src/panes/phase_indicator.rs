use egui::Ui;
use workspace_ui::ThreadRunPhase;

use crate::theme::tokens::{
    CANVAS, FONT_BADGE, FONT_SMALL, INFO, R_SM, RUNNING, SP_1, STATUS_STROKE, SURFACE_RAISED,
    phase_color,
};
use crate::theme::widgets::badge;

pub fn phase_indicator(ui: &mut Ui, phase: ThreadRunPhase) {
    phase_indicator_with_ack(ui, phase, true);
}

pub fn phase_indicator_with_ack(ui: &mut Ui, phase: ThreadRunPhase, unread: bool) {
    let label = match phase {
        ThreadRunPhase::Running => "running",
        ThreadRunPhase::Waiting => "waiting (input)",
        ThreadRunPhase::Pending => "pending",
        ThreadRunPhase::Done => "done",
        ThreadRunPhase::Error => "error",
    };
    ui.horizontal(|ui| {
        if phase == ThreadRunPhase::Running {
            ui.add(egui::Spinner::new().size(FONT_SMALL).color(RUNNING));
        }
        match phase {
            ThreadRunPhase::Pending | ThreadRunPhase::Running => {
                badge(ui, label, phase_color(phase), SURFACE_RAISED);
            }
            ThreadRunPhase::Waiting | ThreadRunPhase::Done | ThreadRunPhase::Error => {
                let accent = match phase {
                    ThreadRunPhase::Error | ThreadRunPhase::Pending | ThreadRunPhase::Running => {
                        phase_color(phase)
                    }
                    ThreadRunPhase::Waiting | ThreadRunPhase::Done => INFO,
                };
                egui::Frame::new()
                    .fill(if unread {
                        accent
                    } else {
                        egui::Color32::TRANSPARENT
                    })
                    .stroke(egui::Stroke::new(STATUS_STROKE, accent))
                    .corner_radius(egui::CornerRadius::same(R_SM))
                    .inner_margin(egui::Margin::symmetric(SP_1 as i8, 0))
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new(label)
                                .size(FONT_BADGE)
                                .color(if unread { CANVAS } else { accent }),
                        );
                    });
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use egui::epaint::Shape;
    use egui_kittest::{Harness, kittest::Queryable};

    use super::*;

    fn harness(phase: Option<ThreadRunPhase>) -> Harness<'static> {
        let mut harness = Harness::new_ui(move |ui| {
            crate::theme::install(ui.ctx());
            ui.horizontal(|ui| {
                if let Some(phase) = phase {
                    phase_indicator(ui, phase);
                }
            });
        });
        // Animated widgets never settle; advance a bounded number of frames.
        harness.run_steps(2);
        harness
    }

    fn spinner_count(harness: &Harness<'_>) -> usize {
        harness
            .output()
            .shapes
            .iter()
            .filter(|shape| matches!(&shape.shape, Shape::Path(path) if !path.closed))
            .count()
    }

    fn pill_count(harness: &Harness<'_>) -> usize {
        harness
            .output()
            .shapes
            .iter()
            .filter(|shape| {
                matches!(&shape.shape, Shape::Rect(rect)
                if rect.fill == crate::theme::tokens::SURFACE_RAISED || rect.fill == INFO)
            })
            .count()
    }

    #[test]
    fn phase_indicator_running_shows_spinner_and_badge() {
        // Given / When
        let harness = harness(Some(ThreadRunPhase::Running));
        // Then
        assert!(harness.query_by_label("running").is_some());
        assert_eq!(spinner_count(&harness), 1);
        assert_eq!(pill_count(&harness), 1);
    }

    #[test]
    fn phase_indicator_waiting_shows_wait_badge_no_spinner() {
        // Given / When
        let harness = harness(Some(ThreadRunPhase::Waiting));
        // Then
        assert!(harness.query_by_label("waiting (input)").is_some());
        assert_eq!(spinner_count(&harness), 0);
        assert_eq!(pill_count(&harness), 1);
    }

    #[test]
    fn phase_indicator_none_shows_nothing() {
        // Given / When
        let harness = harness(None);
        // Then
        assert_eq!(spinner_count(&harness), 0);
        assert_eq!(pill_count(&harness), 0);
        assert!(
            !harness
                .output()
                .shapes
                .iter()
                .any(|shape| matches!(shape.shape, Shape::Text(_)))
        );
    }

    #[test]
    fn phase_indicator_done_shows_done_badge() {
        // Given / When
        let harness = harness(Some(ThreadRunPhase::Done));
        // Then
        assert!(harness.query_by_label("done").is_some());
        assert_eq!(spinner_count(&harness), 0);
        assert_eq!(pill_count(&harness), 1);
    }
}
