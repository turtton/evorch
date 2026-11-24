//! Self-improvement words and widgets shared by the embedded workbench's pane and the core
//! client's improvement feature (ADR 0045): the status mark, the badges of a candidate and
//! the runs behind its evidence.
//!
//! Pure over egui, the theme and protocol types: nothing here reads storage, the runtime or
//! the event bus. Each side maps what it read to these words and draws them the same way.

use std::time::SystemTime;

use egui::{Response, RichText, Sense, Ui, WidgetInfo, WidgetType};
use workbench_protocol::{
    management::ImprovementStatus,
    namespaces::improvement::{ImprovementSeverity, ImprovementSource},
};

use crate::theme::icons;
use crate::theme::text::{badge, muted};
use crate::theme::tokens::{FONT_ICON, palette};
use crate::theme::widgets::copy_id_button;

pub const DISABLED: &str = "Self-improvement drafts are disabled ([self_improvement] enabled=false). Candidates are not collected.";
pub const NO_PROJECT: &str = "Select a project to browse improvement candidates.";
pub const STATUS_LABEL: &str = "Status";
pub const STATUS_ACCESS: &str = "Candidate status";
pub const ALL: &str = "All";
pub const EMPTY: &str = "No improvement candidates yet.";
pub const MARK_REVIEWED: &str = "Mark reviewed";
pub const DISMISS: &str = "Dismiss";
pub const EVIDENCE: &str = "Evidence";
pub const COPY_RUN_ID: &str = "Copy run ID";

#[must_use]
pub const fn status_word(status: ImprovementStatus) -> &'static str {
    match status {
        ImprovementStatus::New => "new",
        ImprovementStatus::Reviewed => "reviewed",
        ImprovementStatus::Dismissed => "dismissed",
    }
}

#[must_use]
pub const fn source_word(source: ImprovementSource) -> &'static str {
    match source {
        ImprovementSource::Diagnostic => "diagnostic",
        ImprovementSource::Lesson => "lesson",
    }
}

#[must_use]
pub const fn severity_word(severity: ImprovementSeverity) -> &'static str {
    match severity {
        ImprovementSeverity::Info => "info",
        ImprovementSeverity::Warning => "warning",
        ImprovementSeverity::Error => "error",
    }
}

pub fn status_mark(ui: &mut Ui, status: ImprovementStatus) -> Response {
    let (icon, color) = match status {
        ImprovementStatus::New => (icons::SPARKLE, palette().ACCENT),
        ImprovementStatus::Reviewed => (icons::CHECK_CIRCLE, palette().SUCCESS),
        ImprovementStatus::Dismissed => (icons::PROHIBIT, palette().TEXT_MUTED),
    };
    let word = status_word(status);
    let response = ui.add(
        egui::Label::new(RichText::new(icon).size(FONT_ICON).color(color)).sense(Sense::hover()),
    );
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, word));
    response.on_hover_text(format!("Status: {word}"))
}

pub fn reopened_badge(ui: &mut Ui, count: u32) -> Response {
    ui.label(
        badge(format!("Reopened ×{count}"))
            .color(palette().WARNING_FG)
            .background_color(palette().WARNING_SURFACE),
    )
}

pub fn kind_badges(ui: &mut Ui, source: &str, severity: &str, code: &str) {
    for word in [source, severity, code] {
        ui.label(badge(word));
    }
}

#[must_use]
pub fn seen_line(occurrences: u32, when: &str) -> String {
    format!("Seen {occurrences} times, last {when}")
}

/// Relative age for the label and absolute UTC time for its tooltip.
#[must_use]
pub fn when(at: SystemTime, now: SystemTime) -> (String, String) {
    (
        crate::format::time_since(at, now),
        crate::format::utc_timestamp(at.into()),
    )
}

pub fn evidence_runs<S: AsRef<str>>(ui: &mut Ui, run: Option<&str>, recent: &[S]) {
    if let Some(run) = run {
        ui.horizontal(|ui| {
            ui.label(muted("Run"));
            copy_id_button(ui, COPY_RUN_ID, run);
        });
    }
    if recent.len() > 1 {
        let recent = recent.iter().map(AsRef::as_ref).collect::<Vec<_>>();
        ui.monospace(format!("Recent runs: {}", recent.join(", ")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::{Harness, kittest::Queryable};
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn a_status_mark_is_named_by_its_word_and_says_it_on_hover() {
        let mut harness = Harness::new_ui(|ui| {
            status_mark(ui, ImprovementStatus::New);
            status_mark(ui, ImprovementStatus::Dismissed);
        });
        harness.run();
        harness.get_by_label("dismissed");
        harness.get_by_label("new").hover();
        harness.run();
        harness.get_by_label("Status: new");
    }

    #[test]
    fn when_is_relative_with_an_absolute_utc_time() {
        let at = UNIX_EPOCH + Duration::from_secs(951_827_696);
        let now = at + Duration::from_secs(3_742);
        assert_eq!(
            when(at, now),
            ("1h 2m ago".into(), "2000-02-29 12:34 UTC".into())
        );
        assert_eq!(seen_line(3, "1h 2m ago"), "Seen 3 times, last 1h 2m ago");
    }

    #[test]
    fn the_runs_offer_a_copy_and_list_the_recent_ones_only_when_there_are_several() {
        let mut harness = Harness::new_ui(|ui| {
            evidence_runs(ui, Some("run-1"), &["run-1"]);
            evidence_runs::<&str>(ui, None, &["run-2", "run-3"]);
        });
        harness.run();
        assert_eq!(harness.query_all_by_label(COPY_RUN_ID).count(), 1);
        harness.get_by_label("Recent runs: run-2, run-3");
        assert!(
            harness
                .query_by_label_contains("Recent runs: run-1")
                .is_none()
        );
    }
}
