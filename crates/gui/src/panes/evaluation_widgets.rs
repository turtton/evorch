//! Evaluation words and badges shared by the embedded workbench's Evaluations panel and the
//! core client's evaluation feature (ADR 0045).
//!
//! Pure over egui, the theme and protocol types: nothing here reads the evaluation files.
//! The embedded panel maps its own phases to the protocol's before it draws them.

use egui::{Color32, Response, Ui};
use workbench_protocol::namespaces::evaluation::{
    EvaluationGroupPhase, EvaluationMemberPhase, EvaluationProblemKind,
};

use crate::theme::text::muted;
use crate::theme::tokens::palette;
use crate::theme::widgets::badge;

/// What "unverified" means, wherever it is shown.
pub const UNVERIFIED_HINT: &str = "Nothing shows that the evaluation process still runs or how it ended: it was started by another session. Its files are shown as they are.";

#[must_use]
pub fn phase_color(running: bool, bad: bool, unverified: bool) -> Color32 {
    if running {
        palette().RUNNING
    } else if bad {
        palette().ERROR_FG
    } else if unverified {
        palette().WARNING_FG
    } else {
        palette().SUCCESS
    }
}

#[must_use]
pub const fn group_phase_label(phase: EvaluationGroupPhase) -> &'static str {
    match phase {
        EvaluationGroupPhase::Running => "Running",
        EvaluationGroupPhase::Finished => "Finished",
        EvaluationGroupPhase::Incomplete => "Incomplete",
        EvaluationGroupPhase::Unverified => "Unverified",
        EvaluationGroupPhase::Partial => "Partial",
    }
}

#[must_use]
pub const fn member_phase_label(phase: EvaluationMemberPhase) -> &'static str {
    match phase {
        EvaluationMemberPhase::Starting => "Starting",
        EvaluationMemberPhase::RunningTrials => "Running trials",
        EvaluationMemberPhase::Reporting => "Reporting",
        EvaluationMemberPhase::Finished => "Finished",
        EvaluationMemberPhase::FinishedWithoutReport => "Finished without a report",
        EvaluationMemberPhase::Failed => "Failed",
        EvaluationMemberPhase::Unverified => "Unverified",
    }
}

#[must_use]
pub const fn member_is_active(phase: EvaluationMemberPhase) -> bool {
    matches!(
        phase,
        EvaluationMemberPhase::Starting
            | EvaluationMemberPhase::RunningTrials
            | EvaluationMemberPhase::Reporting
    )
}

pub fn group_badge(ui: &mut Ui, phase: EvaluationGroupPhase) -> Response {
    let color = phase_color(
        phase == EvaluationGroupPhase::Running,
        phase == EvaluationGroupPhase::Incomplete,
        matches!(
            phase,
            EvaluationGroupPhase::Unverified | EvaluationGroupPhase::Partial
        ),
    );
    badge(ui, group_phase_label(phase), palette().CANVAS, color)
}

pub fn member_badge(ui: &mut Ui, phase: EvaluationMemberPhase) -> Response {
    let color = phase_color(
        member_is_active(phase),
        matches!(
            phase,
            EvaluationMemberPhase::Failed | EvaluationMemberPhase::FinishedWithoutReport
        ),
        phase == EvaluationMemberPhase::Unverified,
    );
    let response = badge(ui, member_phase_label(phase), palette().CANVAS, color);
    if phase == EvaluationMemberPhase::Unverified {
        response.on_hover_text(UNVERIFIED_HINT)
    } else {
        response
    }
}

#[must_use]
pub const fn problem_word(kind: EvaluationProblemKind) -> &'static str {
    match kind {
        EvaluationProblemKind::Malformed => "malformed",
        EvaluationProblemKind::UnsupportedSchema => "unsupported_schema",
        EvaluationProblemKind::TooLarge => "too_large",
        EvaluationProblemKind::PermissionDenied => "permission_denied",
        EvaluationProblemKind::UnsafePath => "unsafe_path",
        EvaluationProblemKind::Io => "io",
    }
}

pub fn problem_line(ui: &mut Ui, problem: &str) -> Response {
    ui.colored_label(palette().WARNING_FG, format!("Artifact problem: {problem}"))
}

pub fn crumb(ui: &mut Ui) -> Response {
    ui.label(muted("›"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::{Harness, kittest::Queryable};

    const GROUPS: [EvaluationGroupPhase; 5] = [
        EvaluationGroupPhase::Running,
        EvaluationGroupPhase::Finished,
        EvaluationGroupPhase::Incomplete,
        EvaluationGroupPhase::Unverified,
        EvaluationGroupPhase::Partial,
    ];
    const MEMBERS: [EvaluationMemberPhase; 7] = [
        EvaluationMemberPhase::Starting,
        EvaluationMemberPhase::RunningTrials,
        EvaluationMemberPhase::Reporting,
        EvaluationMemberPhase::Finished,
        EvaluationMemberPhase::FinishedWithoutReport,
        EvaluationMemberPhase::Failed,
        EvaluationMemberPhase::Unverified,
    ];

    #[test]
    fn every_phase_is_a_badge_named_by_its_word() {
        let mut harness = Harness::new_ui(|ui| {
            for phase in GROUPS {
                group_badge(ui, phase);
            }
            for phase in MEMBERS {
                member_badge(ui, phase);
            }
            problem_line(ui, problem_word(EvaluationProblemKind::TooLarge));
        });
        harness.run();
        for word in [
            "Running",
            "Incomplete",
            "Partial",
            "Starting",
            "Running trials",
            "Reporting",
            "Finished without a report",
            "Failed",
        ] {
            harness.get_by_label(word);
        }
        assert_eq!(harness.query_all_by_label("Finished").count(), 2);
        assert_eq!(harness.query_all_by_label("Unverified").count(), 2);
        harness.get_by_label("Artifact problem: too_large");
    }

    #[test]
    fn an_unverified_member_explains_itself_on_hover() {
        let mut harness = Harness::new_ui(|ui| {
            member_badge(ui, EvaluationMemberPhase::Unverified);
        });
        harness.run();
        harness.get_by_label("Unverified").hover();
        harness.run();
        harness.get_by_label(UNVERIFIED_HINT);
    }
}
