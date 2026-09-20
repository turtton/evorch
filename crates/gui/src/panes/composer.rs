//! Composer strip rendering and action reporting without dispatch.

use crate::model::composer::{ComposerModel, completions};
use crate::theme::tokens::{
    COMPOSER_MAX_HEIGHT, COMPOSER_MIN_HEIGHT, R_2XL, ROW_COMPACT, SP_1, SP_2, SP_3, palette,
};
use crate::theme::widgets::{primary_button, surface_frame};
use workspace_ui::ThreadRunPhase;

#[path = "composer_images.rs"]
mod images;
mod selectors;

#[derive(Clone, Copy, Default)]
pub struct SandboxPickerContext {
    pub mode: config::EscalationApproval,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposerAction {
    Send,
    Cancel,
    Complete(&'static str),
    CompleteExternal(String),
    ModelPreference(Option<workspace_ui::ModelPreference>),
    OpenSandboxSettings,
}

pub fn composer_strip(
    ui: &mut egui::Ui,
    model: &mut ComposerModel,
    picker: crate::panes::model_picker::ModelPickerContext<'_>,
    picker_state: &mut crate::model::model_picker::ModelPickerState,
    phase: Option<ThreadRunPhase>,
    sandbox: SandboxPickerContext,
) -> Option<ComposerAction> {
    let mut action = None;
    surface_frame(palette().SURFACE_RAISED)
        .corner_radius(R_2XL)
        .inner_margin(egui::vec2(SP_3, SP_2))
        .show(ui, |ui| {
        ui.vertical(|ui| {
            ui.set_min_height(ROW_COMPACT);
            ui.spacing_mut().item_spacing = egui::vec2(SP_2, SP_1);
            let candidates = completions(&model.input);
            if model.completions_visible() {
                ui.horizontal(|ui| {
                    for spec in candidates {
                        let label = match spec.argument_hint {
                            Some(hint) => format!("/{} {hint}", spec.name),
                            None => format!("/{}", spec.name),
                        };
                        if ui.push_id(("completion", spec.name), |ui| ui.button(label)).inner.clicked() {
                            action = Some(ComposerAction::Complete(spec.name));
                        }
                    }
                    for spec in model.registry.completions(&model.input) {
                        if ui.button(format!("/{}", spec.name)).clicked() {
                            action = Some(ComposerAction::CompleteExternal(spec.name.clone()));
                        }
                    }
                });
            }
            if let Some(selected) = selectors::row(ui, sandbox, (picker, picker_state)) {
                action = Some(selected);
            }
            images::render(ui, model);
            let target = match model.resolved_model.as_deref() {
                Some(resolved) => format!("{} · {resolved}", model.role.label()),
                None => model.role.label().to_owned(),
            };
            ui.label(egui::RichText::new(format!("送信先: {target}  (Tab で切替)"))
                .small().color(palette().TEXT_MUTED));
            ui.horizontal(|ui| { ui.with_layout(egui::Layout::right_to_left(egui::Align::BOTTOM), |ui| {
                let can_send = !model.input.trim().is_empty() || !model.attachments.is_empty();
                let can_cancel = phase == Some(ThreadRunPhase::Running) && !model.completions_visible();
                let send = if can_cancel {
ui.add(egui::Button::new(egui::RichText::new("Cancel").color(palette().ERROR_FG))
.fill(palette().ERROR_SURFACE))
                } else if can_send {
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
                            // Retain editor focus semantics; raw_input_hook owns Tab
                            // before egui, so toggling no longer relies on this filter.
                            .lock_focus(true)
                            .desired_rows(1)
                            .desired_width(f32::INFINITY)
                            .min_size(egui::vec2(0.0, COMPOSER_MIN_HEIGHT - 2.0 * SP_2))
                            .return_key(egui::KeyboardShortcut::new(egui::Modifiers::SHIFT, egui::Key::Enter))
                            .frame(egui::Frame::NONE.inner_margin(egui::vec2(SP_3, SP_2)))
                            .margin(egui::vec2(SP_3, SP_2))
.background_color(palette().INPUT))
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
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    if model.completions_visible() {
                        model.dismiss_completions();
                    } else if phase == Some(ThreadRunPhase::Running) {
                        action = Some(ComposerAction::Cancel);
                    }
                    input.request_focus();
                } else if can_cancel && send.clicked() {
                    action = Some(ComposerAction::Cancel);
                    input.request_focus();
                } else if can_send && (send.clicked() || enter) {
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
    use crate::theme::tokens::SP_3;

    struct Fixture {
        phase: Option<workspace_ui::ThreadRunPhase>,
        model: ComposerModel,
        picker_state: crate::model::model_picker::ModelPickerState,
        action: Option<ComposerAction>,
    }

    fn harness(input: &str) -> Harness<'static, Fixture> {
        Harness::builder().build_ui_state(
            |ui, state: &mut Fixture| {
                crate::theme::install(ui.ctx());
                if let Some(action) = composer_strip(
                    ui,
                    &mut state.model,
                    crate::panes::model_picker::ModelPickerContext {
                        profiles: &[],
                        preference: None,
                        enabled: false,
                    },
                    &mut state.picker_state,
                    state.phase,
                    SandboxPickerContext::default(),
                ) {
                    state.action = Some(action);
                }
            },
            Fixture {
                phase: None,
                model: ComposerModel {
                    input: input.into(),
                    ..Default::default()
                },
                picker_state: crate::model::model_picker::ModelPickerState::default(),
                action: None,
            },
        )
    }

    include!("composer_capture_test.rs");

    #[test]
    fn esc_with_completions_visible_dismisses_them_and_emits_no_action() {
        let mut h = harness("/");
        h.get_by_label("Message or /command").focus();
        h.run();
        let focus = h.ctx.memory(|m| m.focused());
        h.key_press(egui::Key::Escape);
        h.run();
        assert!(h.query_by_label("/help").is_none());
        assert_eq!(h.state().action, None);
        assert_eq!(h.ctx.memory(|m| m.focused()), focus);
    }

    #[test]
    fn esc_without_completions_emits_cancel_when_running() {
        let mut h = harness("draft");
        h.state_mut().phase = Some(workspace_ui::ThreadRunPhase::Running);
        h.get_by_label("Message or /command").focus();
        h.run();
        h.key_press(egui::Key::Escape);
        h.run();
        assert_eq!(h.state().action, Some(ComposerAction::Cancel));
        assert_eq!(h.state().model.input, "draft");
    }

    #[test]
    fn esc_when_idle_emits_nothing() {
        let mut h = harness("draft");
        h.get_by_label("Message or /command").focus();
        h.run();
        h.key_press(egui::Key::Escape);
        h.run();
        assert_eq!(h.state().action, None);
    }

    #[test]
    fn esc_dismisses_completions_before_cancel_even_when_running() {
        let mut h = harness("/");
        h.state_mut().phase = Some(workspace_ui::ThreadRunPhase::Running);
        h.get_by_label("Message or /command").focus();
        h.run();
        h.get_by_label("Send");
        h.key_press(egui::Key::Escape);
        h.run();
        assert_eq!(h.state().action, None);
        assert!(h.query_by_label("/help").is_none());
        h.key_press(egui::Key::Escape);
        h.run();
        assert_eq!(h.state().action, Some(ComposerAction::Cancel));
    }

    #[test]
    fn cancel_button_replaces_send_while_running() {
        let mut h = harness("");
        h.state_mut().phase = Some(workspace_ui::ThreadRunPhase::Running);
        h.run();
        assert!(h.query_by_label("Send").is_none());
        h.get_by_label("Cancel").click();
        h.run();
        assert_eq!(h.state().action, Some(ComposerAction::Cancel));
    }

    #[test]
    fn send_button_shown_while_waiting() {
        let mut h = harness("draft");
        h.state_mut().phase = Some(workspace_ui::ThreadRunPhase::Waiting);
        h.run();
        assert!(h.query_by_label("Cancel").is_none());
        h.get_by_label("Send").click();
        h.run();
        assert_eq!(h.state().action, Some(ComposerAction::Send));
    }

    #[test]
    fn typing_after_dismissal_reshows_completions() {
        let mut h = harness("/");
        h.get_by_label("Message or /command").focus();
        h.run();
        h.key_press(egui::Key::Escape);
        h.run();
        h.input_mut().events.push(egui::Event::Text("g".into()));
        h.run();
        h.get_by_label("/goal <text>");
        assert_eq!(h.state().action, None);
    }

    #[test]
    fn enter_still_sends_while_running() {
        let mut h = harness("queued");
        h.state_mut().phase = Some(workspace_ui::ThreadRunPhase::Running);
        h.get_by_label("Message or /command").focus();
        h.run();
        h.key_press(egui::Key::Enter);
        h.run();
        assert_eq!(h.state().action, Some(ComposerAction::Send));
    }

    #[test]
    fn send_button_disabled_on_empty_input() {
        // Given
        let mut harness = harness("");
        // When
        harness.run();
        // Then
        assert!(harness.get_by_label("Send").accesskit_node().is_disabled());
        assert_eq!(harness.state().action, None);
    }

    #[test]
    fn send_click_emits_send_action() {
        // Given
        let mut harness = harness("hi");
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
        let mut harness = harness("/");
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
        let mut harness = harness("");
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
        let mut harness = harness("/g");
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
    fn model_picker_is_rendered_inside_the_strip() {
        let mut harness = harness("");
        harness.run();
        harness.get_by_label("Select model");
    }

    #[test]
    fn send_button_disabled_on_whitespace_input() {
        // Given
        let mut harness = harness("  ");
        // When
        harness.run();
        // Then
        assert!(harness.get_by_label("Send").accesskit_node().is_disabled());
    }

    #[test]
    fn enter_with_focus_emits_send() {
        // Given
        let mut harness = harness("hi");
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
        let mut harness = harness("hi");
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
        let mut harness = harness("hi");
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
        let mut harness = harness("hi");
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
        let mut harness = harness("");
        // When
        harness.run();
        // Then
        let rect = harness.get_by_label("Message or /command").rect();
        assert!(rect.height() >= crate::theme::tokens::COMPOSER_MIN_HEIGHT - 2.0 * SP_2 - 1.0);
    }

    #[test]
    fn composer_caps_height_and_scrolls() {
        // Given
        let mut harness = harness(&"line\n".repeat(40));
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
