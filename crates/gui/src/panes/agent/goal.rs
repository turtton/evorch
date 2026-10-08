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
    Procedures,
}

/// Goal controls and optional procedure tracking share one inset composer band.
pub(super) fn progress_strip(
    ui: &mut egui::Ui,
    goal: Option<&ThreadGoalSnapshot>,
    todo: Option<&event_bus::ThreadTodoSnapshot>,
) -> Option<GoalAction> {
    let thread = goal
        .map(|goal| goal.thread_id.as_str())
        .or_else(|| todo.map(|todo| todo.thread_id.as_str()))?;
    let expanded_id = ui.id().with(("progress-details", thread));
    let todo = todo.filter(|todo| !todo.items.is_empty());
    if goal.is_none() && todo.is_none() {
        ui.data_mut(|data| data.remove::<Detail>(expanded_id));
        return None;
    }
    let mut expanded = ui.data(|data| data.get_temp::<Detail>(expanded_id));
    if matches!(expanded, Some(Detail::Goal)) && goal.is_none()
        || matches!(expanded, Some(Detail::Procedures)) && todo.is_none()
    {
        expanded = None;
    }
    let mut action = None;
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
                    if let Some(goal) = goal {
                        action = goal_row(ui, goal, &mut expanded);
                    }
                    if let Some(todo) = todo {
                        procedure_row(ui, todo, &mut expanded);
                    }
                    if let Some(detail) = expanded {
                        ui.separator();
                        egui::ScrollArea::vertical()
                            .id_salt(("progress-scroll", thread, detail))
                            .min_scrolled_height(180.0)
                            .max_height(180.0)
                            .show(ui, |ui| match detail {
                                Detail::Goal => goal_details(ui, goal.expect("visible goal")),
                                Detail::Procedures => {
                                    procedure_details(ui, todo.expect("visible procedures"))
                                }
                            });
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
    action
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

fn procedure_row(
    ui: &mut egui::Ui,
    todo: &event_bus::ThreadTodoSnapshot,
    expanded: &mut Option<Detail>,
) {
    use event_bus::ThreadTodoStatus;
    let completed = todo
        .items
        .iter()
        .filter(|item| item.status == ThreadTodoStatus::Completed)
        .count();
    let active = todo
        .items
        .iter()
        .filter(|item| item.status == ThreadTodoStatus::InProgress)
        .count();
    let current = todo
        .items
        .iter()
        .find(|item| item.status == ThreadTodoStatus::InProgress)
        .or_else(|| {
            todo.items
                .iter()
                .find(|item| item.status == ThreadTodoStatus::Pending)
        });
    let text = current
        .map(|item| item.content.as_str())
        .unwrap_or("All procedures completed");
    let accessible_text = if active > 1 {
        format!("{text} · +{} in progress", active - 1)
    } else {
        text.to_string()
    };
    ui.horizontal(|ui| {
        let (icon, status, color) = if completed == todo.items.len() {
            (
                icons::CHECK_CIRCLE,
                "Procedures completed",
                palette().SUCCESS,
            )
        } else if active > 0 {
            (
                icons::CIRCLE,
                "Procedures in progress (stored status)",
                palette().ACCENT_FG,
            )
        } else {
            (icons::CIRCLE, "Procedures pending", palette().TEXT_MUTED)
        };
        let response = ui.label(egui::RichText::new(icon).color(color));
        response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, status));
        response.on_hover_text(status);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            details_button(ui, expanded, Detail::Procedures, "procedure");
            // Counts and controls keep their intrinsic width; only the procedure title truncates.
            if active > 1 {
                let response = ui.label(
                    egui::RichText::new(format!("+{}", active - 1))
                        .size(FONT_SMALL)
                        .color(palette().TEXT_MUTED),
                );
                response.widget_info(|| {
                    egui::WidgetInfo::labeled(
                        egui::WidgetType::Label,
                        true,
                        format!("{active} procedures in progress"),
                    )
                });
                response.on_hover_text(format!("{active} procedures in progress"));
            }
            ui.label(
                egui::RichText::new(format!("{completed}/{}", todo.items.len()))
                    .size(FONT_SMALL)
                    .color(palette().TEXT_MUTED),
            );
            let response = ui.add_sized(
                [ui.available_width().max(0.0), ROW_DENSE - SP_1],
                egui::Label::new(text).truncate(),
            );
            response.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::WidgetType::Label,
                    true,
                    format!("Procedures: {accessible_text}"),
                )
            });
            response.on_hover_text(accessible_text);
        });
    });
}

fn procedure_details(ui: &mut egui::Ui, todo: &event_bus::ThreadTodoSnapshot) {
    use event_bus::ThreadTodoStatus;
    for item in &todo.items {
        let (icon, status) = match item.status {
            ThreadTodoStatus::Pending => (icons::CIRCLE, "Pending"),
            ThreadTodoStatus::InProgress => (icons::CIRCLE, "In progress"),
            ThreadTodoStatus::Completed => (icons::CHECK_CIRCLE, "Completed"),
        };
        ui.horizontal_top(|ui| {
            let response = ui.label(icon);
            response
                .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, status));
            ui.vertical(|ui| {
                ui.label(&item.content);
                ui.label(
                    egui::RichText::new(status)
                        .size(FONT_SMALL)
                        .color(palette().TEXT_MUTED),
                );
            });
        });
    }
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
