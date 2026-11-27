//! The progress band above a composer: queued input rows, the thread goal and
//! the plan's progress share one inset band, and the goal's details expand
//! inside it.
//!
//! Pure drawing shared by the embedded workbench and the core client: the goal
//! arrives as a borrowed [`GoalBandView`] whatever it was read from, the queue
//! rows are drawn by the caller's closure, and a press comes back as a
//! [`GoalAction`] for the owner to send.
use crate::panes::conversation_plan::{PlanBand, band_row};
use crate::theme::widgets::{icon_button, labeled};
use crate::theme::{icons, tokens::*};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalPhaseView {
    Working,
    Checking,
    Reviewing,
    Repairing,
    Complete,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GoalCheckRow<'a> {
    /// Zero-based index into the goal's criteria.
    pub criterion: usize,
    pub met: bool,
    pub evidence: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GoalUsageView {
    pub requests: u64,
    pub input: u64,
    pub output: u64,
}

/// A thread goal as the band draws it. `original_request` and `usage` are
/// `None` where the source does not have them; their detail lines are skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalBandView<'a> {
    pub thread_id: &'a str,
    pub goal_id: &'a str,
    pub objective: &'a str,
    pub phase: GoalPhaseView,
    pub review_enabled: bool,
    pub checks_paused: bool,
    pub work_stopped: bool,
    pub reason: Option<&'a str>,
    pub review_round: u32,
    pub max_review_rounds: u32,
    pub criteria: &'a [String],
    pub checks: Vec<GoalCheckRow<'a>>,
    pub findings: &'a [String],
    pub original_request: Option<&'a str>,
    pub usage: Option<GoalUsageView>,
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Detail {
    Goal,
}

/// The details left open, and the goal they were opened for. A different goal
/// starts collapsed; a goal that is only missing for a while (a thread switch,
/// a reconnect, a re-read still on its way) finds them open again.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Expanded {
    detail: Detail,
    goal_id: String,
}

/// Tallest the expanded goal details grow before scrolling.
pub const DETAILS_MAX_HEIGHT: f32 = 180.0;

/// What the band reports for one frame. `Q` is what the caller's queue rows
/// return when one of their controls is pressed.
#[derive(Debug)]
pub struct BandOutput<Q> {
    pub action: Option<GoalAction>,
    /// A queued input control pressed this frame.
    pub queue_action: Option<Q>,
    /// Height the details viewport occupied this frame (0 when collapsed).
    pub details_viewport: f32,
    /// Height the details want, capped at [`DETAILS_MAX_HEIGHT`].
    pub details_wanted: f32,
    /// The plan band's open button was pressed this frame.
    pub open_plan: bool,
}

impl<Q> Default for BandOutput<Q> {
    fn default() -> Self {
        Self {
            action: None,
            queue_action: None,
            details_viewport: 0.0,
            details_wanted: 0.0,
            open_plan: false,
        }
    }
}

/// Queued input rows, then the goal row, then the plan row, in one inset band.
/// Expanded goal details scroll within `details_max` so the composer below
/// keeps its room. `id_salt` keeps the expanded state per conversation, and a
/// frame without the goal leaves it as it was. With nothing to show the band
/// draws nothing.
pub fn progress_band<Q>(
    ui: &mut egui::Ui,
    id_salt: &str,
    goal: Option<&GoalBandView<'_>>,
    queue: Option<impl FnOnce(&mut egui::Ui) -> Option<Q>>,
    details_max: f32,
    plan: Option<PlanBand<'_>>,
) -> BandOutput<Q> {
    let mut output = BandOutput::default();
    // Before the first plan there is no plan row, so it does not keep the band open.
    let plan = plan.filter(PlanBand::has_row);
    if goal.is_none() && queue.is_none() && plan.is_none() {
        return output;
    }
    let expanded_id = ui.id().with(("progress-details", id_salt));
    let mut expanded = goal.and_then(|goal| {
        ui.data(|data| data.get_temp::<Expanded>(expanded_id))
            .filter(|stored| stored.goal_id == goal.goal_id)
            .map(|stored| stored.detail)
    });
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
                    if let Some(queue) = queue {
                        output.queue_action = queue(ui);
                        if goal.is_some() || plan.is_some() {
                            ui.separator();
                        }
                    }
                    if let Some(goal) = goal {
                        output.action = goal_row(ui, id_salt, goal, &mut expanded);
                    }
                    if let Some(plan) = plan {
                        output.open_plan = band_row(ui, plan);
                    }
                    if let Some(detail) = expanded {
                        ui.separator();
                        let height = details_max.clamp(ROW_DENSE, DETAILS_MAX_HEIGHT);
                        let details = egui::ScrollArea::vertical()
                            .id_salt(("progress-scroll", id_salt, detail))
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
    if let Some(goal) = goal {
        ui.data_mut(|data| match expanded {
            Some(detail) => {
                let goal_id = goal.goal_id.to_owned();
                data.insert_temp(expanded_id, Expanded { detail, goal_id });
            }
            None => data.remove::<Expanded>(expanded_id),
        });
    }
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
    id_salt: &str,
    goal: &GoalBandView<'_>,
    expanded: &mut Option<Detail>,
) -> Option<GoalAction> {
    let mut action = None;
    ui.horizontal(|ui| {
        let state = match goal.phase {
            GoalPhaseView::Complete => {
                Some((icons::CHECK_CIRCLE, "Goal completed", palette().SUCCESS))
            }
            GoalPhaseView::Blocked => Some((
                icons::WARNING_CIRCLE,
                "Goal needs attention",
                palette().WARNING_FG,
            )),
            GoalPhaseView::Checking => Some((
                icons::MAGNIFYING_GLASS,
                "Checking completion",
                palette().TEXT_MUTED,
            )),
            GoalPhaseView::Reviewing => Some((
                icons::MAGNIFYING_GLASS,
                "Independent review in progress",
                palette().TEXT_MUTED,
            )),
            GoalPhaseView::Working | GoalPhaseView::Repairing => None,
        };
        if let Some((icon, label, color)) = state {
            let response = ui.label(egui::RichText::new(icon).color(color));
            response
                .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, label));
            response.on_hover_text(label);
        }
        // The phase icon above appears/disappears while a click can be in flight.
        // A positional child ID would then change every control's ID, losing the
        // press that egui hit-tested against the previous frame.
        let controls_id = ui
            .id()
            .with(("goal-controls", id_salt, goal.thread_id, goal.goal_id));
        let controls = egui::UiBuilder::new()
            .id(controls_id)
            .layout(egui::Layout::right_to_left(egui::Align::Center));
        ui.scope_builder(controls, |ui| {
            details_button(ui, expanded, Detail::Goal, "goal");
            ui.add_enabled_ui(goal.phase != GoalPhaseView::Complete, |ui| {
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
                        thread_id: goal.thread_id.to_owned(),
                        goal_id: goal.goal_id.to_owned(),
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
                        thread_id: goal.thread_id.to_owned(),
                        goal_id: goal.goal_id.to_owned(),
                        enabled: !goal.review_enabled,
                    });
                }
            });
            let response = ui.add_sized(
                [ui.available_width().max(0.0), ROW_DENSE - SP_1],
                egui::Label::new(goal.objective).truncate(),
            );
            response.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::WidgetType::Label,
                    true,
                    format!("Goal: {}", goal.objective),
                )
            });
            response.on_hover_text(goal.objective);
        });
    });
    action
}

fn goal_details(ui: &mut egui::Ui, goal: &GoalBandView<'_>) {
    ui.label(goal.objective);
    if let Some(request) = goal.original_request {
        ui.label(format!("Original request: {request}"));
    }
    if goal.checks_paused {
        ui.label("Completion checks paused. Work may continue.");
    }
    if goal.work_stopped {
        ui.label("Work stopped. Continue the conversation to resume.");
    }
    if let Some(reason) = goal.reason {
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
            ui.label(check.evidence);
        }
    }
    if !goal.findings.is_empty() {
        ui.label("Review findings");
        for finding in goal.findings {
            ui.label(finding);
        }
    }
    if let Some(usage) = goal.usage {
        ui.label(format!(
            "Requests: {} · input {} · output {} tokens",
            usage.requests, usage.input, usage.output
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::{Harness, kittest::Queryable};

    fn view(goal_id: &'static str, phase: GoalPhaseView) -> GoalBandView<'static> {
        GoalBandView {
            thread_id: "A",
            goal_id,
            objective: "ship it",
            phase,
            review_enabled: false,
            checks_paused: false,
            work_stopped: false,
            reason: None,
            review_round: 1,
            max_review_rounds: 3,
            criteria: &[],
            checks: Vec::new(),
            findings: &[],
            original_request: None,
            usage: None,
        }
    }

    /// A band for conversation "A" showing the goal named in the state, or no
    /// goal at all while it is `None`.
    fn band() -> Harness<'static, Option<&'static str>> {
        Harness::new_ui_state(
            |ui, goal_id: &mut Option<&'static str>| {
                let view = goal_id.map(|id| view(id, GoalPhaseView::Working));
                progress_band::<()>(
                    ui,
                    "A",
                    view.as_ref(),
                    None::<fn(&mut egui::Ui) -> Option<()>>,
                    DETAILS_MAX_HEIGHT,
                    None,
                );
            },
            Some("goal-1"),
        )
    }

    struct ClickState {
        goal: GoalBandView<'static>,
        actions: Vec<GoalAction>,
    }

    fn click_harness(phase: GoalPhaseView) -> Harness<'static, ClickState> {
        Harness::new_ui_state(
            |ui, state: &mut ClickState| {
                let output = progress_band::<()>(
                    ui,
                    "A",
                    Some(&state.goal),
                    None::<fn(&mut egui::Ui) -> Option<()>>,
                    DETAILS_MAX_HEIGHT,
                    None,
                );
                state.actions.extend(output.action);
            },
            ClickState {
                goal: view("goal-1", phase),
                actions: Vec::new(),
            },
        )
    }

    fn pointer(harness: &mut Harness<'_, ClickState>, pos: egui::Pos2, pressed: bool) {
        harness.event(egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        });
        harness.step();
    }

    #[test]
    fn controls_keep_a_single_click_when_the_phase_changes() {
        for label in [
            "Enable independent review before completion",
            "Pause goal checks",
            "Show goal details",
        ] {
            for (from, to) in [
                (GoalPhaseView::Working, GoalPhaseView::Checking),
                (GoalPhaseView::Checking, GoalPhaseView::Repairing),
                (GoalPhaseView::Repairing, GoalPhaseView::Reviewing),
                (GoalPhaseView::Reviewing, GoalPhaseView::Blocked),
            ] {
                for change_on_press in [true, false] {
                    let mut harness = click_harness(from);
                    harness.run();
                    let pos = harness.get_by_label(label).rect().center();
                    harness.hover_at(pos);
                    harness.step();
                    // A core update can arrive on either edge of a real click.
                    // Keep press/release in separate frames, with no retry.
                    for pressed in [true, false] {
                        if pressed == change_on_press {
                            harness.state_mut().goal.phase = to;
                        }
                        pointer(&mut harness, pos, pressed);
                    }
                    harness.run();
                    let expected = match label {
                        "Enable independent review before completion" => vec![GoalAction::Review {
                            thread_id: "A".into(),
                            goal_id: "goal-1".into(),
                            enabled: true,
                        }],
                        "Pause goal checks" => vec![GoalAction::ChecksPaused {
                            thread_id: "A".into(),
                            goal_id: "goal-1".into(),
                            paused: true,
                        }],
                        _ => {
                            assert!(
                                harness.query_by_label("Hide goal details").is_some(),
                                "{label}: {from:?} -> {to:?}, change_on_press={change_on_press}"
                            );
                            vec![]
                        }
                    };
                    assert_eq!(
                        harness.state().actions,
                        expected,
                        "{label}: {from:?} -> {to:?}, change_on_press={change_on_press}"
                    );
                }
            }
        }
    }

    #[test]
    fn completing_or_replacing_a_goal_does_not_inherit_a_press() {
        for replacement in ["complete", "goal", "thread"] {
            let mut harness = click_harness(GoalPhaseView::Working);
            harness.run();
            let pos = harness
                .get_by_label("Enable independent review before completion")
                .rect()
                .center();
            harness.hover_at(pos);
            harness.step();
            pointer(&mut harness, pos, true);
            match replacement {
                "complete" => harness.state_mut().goal.phase = GoalPhaseView::Complete,
                "goal" => harness.state_mut().goal.goal_id = "goal-2",
                "thread" => harness.state_mut().goal.thread_id = "B",
                _ => unreachable!(),
            }
            pointer(&mut harness, pos, false);
            harness.run();
            assert!(harness.state().actions.is_empty(), "{replacement}");
        }
    }

    // Given: a band with only a plan that was never written to
    // When: it is drawn / Then: the band takes no room, as with nothing to show
    #[test]
    fn a_band_with_only_an_unplanned_plan_draws_nothing() {
        let summary = workbench_protocol::namespaces::execution::plan::PlanSummary {
            thread: workbench_protocol::ids::ThreadId::new("A").expect("id"),
            plan_id: None,
            revision: workbench_protocol::wire::Revision(0),
            progress: Default::default(),
            updated_at_unix_ms: None,
        };
        let mut height = None;
        let mut harness = Harness::new_ui(|ui| {
            let before = ui.min_rect().height();
            progress_band::<()>(
                ui,
                "A",
                None,
                None::<fn(&mut egui::Ui) -> Option<()>>,
                DETAILS_MAX_HEIGHT,
                Some(PlanBand {
                    summary: Some(&summary),
                }),
            );
            height = Some(ui.min_rect().height() - before);
        });
        harness.run();
        drop(harness);
        assert_eq!(height, Some(0.0));
    }

    // Given: the goal's details are open
    // When: the goal is missing for a few frames, then the same goal returns
    // Then: the details are open again
    #[test]
    fn details_stay_open_while_the_goal_is_briefly_missing() {
        let mut harness = band();
        harness.get_by_label("Show goal details").click();
        harness.run();
        harness.get_by_label("Independent review: disabled · round 1/3");

        *harness.state_mut() = None;
        harness.run();
        assert!(harness.query_by_label("Hide goal details").is_none());

        *harness.state_mut() = Some("goal-1");
        harness.run();
        harness.get_by_label("Hide goal details");
        harness.get_by_label("Independent review: disabled · round 1/3");
    }

    // Given: the goal's details are open
    // When: a different goal takes its place
    // Then: the new goal's details start collapsed
    #[test]
    fn a_different_goal_starts_collapsed() {
        let mut harness = band();
        harness.get_by_label("Show goal details").click();
        harness.run();
        harness.get_by_label("Hide goal details");

        *harness.state_mut() = Some("goal-2");
        harness.run();
        harness.get_by_label("Show goal details");
        assert!(
            harness
                .query_by_label("Independent review: disabled · round 1/3")
                .is_none()
        );
    }

    // Given: the goal's details are open
    // When: they are hidden, the goal goes missing, and it returns
    // Then: they stay collapsed
    #[test]
    fn hidden_details_stay_hidden_after_the_goal_returns() {
        let mut harness = band();
        harness.get_by_label("Show goal details").click();
        harness.run();
        harness.get_by_label("Hide goal details").click();
        harness.run();

        *harness.state_mut() = None;
        harness.run();
        *harness.state_mut() = Some("goal-1");
        harness.run();
        harness.get_by_label("Show goal details");
    }
}
