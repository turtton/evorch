//! Thread goal controls belong to the composer, independently of worker Stop.
use event_bus::{ThreadGoalPhase, ThreadGoalSnapshot};

use crate::model::commands::WorkbenchCommand;
use crate::theme::widgets::{icon_button, labeled};
use crate::theme::{icons, tokens::*};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalAction {
    Review {
        thread_id: String,
        goal_id: String,
        enabled: bool,
    },
    ChecksPaused {
        thread_id: String,
        goal_id: String,
        paused: bool,
    },
}

impl GoalAction {
    pub fn into_command(self) -> WorkbenchCommand {
        match self {
            Self::Review {
                thread_id,
                goal_id,
                enabled,
            } => WorkbenchCommand::SetGoalReview {
                thread_id,
                goal_id,
                enabled,
            },
            Self::ChecksPaused {
                thread_id,
                goal_id,
                paused,
            } => WorkbenchCommand::SetGoalChecksPaused {
                thread_id,
                goal_id,
                paused,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Detail {
    Goal,
}

/// Tallest the expanded goal details grow before scrolling.
const DETAILS_MAX_HEIGHT: f32 = 180.0;

#[derive(Debug, Default)]
pub(super) struct BandOutput {
    pub action: Option<GoalAction>,
    /// A queued follow-up control pressed this frame.
    pub queue_action: Option<crate::panes::composer::ComposerAction>,
    /// Height the details viewport occupied this frame (0 when collapsed).
    pub details_viewport: f32,
    /// Height the details want, capped at [`DETAILS_MAX_HEIGHT`].
    pub details_wanted: f32,
}

/// Queued follow-ups, then goal controls, share one inset composer band. Expanded details scroll within
/// `details_max` so the composer input below keeps its room.
pub(super) fn progress_strip(
    ui: &mut egui::Ui,
    goal: Option<&ThreadGoalSnapshot>,
    queue: super::queue::QueueBand<'_>,
    details_max: f32,
) -> BandOutput {
    let mut output = BandOutput::default();
    let thread = goal.map(|goal| goal.thread_id.as_str()).unwrap_or_default();
    let expanded_id = ui.id().with(("progress-details", thread));
    if goal.is_none() && queue.items.is_empty() {
        ui.data_mut(|data| data.remove::<Detail>(expanded_id));
        return output;
    }
    let mut expanded = ui.data(|data| data.get_temp::<Detail>(expanded_id));
    if matches!(expanded, Some(Detail::Goal)) && goal.is_none() {
        expanded = None;
    }
    ui.horizontal(|ui| {
        ui.add_space(SP_3);
        egui::Frame::new()
            .fill(palette().SURFACE_RAISED)
            .corner_radius(egui::CornerRadius {
                nw: R_LG,
                ne: R_LG,
                sw: 0,
                se: 0,
            })
            .inner_margin(egui::Margin::symmetric(SP_2 as i8, SP_1 as i8))
            .show(ui, |ui| {
                ui.set_width((ui.available_width() - SP_3).max(80.0));
                ui.vertical(|ui| {
                    if !queue.items.is_empty() {
                        output.queue_action = super::queue::rows(ui, queue);
                        if goal.is_some() {
                            ui.separator();
                        }
                    }
                    if let Some(goal) = goal {
                        output.action = goal_row(ui, goal, &mut expanded);
                    }
                    if let Some(detail) = expanded {
                        ui.separator();
                        let height = details_max.clamp(ROW_DENSE, DETAILS_MAX_HEIGHT);
                        let details = egui::ScrollArea::vertical()
                            .id_salt(("progress-scroll", thread, detail))
                            .min_scrolled_height(height)
                            .max_height(height)
                            .show(ui, |ui| match detail {
                                Detail::Goal => goal_details(ui, goal.expect("visible goal")),
                            });
                        output.details_viewport = details.inner_rect.height();
                        output.details_wanted = details.content_size.y.min(DETAILS_MAX_HEIGHT);
                    }
                });
            });
    });
    ui.data_mut(|data| {
        if let Some(detail) = expanded {
            data.insert_temp(expanded_id, detail);
        } else {
            data.remove::<Detail>(expanded_id);
        }
    });
    output
}

fn details_button(ui: &mut egui::Ui, expanded: &mut Option<Detail>, detail: Detail, subject: &str) {
    let visible = *expanded == Some(detail);
    if icon_button(
        ui,
        if visible {
            icons::CARET_UP
        } else {
            icons::CARET_DOWN
        },
        &format!(
            "{} {subject} details",
            if visible { "Hide" } else { "Show" }
        ),
    )
    .clicked()
    {
        *expanded = if visible { None } else { Some(detail) };
    }
}

fn goal_row(
    ui: &mut egui::Ui,
    goal: &ThreadGoalSnapshot,
    expanded: &mut Option<Detail>,
) -> Option<GoalAction> {
    let mut action = None;
    ui.horizontal(|ui| {
        let state = match goal.phase {
            ThreadGoalPhase::Complete => {
                Some((icons::CHECK_CIRCLE, "Goal completed", palette().SUCCESS))
            }
            ThreadGoalPhase::Blocked => Some((
                icons::WARNING_CIRCLE,
                "Goal needs attention",
                palette().WARNING_FG,
            )),
            ThreadGoalPhase::Checking => Some((
                icons::MAGNIFYING_GLASS,
                "Checking completion",
                palette().TEXT_MUTED,
            )),
            ThreadGoalPhase::Reviewing => Some((
                icons::MAGNIFYING_GLASS,
                "Independent review in progress",
                palette().TEXT_MUTED,
            )),
            ThreadGoalPhase::Working | ThreadGoalPhase::Repairing => None,
        };
        if let Some((icon, label, color)) = state {
            let response = ui.label(egui::RichText::new(icon).color(color));
            response
                .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, label));
            response.on_hover_text(label);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            details_button(ui, expanded, Detail::Goal, "goal");
            ui.add_enabled_ui(goal.phase != ThreadGoalPhase::Complete, |ui| {
                let label = if goal.checks_paused {
                    "Resume goal checks"
                } else {
                    "Pause goal checks"
                };
                if icon_button(
                    ui,
                    if goal.checks_paused {
                        icons::PLAY
                    } else {
                        icons::PAUSE
                    },
                    label,
                )
                .on_hover_text(
                    "Controls completion checks and review. The working agent keeps running.",
                )
                .clicked()
                {
                    action = Some(GoalAction::ChecksPaused {
                        thread_id: goal.thread_id.clone(),
                        goal_id: goal.goal_id.clone(),
                        paused: !goal.checks_paused,
                    });
                }
                let label = if goal.review_enabled {
                    "Disable independent review before completion"
                } else {
                    "Enable independent review before completion"
                };
                let color = if goal.review_enabled {
                    palette().ACCENT_FG
                } else {
                    palette().TEXT
                };
                let button = egui::Button::new(
                    egui::RichText::new(icons::CLIPBOARD)
                        .size(FONT_ICON)
                        .color(color),
                )
                .selected(goal.review_enabled)
                .min_size(egui::vec2(ROW_DENSE - SP_1, ROW_DENSE - SP_1));
                let response = labeled(ui.add(button), label);
                ui.painter().text(
                    response.rect.center() + egui::vec2(0.0, 2.0),
                    egui::Align2::CENTER_CENTER,
                    icons::CHECK,
                    egui::FontId::proportional(10.0),
                    color,
                );
                if response.clicked() {
                    action = Some(GoalAction::Review {
                        thread_id: goal.thread_id.clone(),
                        goal_id: goal.goal_id.clone(),
                        enabled: !goal.review_enabled,
                    });
                }
            });
            let response = ui.add_sized(
                [ui.available_width().max(0.0), ROW_DENSE - SP_1],
                egui::Label::new(&goal.objective).truncate(),
            );
            response.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::WidgetType::Label,
                    true,
                    format!("Goal: {}", goal.objective),
                )
            });
            response.on_hover_text(&goal.objective);
        });
    });
    action
}

fn goal_details(ui: &mut egui::Ui, goal: &ThreadGoalSnapshot) {
    ui.label(&goal.objective);
    ui.label(format!("Original request: {}", goal.original_request));
    if goal.checks_paused {
        ui.label("Completion checks paused. Work may continue.");
    }
    if goal.work_stopped {
        ui.label("Work stopped. Continue the conversation to resume.");
    }
    if let Some(reason) = &goal.reason {
        ui.label(reason);
    }
    ui.label(format!(
        "Independent review: {} · round {}/{}",
        if goal.review_enabled {
            "enabled"
        } else {
            "disabled"
        },
        goal.review_round,
        goal.max_review_rounds
    ));
    for (index, criterion) in goal.criteria.iter().enumerate() {
        let check = goal.checks.iter().find(|check| check.criterion == index);
        let icon = if check.is_some_and(|check| check.met) {
            icons::CHECK_CIRCLE
        } else {
            icons::CIRCLE
        };
        ui.label(icons::with_icon(icon, criterion));
        if let Some(check) = check {
            ui.label(&check.evidence);
        }
    }
    if !goal.findings.is_empty() {
        ui.label("Review findings");
        for finding in &goal.findings {
            ui.label(finding);
        }
    }
    ui.label(format!(
        "Requests: {} · input {} · output {} tokens",
        goal.usage.model_requests, goal.usage.input_tokens, goal.usage.output_tokens
    ));
}
