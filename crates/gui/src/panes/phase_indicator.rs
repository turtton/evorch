use egui::Ui;
use workspace_ui::ThreadRunPhase;

use crate::theme::tokens::{
    FONT_BADGE, FONT_SMALL, R_SM, SP_1, STATUS_STROKE, palette, phase_color, phase_label,
};
use crate::theme::widgets::badge;

pub fn phase_circle(ui: &mut Ui, phase: Option<ThreadRunPhase>) {
    let response = match phase {
        Some(ThreadRunPhase::Running) => {
            crate::theme::widgets::spinner_with(ui, FONT_SMALL, Some(palette().RUNNING))
        }
        Some(phase) => crate::theme::widgets::status_dot(ui, phase_color(phase)),
        None => crate::theme::widgets::status_dot(ui, palette().TEXT_MUTED),
    };
    response.on_hover_text(phase.map_or("Idle", phase_label));
}

pub fn phase_indicator(ui: &mut Ui, phase: ThreadRunPhase) {
    phase_indicator_with_ack(ui, phase, true);
}

pub fn phase_indicator_with_ack(ui: &mut Ui, phase: ThreadRunPhase, unread: bool) {
    let label = phase_label(phase);
    ui.horizontal(|ui| match phase {
        ThreadRunPhase::Running => phase_circle(ui, Some(phase)),
        ThreadRunPhase::Pending => {
            badge(ui, label, phase_color(phase), palette().SURFACE_RAISED);
        }
        ThreadRunPhase::Stopped
        | ThreadRunPhase::Waiting
        | ThreadRunPhase::Done
        | ThreadRunPhase::Error => {
            let accent = phase_color(phase);
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
                            .color(if unread { palette().CANVAS } else { accent }),
                    );
                });
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
                    if rect.fill == palette().SURFACE_RAISED
                        || PHASES.iter().any(|phase| rect.fill == phase_color(*phase)))
            })
            .count()
    }

    const PHASES: [ThreadRunPhase; 6] = [
        ThreadRunPhase::Pending,
        ThreadRunPhase::Running,
        ThreadRunPhase::Waiting,
        ThreadRunPhase::Done,
        ThreadRunPhase::Error,
        ThreadRunPhase::Stopped,
    ];

    #[test]
    fn phase_indicator_running_shows_spinner_without_badge() {
        // Given / When
        let harness = harness(Some(ThreadRunPhase::Running));
        // Then
        assert!(harness.query_by_label("Running").is_none());
        assert_eq!(spinner_count(&harness), 1);
        assert_eq!(pill_count(&harness), 0);
    }

    #[test]
    fn phase_indicator_waiting_shows_wait_badge_no_spinner() {
        // Given / When
        let harness = harness(Some(ThreadRunPhase::Waiting));
        // Then
        assert!(harness.query_by_label("Waiting for input").is_some());
        assert_eq!(spinner_count(&harness), 0);
        assert_eq!(pill_count(&harness), 1);
    }

    #[test]
    fn stopped_badge_is_resumable_and_warning_not_error() {
        let harness = harness(Some(ThreadRunPhase::Stopped));
        harness.get_by_label("Stopped (resumable)");
        assert_eq!(spinner_count(&harness), 0);
        assert_eq!(phase_color(ThreadRunPhase::Stopped), palette().WARNING_FG);
        assert_ne!(phase_color(ThreadRunPhase::Stopped), palette().ERROR_FG);
    }

    #[test]
    fn badges_use_the_shared_phase_label_and_color() {
        // Given: every phase that renders a badge rather than the spinner.
        for phase in PHASES.into_iter().filter(|p| *p != ThreadRunPhase::Running) {
            // When: the unread indicator renders.
            let harness = harness(Some(phase));
            // Then: its text and accent match phase_label / phase_color, so the
            // badge cannot drift from rows, tooltips and the sidebar dot.
            harness.get_by_label(phase_label(phase));
            let accent = if phase == ThreadRunPhase::Pending {
                palette().SURFACE_RAISED
            } else {
                phase_color(phase)
            };
            assert!(
                harness
                    .output()
                    .shapes
                    .iter()
                    .any(|shape| matches!(&shape.shape, Shape::Rect(rect) if rect.fill == accent)),
                "{phase:?} badge fill"
            );
        }
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
}
