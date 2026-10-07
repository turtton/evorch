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

pub(super) fn goal_strip(ui: &mut egui::Ui, goal: &ThreadGoalSnapshot) -> Option<GoalAction> {
    let mut action = None;
    let expanded_id = ui.id().with(("goal-details", &goal.goal_id));
    let mut expanded = ui
        .data(|data| data.get_temp::<bool>(expanded_id))
        .unwrap_or(false);
    // The inset, upper rounded tier joins the full-width composer below it.
    ui.horizontal(|ui| {
        ui.add_space(SP_3);
        egui::Frame::new()
            .fill(palette().SURFACE_RAISED)
            .corner_radius(egui::CornerRadius { nw: R_LG, ne: R_LG, sw: 0, se: 0 })
            .inner_margin(egui::Margin::symmetric(SP_2 as i8, SP_1 as i8))
            .show(ui, |ui| {
                ui.vertical(|ui| {
                ui.set_width((ui.available_width() - SP_3).max(80.0));
                ui.horizontal(|ui| {
                    let state = match goal.phase {
                        ThreadGoalPhase::Complete => Some((icons::CHECK_CIRCLE, "Goal completed", palette().SUCCESS)),
                        ThreadGoalPhase::Blocked => Some((icons::WARNING_CIRCLE, "Goal needs attention", palette().WARNING_FG)),
                        ThreadGoalPhase::Checking => Some((icons::MAGNIFYING_GLASS, "Checking completion", palette().TEXT_MUTED)),
                        ThreadGoalPhase::Reviewing => Some((icons::MAGNIFYING_GLASS, "Independent review in progress", palette().TEXT_MUTED)),
                        ThreadGoalPhase::Working | ThreadGoalPhase::Repairing => None,
                    };
                    if let Some((icon, label, color)) = state {
                        let response = ui.label(egui::RichText::new(icon).color(color));
                        response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, label));
                        response.on_hover_text(label);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if icon_button(ui, if expanded { icons::CARET_UP } else { icons::CARET_DOWN }, if expanded { "Hide goal details" } else { "Show goal details" }).clicked() {
                            expanded = !expanded;
                        }
                        let complete = goal.phase == ThreadGoalPhase::Complete;
                        ui.add_enabled_ui(!complete, |ui| {
                            let label = if goal.checks_paused { "Resume goal checks" } else { "Pause goal checks" };
                            let response = icon_button(ui, if goal.checks_paused { icons::PLAY } else { icons::PAUSE }, label)
                                .on_hover_text("Controls completion checks and review. The working agent keeps running.");
                            if response.clicked() {
                                action = Some(GoalAction::ChecksPaused { thread_id: goal.thread_id.clone(), goal_id: goal.goal_id.clone(), paused: !goal.checks_paused });
                            }
                            let label = if goal.review_enabled { "Disable independent review before completion" } else { "Enable independent review before completion" };
                            let icon_color = if goal.review_enabled { palette().ACCENT_FG } else { palette().TEXT };
                            let button = egui::Button::new(egui::RichText::new(icons::CLIPBOARD).size(FONT_ICON).color(icon_color))
                                .selected(goal.review_enabled)
                                .min_size(egui::vec2(ROW_DENSE - SP_1, ROW_DENSE - SP_1));
                            let response = labeled(ui.add(button), label);
                            // Phosphor has no clipboard-check glyph. Overlay a small
                            // check on the empty clipboard without implying a verdict.
                            ui.painter().text(response.rect.center() + egui::vec2(0.0, 2.0), egui::Align2::CENTER_CENTER, icons::CHECK, egui::FontId::proportional(10.0), icon_color);
                            if response.clicked() {
                                action = Some(GoalAction::Review { thread_id: goal.thread_id.clone(), goal_id: goal.goal_id.clone(), enabled: !goal.review_enabled });
                            }
                        });
                        let width = ui.available_width().max(0.0);
                        let response = ui.add_sized([width, ROW_DENSE - SP_1], egui::Label::new(&goal.objective).truncate());
                        response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, format!("Goal: {}", goal.objective)));
                        response.on_hover_text(&goal.objective);
                    });
                });
                if expanded {
                    ui.separator();
                    egui::ScrollArea::vertical().min_scrolled_height(180.0).max_height(180.0).show(ui, |ui| {
                        ui.label(&goal.objective);
                        ui.label(format!("Original request: {}", goal.original_request));
                        if goal.checks_paused { ui.label("Completion checks paused. Work may continue."); }
                        if goal.work_stopped { ui.label("Work stopped. Continue the conversation to resume."); }
                        if let Some(reason) = &goal.reason { ui.label(reason); }
                        ui.label(format!("Independent review: {} · round {}/{}", if goal.review_enabled { "enabled" } else { "disabled" }, goal.review_round, goal.max_review_rounds));
                        for (index, criterion) in goal.criteria.iter().enumerate() {
                            let check = goal.checks.iter().find(|check| check.criterion == index);
                            let icon = if check.is_some_and(|check| check.met) { icons::CHECK_CIRCLE } else { icons::CIRCLE };
                            ui.label(icons::with_icon(icon, criterion));
                            if let Some(check) = check { ui.label(&check.evidence); }
                        }
                        if !goal.findings.is_empty() {
                            ui.label("Review findings");
                            for finding in &goal.findings { ui.label(finding); }
                        }
                        ui.label(format!("Requests: {} · input {} · output {} tokens", goal.usage.model_requests, goal.usage.input_tokens, goal.usage.output_tokens));
                    });
                }
                });
            });
    });
    ui.data_mut(|data| data.insert_temp(expanded_id, expanded));
    action
}
