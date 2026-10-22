//! Bottom panel holding the goal/procedure band, composer and status strip.
//!
//! The panel is sized from measured parts instead of last frame's total so the
//! input claims room before expanded band details: a growing message shrinks
//! the details' scroll viewport, never the input, the Send row or the status.
use super::{AgentPaneAction, ConversationContext, goal, header::status_strip};
use crate::model::composer::ComposerModel;
use crate::model::model_picker::ModelPickerState;
use crate::panes::composer::{ComposerAction, composer_strip_within};
use crate::theme::tokens::{COMPOSER_MIN_HEIGHT, ROW_COMPACT, ROW_DENSE, SP_2};

#[derive(Debug, Clone, Copy, PartialEq)]
struct Measured {
    /// Everything except the band details and input viewports.
    chrome: f32,
    details: f32,
    input: f32,
}

impl Measured {
    fn differs(self, other: Self) -> bool {
        (self.chrome - other.chrome).abs() > 0.5
            || (self.details - other.details).abs() > 0.5
            || (self.input - other.input).abs() > 0.5
    }
}

pub(super) fn composer_panel(
    ui: &mut egui::Ui,
    ctx: &ConversationContext<'_>,
    composer: &mut ComposerModel,
    picker_state: &mut ModelPickerState,
    agent_focused: bool,
    action: &mut Option<AgentPaneAction>,
) {
    let id = ui.id().with("composer-layout");
    let min_input = COMPOSER_MIN_HEIGHT - 2.0 * SP_2;
    let last = ui.data(|data| data.get_temp::<Measured>(id));
    let measured = last.unwrap_or(Measured {
        chrome: COMPOSER_MIN_HEIGHT,
        details: 0.0,
        input: min_input,
    });
    // At least one transcript row stays visible above the panel.
    let room = (ui.available_height() - ROW_COMPACT).max(0.0);
    let input = measured.input.min((room - measured.chrome).max(min_input));
    let details = if measured.details > 0.0 {
        (room - measured.chrome - input).clamp(ROW_DENSE.min(measured.details), measured.details)
    } else {
        0.0
    };
    let height = (measured.chrome + details + input).max(COMPOSER_MIN_HEIGHT);
    egui::Panel::bottom(ui.id().with("composer"))
        .exact_size(height)
        .resizable(false)
        .show_separator_line(false)
        .frame(egui::Frame::NONE)
        .show(ui, |ui| {
            let strip = ui.scope(|ui| {
                let vertical_spacing = ui.spacing().item_spacing.y;
                let mut band = goal::BandOutput::default();
                if ctx.goal.is_some() || ctx.todo.is_some() || !composer.queued.is_empty() {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    let queue = super::queue::QueueBand {
                        items: &composer.queued,
                        next_turn_requested: composer.follow_ups.next_turn_requested,
                    };
                    band = goal::progress_strip(ui, ctx.goal, ctx.todo, queue, details);
                }
                let strip = ui
                    .push_id("composer-strip", |ui| {
                        composer_strip_within(
                            ui,
                            composer,
                            (ctx.model_picker, picker_state),
                            ctx.phase,
                            ctx.sandbox_picker,
                            input,
                        )
                    })
                    .inner;
                ui.spacing_mut().item_spacing.y = vertical_spacing;
                status_strip(ui, ctx, agent_focused);
                (band, strip)
            });
            let (band, strip_output) = strip.inner;
            let next = Measured {
                chrome: strip.response.rect.height()
                    - band.details_viewport
                    - strip_output.input_viewport,
                details: band.details_wanted,
                input: strip_output.input_wanted,
            };
            if last.is_none_or(|last| last.differs(next)) {
                ui.data_mut(|data| data.insert_temp(id, next));
                ui.ctx().request_discard("composer panel layout changed");
                ui.ctx().request_repaint();
            }
            if let Some(goal_action) = band.action {
                *action = Some(AgentPaneAction::Goal(goal_action));
            }
            if let Some(queue_action) = band.queue_action {
                *action = Some(AgentPaneAction::Composer(queue_action));
            }
            if let Some(composer_action) = strip_output.action {
                *action = Some(match composer_action {
                    ComposerAction::ModelPreference(preference) => {
                        AgentPaneAction::ModelPreference(preference)
                    }
                    other => AgentPaneAction::Composer(other),
                });
            }
        });
}
