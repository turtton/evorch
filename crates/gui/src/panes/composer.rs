//! Composer strip rendering and action reporting without dispatch.

use crate::model::composer::{ComposerModel, ProviderStatus, completions};
use crate::theme::text::muted;
use crate::theme::tokens::{INPUT, ROW_COMPACT, SP_1, SP_2, SURFACE_RAISED};
use crate::theme::widgets::{primary_button, surface_frame};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposerAction {
    Send,
    Complete(&'static str),
    OpenSettings,
}

pub fn composer_strip(
    ui: &mut egui::Ui,
    model: &mut ComposerModel,
    provider: &crate::model::composer::ProviderStatus,
) -> Option<ComposerAction> {
    let mut action = None;
    surface_frame(SURFACE_RAISED).show(ui, |ui| {
        ui.vertical(|ui| {
            ui.set_min_height(ROW_COMPACT);
            ui.spacing_mut().item_spacing = egui::vec2(SP_2, SP_1);
            let candidates = completions(&model.input);
            if !candidates.is_empty() {
                ui.horizontal(|ui| {
                    for spec in candidates {
                        let label = match spec.argument_hint {
                            Some(hint) => format!("/{} {hint}", spec.name),
                            None => format!("/{}", spec.name),
                        };
                        if ui.button(label).clicked() {
                            action = Some(ComposerAction::Complete(spec.name));
                        }
                    }
                });
            }
            match provider {
                ProviderStatus::Configured => {
                    if ui.add(egui::Button::new("Settings").small()).clicked() {
                        action = Some(ComposerAction::OpenSettings);
                    }
                }
                ProviderStatus::NotConfigured { guidance } => {
                    ui.label(muted(guidance));
                    if primary_button(ui, "Open Settings").clicked() {
                        action = Some(ComposerAction::OpenSettings);
                    }
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let can_send = !model.input.trim().is_empty();
                let send = if can_send {
                    primary_button(ui, "Send")
                } else {
                    ui.add_enabled(false, egui::Button::new("Send"))
                };
                let input = ui.add(
                    egui::TextEdit::singleline(&mut model.input)
                        .hint_text("Message or /command")
                        .desired_width(f32::INFINITY)
                        .background_color(INPUT),
                );
                let enter = input.lost_focus()
                    && ui.input(|input| {
                        input.key_pressed(egui::Key::Enter) && !input.modifiers.shift
                    });
                if can_send && (send.clicked() || enter) {
                    action = Some(ComposerAction::Send);
                }
            });
        });
    });
    action
}

#[cfg(test)]
mod tests {
    use egui_kittest::{
        Harness,
        kittest::{NodeT, Queryable},
    };

    use super::*;
    use crate::model::composer::{PROVIDER_MISSING_GUIDANCE, ProviderStatus};

    struct Fixture {
        model: ComposerModel,
        provider: ProviderStatus,
        action: Option<ComposerAction>,
    }

    fn harness(input: &str, provider: ProviderStatus) -> Harness<'static, Fixture> {
        Harness::builder().build_ui_state(
            |ui, state: &mut Fixture| {
                crate::theme::install(ui.ctx());
                if let Some(action) = composer_strip(ui, &mut state.model, &state.provider) {
                    state.action = Some(action);
                }
            },
            Fixture {
                model: ComposerModel {
                    input: input.into(),
                },
                provider,
                action: None,
            },
        )
    }

    #[test]
    fn send_button_disabled_on_empty_input() {
        // Given
        let mut harness = harness("", ProviderStatus::Configured);
        // When
        harness.run();
        // Then
        assert!(harness.get_by_label("Send").accesskit_node().is_disabled());
        assert_eq!(harness.state().action, None);
    }

    #[test]
    fn send_click_emits_send_action() {
        // Given
        let mut harness = harness("hi", ProviderStatus::Configured);
        harness.run();
        // When
        harness.get_by_label("Send").click();
        harness.run();
        // Then
        assert_eq!(harness.state().action, Some(ComposerAction::Send));
    }

    #[test]
    fn slash_prefix_shows_completion_candidates_and_click_fills_via_action() {
        // Given
        let mut harness = harness("/", ProviderStatus::Configured);
        harness.run();
        harness.get_by_label("/help");
        // When
        harness.get_by_label("/goal <text>").click();
        harness.run();
        // Then
        assert_eq!(
            harness.state().action,
            Some(ComposerAction::Complete("goal"))
        );
        assert_eq!(harness.state().model.input, "/");
    }

    #[test]
    fn guidance_label_shown_when_provider_missing() {
        // Given
        let mut harness = harness("", ProviderStatus::default());
        // When
        harness.run();
        // Then
        harness.get_by_label(PROVIDER_MISSING_GUIDANCE);
    }

    #[test]
    fn send_button_disabled_on_whitespace_input() {
        // Given
        let mut harness = harness("  ", ProviderStatus::Configured);
        // When
        harness.run();
        // Then
        assert!(harness.get_by_label("Send").accesskit_node().is_disabled());
    }
}
