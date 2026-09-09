//! Composer strip rendering and action reporting without dispatch.

use crate::model::composer::{ComposerModel, ProviderStatus, completions};
use crate::theme::text::muted;
use crate::theme::tokens::{
    COMPOSER_MAX_HEIGHT, COMPOSER_MIN_HEIGHT, INPUT, R_2XL, ROW_COMPACT, SP_1, SP_2, SP_3,
    SURFACE_RAISED,
};
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
    surface_frame(SURFACE_RAISED)
        .corner_radius(R_2XL)
        .inner_margin(egui::vec2(SP_3, SP_2))
        .show(ui, |ui| {
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
            ui.horizontal(|ui| { ui.with_layout(egui::Layout::right_to_left(egui::Align::BOTTOM), |ui| {
                let can_send = !model.input.trim().is_empty();
                let send = if can_send {
                    primary_button(ui, "Send")
                } else {
                    ui.add_enabled(false, egui::Button::new("Send"))
                };
                let ime_id = ui.id().with("ime-composing");
                let mut ime_composing = ui.data(|data| data.get_temp::<bool>(ime_id).unwrap_or_default());
                ui.input(|input| {
                    for event in &input.events {
                        if let egui::Event::Ime(event) = event {
                            #[allow(deprecated)] // Accept legacy backend Enabled/Disabled events too.
                            match event {
                                egui::ImeEvent::Preedit { text, .. } => ime_composing = !text.is_empty(),
                                egui::ImeEvent::Commit(_) | egui::ImeEvent::Disabled => ime_composing = false,
                                egui::ImeEvent::Enabled | egui::ImeEvent::DeleteSurrounding { .. } => {}
                            }
                        }
                    }
                });
                let input = egui::ScrollArea::vertical()
                    .id_salt("composer-scroll")
                    .min_scrolled_height(COMPOSER_MIN_HEIGHT - 2.0 * SP_2)
                    .max_height(COMPOSER_MAX_HEIGHT)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.with_layout(egui::Layout::top_down(egui::Align::LEFT), |ui| {
                        ui.add(egui::TextEdit::multiline(&mut model.input)
                            .id_salt("composer-input")
                            .hint_text("Message or /command  (Enter to send, Shift+Enter for newline)")
                            .desired_rows(1)
                            .desired_width(f32::INFINITY)
                            .min_size(egui::vec2(0.0, COMPOSER_MIN_HEIGHT - 2.0 * SP_2))
                            .return_key(egui::KeyboardShortcut::new(egui::Modifiers::SHIFT, egui::Key::Enter))
                            .frame(egui::Frame::NONE.inner_margin(egui::vec2(SP_3, SP_2)))
                            .margin(egui::vec2(SP_3, SP_2))
                            .background_color(INPUT))
                        }).inner
                    }).inner;
                input.widget_info(|| {
                    egui::WidgetInfo::labeled(
                        egui::WidgetType::TextEdit,
                        true,
                        "Message or /command",
                    )
                });
                let enter = input.has_focus()
                    && ui.input(|input| {
                        input.key_pressed(egui::Key::Enter) && !input.modifiers.shift && !input.modifiers.command
                    }) && !ime_composing;
                let focused = input.has_focus();
                ui.data_mut(|data| data.insert_temp(ime_id, ime_composing && focused));
                if !model.input.trim().is_empty() && (send.clicked() || enter) {
                    action = Some(ComposerAction::Send);
                    input.request_focus();
                }
            }); });
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
    use crate::theme::tokens::SP_3;

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
    fn typing_slash_prefix_keeps_focus() {
        // Given: the composer has focus and no input
        let mut harness = harness("", ProviderStatus::Configured);
        harness.get_by_label("Message or /command").focus();
        harness.run();

        // When: a slash-command prefix is typed one character at a time
        harness
            .input_mut()
            .events
            .push(egui::Event::Text("/".into()));
        harness.run();
        // Then: focus and input survive the first completion row
        assert!(harness.ctx.memory(|memory| memory.focused()).is_some());
        assert_eq!(harness.state().model.input, "/");

        harness
            .input_mut()
            .events
            .push(egui::Event::Text("g".into()));
        harness.run();
        assert!(harness.ctx.memory(|memory| memory.focused()).is_some());
        assert_eq!(harness.state().model.input, "/g");
        harness.get_by_label("/goal <text>");

        harness
            .input_mut()
            .events
            .push(egui::Event::Text("o".into()));
        harness.run();
        assert!(harness.ctx.memory(|memory| memory.focused()).is_some());
        assert_eq!(harness.state().model.input, "/go");
    }

    #[test]
    fn completion_disappearance_keeps_focus() {
        // Given: a focused composer with slash completions visible
        let mut harness = harness("/g", ProviderStatus::Configured);
        harness.get_by_label("Message or /command").focus();
        harness.run();

        // When: a space makes the completion candidates disappear
        harness
            .input_mut()
            .events
            .push(egui::Event::Text(" ".into()));
        harness.run();

        // Then: the composer remains focused
        assert!(harness.ctx.memory(|memory| memory.focused()).is_some());
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

    #[test]
    fn enter_with_focus_emits_send() {
        // Given
        let mut harness = harness("hi", ProviderStatus::Configured);
        harness.get_by_label("Message or /command").focus();
        harness.run();
        // When
        harness.key_press(egui::Key::Enter);
        harness.run();
        // Then
        assert_eq!(harness.state().action, Some(ComposerAction::Send));
    }

    #[test]
    fn shift_enter_inserts_newline_and_does_not_send() {
        // Given
        let mut harness = harness("hi", ProviderStatus::Configured);
        harness.get_by_label("Message or /command").focus();
        harness.run();
        // When
        harness.key_press_modifiers(egui::Modifiers::SHIFT, egui::Key::Enter);
        harness.run();
        // Then
        assert_eq!(harness.state().action, None);
        assert!(harness.state().model.input.contains('\n'));
    }

    #[test]
    fn enter_during_ime_preedit_does_not_send() {
        // Given: composition persists across frames without another preedit event.
        let mut harness = harness("hi", ProviderStatus::Configured);
        harness.get_by_label("Message or /command").focus();
        harness.run();
        harness
            .input_mut()
            .events
            .push(egui::Event::Ime(egui::ImeEvent::Preedit {
                text: "あ".into(),
                active_range_chars: None,
            }));
        harness.run();
        // When
        harness.key_press(egui::Key::Enter);
        harness.run();
        // Then
        assert_eq!(harness.state().action, None);
    }

    #[test]
    fn enter_after_ime_commit_emits_send() {
        // Given
        let mut harness = harness("hi", ProviderStatus::Configured);
        harness.get_by_label("Message or /command").focus();
        harness.run();
        harness
            .input_mut()
            .events
            .push(egui::Event::Ime(egui::ImeEvent::Preedit {
                text: "あ".into(),
                active_range_chars: None,
            }));
        harness.run();
        harness
            .input_mut()
            .events
            .push(egui::Event::Ime(egui::ImeEvent::Commit("あ".into())));
        harness.run();
        // When
        harness.key_press(egui::Key::Enter);
        harness.run();
        // Then
        assert_eq!(harness.state().action, Some(ComposerAction::Send));
    }

    #[test]
    fn composer_respects_min_height_token() {
        // Given
        let mut harness = harness("", ProviderStatus::Configured);
        // When
        harness.run();
        // Then
        let rect = harness.get_by_label("Message or /command").rect();
        assert!(rect.height() >= crate::theme::tokens::COMPOSER_MIN_HEIGHT - 2.0 * SP_2 - 1.0);
    }

    #[test]
    fn composer_caps_height_and_scrolls() {
        // Given
        let mut harness = harness(&"line\n".repeat(40), ProviderStatus::Configured);
        // When
        harness.run();
        // Then: a scrollable text document is taller than its bounded viewport.
        let rect = harness.get_by_label("Message or /command").rect();
        assert!(rect.height() > crate::theme::tokens::COMPOSER_MAX_HEIGHT);
        assert!(
            harness.get_by_label("Send").rect().max.y
                <= crate::theme::tokens::COMPOSER_MAX_HEIGHT + ROW_COMPACT + 2.0 * SP_3
        );
    }
}
