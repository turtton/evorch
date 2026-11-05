//! The pull request bar (ADR 0043): one line above the composer for the thread's linked pull
//! request, expanding upward into its checks and facts.
//!
//! The bar belongs to the thread, not to a run, so it is drawn in a subagent's transcript too.
//! It shows what the database holds and offers two operations: Refresh and Unlink. A state that
//! cannot be established is never drawn as success.

use std::time::Duration;

use egui::{Align, Layout, RichText};
use storage::pull_requests::{CheckConclusion, CheckStatus, PullRequestState, ReviewDecision};

use crate::format;
use crate::model::pull_requests::{
    PrGlyph, PullRequestAction, Status, ThreadPullRequest, check_is_failed, ci_summary, glyph,
    mismatches, state_word, statuses,
};
use crate::theme::icons;
use crate::theme::text::{medium, muted};
use crate::theme::tokens::*;
use crate::theme::widgets::ghost;

/// What the bar draws.
pub struct PullRequestBarContext<'a> {
    pub view: &'a ThreadPullRequest,
    pub expanded: bool,
    /// Wall clock, nanoseconds since the Unix epoch.
    pub now_ns: i64,
}

/// The share of the conversation area the bar may take, expanded.
pub const MAX_HEIGHT_SHARE: f32 = 0.4;

/// The tallest the bar may be for a conversation area of `conversation_height`.
#[must_use]
pub fn max_bar_height(conversation_height: f32) -> f32 {
    conversation_height * MAX_HEIGHT_SHARE
}

pub const REFRESH_LABEL: &str = "Refresh";
pub const UNLINK_LABEL: &str = "Unlink";

/// The colour, icon and word of a glyph class. Colour is never the only signal.
#[must_use]
pub fn glyph_style(glyph: PrGlyph) -> (&'static str, egui::Color32) {
    match glyph {
        PrGlyph::Passing => (icons::CHECK, palette().SUCCESS),
        PrGlyph::Failing => (icons::X, palette().ERROR_FG),
        PrGlyph::Pending => (icons::CIRCLE, palette().RUNNING),
        PrGlyph::Merged => (icons::GIT_MERGE, palette().MERGED_FG),
        PrGlyph::Closed => (icons::PROHIBIT, palette().TEXT_MUTED),
        PrGlyph::Unknown => (icons::QUESTION, palette().TEXT_MUTED),
    }
}

fn age(now_ns: i64, at_ns: i64) -> String {
    let nanos = u64::try_from(now_ns.saturating_sub(at_ns)).unwrap_or(0);
    format::time_ago(Duration::from_nanos(nanos))
}

/// Draws the bar as a bottom panel. `conversation_height` is the height of the area the bar
/// shares with the transcript; the bar never grows past 40% of it.
pub(super) fn show(
    ui: &mut egui::Ui,
    bar: &PullRequestBarContext<'_>,
    conversation_height: f32,
    action: &mut Option<super::AgentPaneAction>,
) {
    let view = bar.view;
    let thread_id = view.thread_id.clone();
    let mut asked = None;
    let frame = egui::Frame::new()
        .fill(palette().SURFACE)
        .stroke(egui::Stroke::new(1.0, palette().BORDER))
        .inner_margin(egui::Margin::symmetric(SP_3 as i8, SP_1 as i8));
    egui::Panel::bottom(ui.id().with("pull-request-bar"))
        .resizable(false)
        .show_separator_line(false)
        .frame(frame)
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = SP_1;
            let fixed = ui.spacing().interact_size.y + SP_1 * 4.0 + 2.0;
            if bar.expanded {
                let room = (max_bar_height(conversation_height) - fixed).max(ROW_DENSE);
                egui::ScrollArea::vertical()
                    .id_salt("pull-request-details")
                    .max_height(room)
                    .auto_shrink([false, true])
                    .show(ui, |ui| details(ui, bar));
                ui.separator();
            }
            summary_row(ui, bar, &mut asked);
        });
    match asked {
        Some(PullRequestAction::Refresh { .. }) => {
            *action = Some(super::AgentPaneAction::PullRequest(
                PullRequestAction::Refresh { thread_id },
            ));
        }
        Some(PullRequestAction::Unlink { .. }) => {
            *action = Some(super::AgentPaneAction::PullRequest(
                PullRequestAction::Unlink { thread_id },
            ));
        }
        Some(PullRequestAction::ToggleExpanded { .. }) => {
            *action = Some(super::AgentPaneAction::PullRequest(
                PullRequestAction::ToggleExpanded { thread_id },
            ));
        }
        _ => {}
    }
}

/// One line of status: its words, colour and, when there is more to read, the hover.
struct StatusLine {
    text: String,
    color: egui::Color32,
    hover: Option<String>,
}

/// Every status that holds, each on its own (never one hiding another), then when the facts
/// were last fetched.
fn status_lines(view: &ThreadPullRequest, now_ns: i64) -> Vec<StatusLine> {
    let mut lines: Vec<StatusLine> = statuses(view, now_ns)
        .into_iter()
        .map(|status| match status {
            Status::NotTracked => StatusLine {
                text: "Not tracked in this window".to_owned(),
                color: palette().TEXT_MUTED,
                hover: Some("Another window polls this pull request, or none does".into()),
            },
            Status::Stale => StatusLine {
                text: "Stale".to_owned(),
                color: palette().WARNING_FG,
                hover: Some("The facts are older than the polling interval allows".into()),
            },
            Status::FetchFailed { kind, message } => StatusLine {
                text: format!("Fetch failed ({kind})"),
                color: palette().WARNING_FG,
                hover: Some(message),
            },
        })
        .collect();
    if !lines.is_empty() {
        lines.push(StatusLine {
            text: view.fetched_at_ns.map_or_else(
                || "never fetched".to_owned(),
                |at| format!("last fetched {}", age(now_ns, at)),
            ),
            color: palette().TEXT_MUTED,
            hover: None,
        });
    }
    lines
}

fn summary_row(
    ui: &mut egui::Ui,
    bar: &PullRequestBarContext<'_>,
    asked: &mut Option<PullRequestAction>,
) {
    let view = bar.view;
    let thread_id = view.thread_id.clone();
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = SP_2;
        let unlink = ui.add(ghost(RichText::new(icons::with_icon(
            icons::LINK_BREAK,
            UNLINK_LABEL,
        ))));
        unlink.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, true, UNLINK_LABEL)
        });
        if unlink
            .on_hover_text("Stop tracking this pull request for the thread")
            .clicked()
        {
            *asked = Some(PullRequestAction::Unlink {
                thread_id: thread_id.clone(),
            });
        }
        let refresh = ui.add(ghost(RichText::new(icons::with_icon(
            icons::ARROWS_CLOCKWISE,
            REFRESH_LABEL,
        ))));
        refresh.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, true, REFRESH_LABEL)
        });
        if refresh
            .on_hover_text("Fetch the pull request now")
            .clicked()
        {
            *asked = Some(PullRequestAction::Refresh {
                thread_id: thread_id.clone(),
            });
        }
        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = SP_2;
            let caret = if bar.expanded {
                icons::CARET_DOWN
            } else {
                icons::CARET_UP
            };
            let toggle = ui.add(ghost(RichText::new(caret)));
            toggle.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::WidgetType::Button,
                    true,
                    if bar.expanded {
                        "Collapse pull request"
                    } else {
                        "Expand pull request"
                    },
                )
            });
            if toggle.clicked() {
                *asked = Some(PullRequestAction::ToggleExpanded {
                    thread_id: thread_id.clone(),
                });
            }
            let class = glyph(view).unwrap_or(PrGlyph::Unknown);
            let (icon, color) = glyph_style(class);
            if let Some(number) = view.number() {
                ui.label(medium(format!("#{number}")));
            }
            ui.label(RichText::new(icon).color(color));
            let state = view
                .facts
                .as_ref()
                .map_or("Unknown", |facts| state_word(facts.state));
            ui.label(RichText::new(state).color(color));
            let ci = ci_summary(view);
            if !ci.is_empty() {
                ui.label(RichText::new(ci).color(color));
            }
            for line in status_lines(view, bar.now_ns) {
                let label = ui.label(RichText::new(line.text).size(FONT_SMALL).color(line.color));
                if let Some(hover) = line.hover {
                    label.on_hover_text(hover);
                }
            }
            if let Some(facts) = &view.facts {
                ui.add(egui::Label::new(muted(facts.head_branch.as_str())).truncate());
            }
        });
    });
}

fn details(ui: &mut egui::Ui, bar: &PullRequestBarContext<'_>) {
    let view = bar.view;
    ui.spacing_mut().item_spacing.y = SP_1;
    let Some(facts) = &view.facts else {
        ui.label(muted(match &view.failure {
            Some(failure) => format!("Not fetched yet: {}", failure.message),
            None => "Waiting for the first fetch".to_owned(),
        }));
        return;
    };
    if let Some(label) = view.label() {
        ui.label(medium(label));
    }
    checks(ui, facts, view.failure.is_none());
    ui.add_space(SP_1);
    egui::Grid::new("pull-request-facts")
        .num_columns(2)
        .spacing([SP_3, SP_1])
        .show(ui, |ui| {
            let row = |ui: &mut egui::Ui, name: &str, value: RichText| {
                ui.label(muted(name));
                ui.label(value);
                ui.end_row();
            };
            row(ui, "State", RichText::new(state_word(facts.state)));
            row(
                ui,
                "Branch",
                RichText::new(format!("{} ← {}", facts.base_branch, facts.head_branch)),
            );
            row(
                ui,
                "Head",
                RichText::new(&facts.head_sha).monospace().size(FONT_SMALL),
            );
            if let Some(local) = &view.local_head {
                row(
                    ui,
                    "Local HEAD",
                    RichText::new(&local.head_sha).monospace().size(FONT_SMALL),
                );
            }
            row(
                ui,
                "Review",
                RichText::new(review_word(facts.review_decision)),
            );
            if let Some(at) = view.fetched_at_ns {
                ui.label(muted("Fetched"));
                ui.label(age(bar.now_ns, at))
                    .on_hover_text(format::utc_timestamp(
                        chrono::DateTime::from_timestamp_nanos(at),
                    ));
                ui.end_row();
            }
        });
    for line in status_lines(view, bar.now_ns) {
        ui.label(RichText::new(line.text).color(line.color));
    }
    for mismatch in mismatches(view) {
        ui.label(
            RichText::new(icons::with_icon(icons::WARNING, mismatch.text()))
                .color(palette().WARNING_FG),
        );
    }
}

fn review_word(decision: ReviewDecision) -> &'static str {
    match decision {
        ReviewDecision::Approved => "Approved",
        ReviewDecision::ChangesRequested => "Changes requested",
        ReviewDecision::ReviewRequired => "Review required",
        ReviewDecision::None => "None",
    }
}

fn conclusion_word(conclusion: Option<CheckConclusion>) -> &'static str {
    match conclusion {
        Some(CheckConclusion::Success) => "success",
        Some(CheckConclusion::Failure) => "failure",
        Some(CheckConclusion::Neutral) => "neutral",
        Some(CheckConclusion::Cancelled) => "cancelled",
        Some(CheckConclusion::Skipped) => "skipped",
        Some(CheckConclusion::TimedOut) => "timed out",
        Some(CheckConclusion::ActionRequired) => "action required",
        Some(CheckConclusion::Stale) => "stale",
        Some(CheckConclusion::StartupFailure) => "startup failure",
        Some(CheckConclusion::Other) | None => "unfinished",
    }
}

/// Failures one by one, the checks still running one by one, and everything that passed on one
/// line.
fn checks(ui: &mut egui::Ui, facts: &storage::pull_requests::PullRequestFacts, fetch_worked: bool) {
    if facts.state != PullRequestState::Open && facts.checks.is_empty() {
        return;
    }
    let mut passed = 0;
    for check in &facts.checks {
        if check_is_failed(check) {
            ui.label(
                RichText::new(icons::with_icon(
                    icons::X,
                    format!("{} · {}", check.name, conclusion_word(check.conclusion)),
                ))
                .color(palette().ERROR_FG),
            );
        } else if check.status != CheckStatus::Completed {
            ui.label(
                RichText::new(icons::with_icon(
                    icons::CIRCLE,
                    format!("{} · running", check.name),
                ))
                .color(palette().RUNNING),
            );
        } else if matches!(
            check.conclusion,
            Some(CheckConclusion::Success | CheckConclusion::Neutral | CheckConclusion::Skipped)
        ) {
            passed += 1;
        } else {
            ui.label(
                RichText::new(icons::with_icon(
                    icons::QUESTION,
                    format!("{} · {}", check.name, conclusion_word(check.conclusion)),
                ))
                .color(palette().TEXT_MUTED),
            );
        }
    }
    if passed > 0 {
        // A pass is only drawn as success while the list is the whole list and the newest fetch
        // worked; otherwise it is a count, in neutral.
        let (icon, color) = if fetch_worked && facts.checks_complete {
            (icons::CHECK, palette().SUCCESS)
        } else {
            (icons::MINUS, palette().TEXT_MUTED)
        };
        ui.label(RichText::new(icons::with_icon(icon, format!("{passed} passed"))).color(color));
    }
    if !facts.checks_complete {
        ui.label(muted("The list of checks is incomplete"));
    }
}
