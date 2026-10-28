//! Composer strip rendering and action reporting without dispatch.

use crate::model::composer::{CompletionItem, CompletionKind, ComposerModel};
use crate::theme::icons;
use crate::theme::tokens::{
    COMPOSER_MAX_HEIGHT, COMPOSER_MIN_HEIGHT, R_2XL, ROW_COMPACT, SP_1, SP_2, SP_3, palette,
};
use crate::theme::widgets::{
    accessible, ghost, primary_button, primary_icon_button, surface_frame,
};
use workspace_ui::ThreadRunPhase;

#[path = "composer_images.rs"]
mod images;
mod selectors;
pub(crate) use selectors::sandbox_mode;

#[derive(Clone, Copy, Default)]
pub struct SandboxPickerContext {
    pub mode: config::EscalationApproval,
    pub escalate_to_user_on_deny: bool,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposerAction {
    Send,
    Stop,
    DeliverNextTurn,
    /// Withdraw a live queued input, restoring it only for Edit.
    EditQueued(usize),
    DeleteQueued(usize),
    /// Put an undelivered queued follow-up back into the input.
    RestoreQueued(usize),
    /// Drop an undelivered queued follow-up.
    DismissQueued(usize),
    Complete(CompletionItem),
    ToggleRole,
    ModelPreference(Option<workspace_ui::ModelPreference>),
    OpenSandboxSettings,
}

/// What the strip reports to the pane that sizes it.
pub struct ComposerStripOutput {
    pub action: Option<ComposerAction>,
    /// Height the input viewport occupied this frame.
    pub input_viewport: f32,
    /// Height the input wants to show every wrapped row, capped at
    /// [`COMPOSER_MAX_HEIGHT`]; it can exceed `input_viewport` when the pane
    /// granted less room.
    pub input_wanted: f32,
}

pub fn composer_strip(
    ui: &mut egui::Ui,
    model: &mut ComposerModel,
    picker: crate::panes::model_picker::ModelPickerContext<'_>,
    picker_state: &mut crate::model::model_picker::ModelPickerState,
    phase: Option<ThreadRunPhase>,
    sandbox: SandboxPickerContext,
) -> Option<ComposerAction> {
    composer_strip_within(
        ui,
        model,
        (picker, picker_state),
        phase,
        sandbox,
        COMPOSER_MAX_HEIGHT,
    )
    .action
}

/// [`composer_strip`] whose input viewport grows with wrapped rows up to
/// `max_input` (itself capped at [`COMPOSER_MAX_HEIGHT`]) and scrolls beyond.
pub fn composer_strip_within(
    ui: &mut egui::Ui,
    model: &mut ComposerModel,
    (picker, picker_state): (
        crate::panes::model_picker::ModelPickerContext<'_>,
        &mut crate::model::model_picker::ModelPickerState,
    ),
    phase: Option<ThreadRunPhase>,
    sandbox: SandboxPickerContext,
    max_input: f32,
) -> ComposerStripOutput {
    let mut action = None;
    // While the model picker is up its keys are not the composer's; Escape in
    // particular must not stop a running turn.
    let picker_was_open = picker_state.open;
    let input_id = ui.id().with("composer-input");
    let min_input = COMPOSER_MIN_HEIGHT - 2.0 * SP_2;
    // Wrapped rows are only known after layout, so the viewport follows the
    // last measured document height and the pass is redone when it changes.
    let measured_id = input_id.with("wanted-height");
    let wanted = ui
        .data(|data| data.get_temp::<f32>(measured_id))
        .unwrap_or_else(|| {
            let rows = model.input.split('\n').count().max(1);
            rows as f32 * ui.text_style_height(&egui::TextStyle::Body) + 2.0 * SP_2
        });
    let wanted = wanted.clamp(min_input, COMPOSER_MAX_HEIGHT);
    let viewport = wanted.min(max_input.clamp(min_input, COMPOSER_MAX_HEIGHT));
    let mut measured = wanted;
    surface_frame(palette().SURFACE_RAISED)
        .corner_radius(R_2XL)
        .inner_margin(egui::vec2(SP_3, SP_2))
        .show(ui, |ui| {
        ui.vertical(|ui| {
            ui.set_min_height(ROW_COMPACT);
            ui.spacing_mut().item_spacing = egui::vec2(SP_2, SP_1);
            let ime_id = ui.id().with("ime-composing");
            let ime_composing = ime_composing(ui, ime_id);
            let items = model.completion_items();
            model.completion_selected = model.completion_selected.min(items.len().saturating_sub(1));
            // The workbench clears `model.focused` before rendering; egui keeps the real focus.
            let focused = ui.memory(|memory| memory.has_focus(input_id));
            if let Some(item) = completion_keys(ui, model, &items, ime_composing, focused) {
                action = Some(ComposerAction::Complete(item));
            }
            if let Some(item) = completion_list(ui, &items, model.completion_selected) {
                action = Some(ComposerAction::Complete(item));
            }
            if let Some(selected) = selectors::row(ui, sandbox, (model.role, model.role_locked), (picker, picker_state)) {
                action = Some(selected);
            }
            if model.images_needed > 0 {
                ui.label(egui::RichText::new(format!(
                    "Re-attach {}",
                    if model.images_needed == 1 { "1 image".to_owned() } else { format!("{} images", model.images_needed) }
                )).color(palette().WARNING_FG));
            }
            images::render(ui, model);
            ui.horizontal(|ui| { ui.with_layout(egui::Layout::right_to_left(egui::Align::BOTTOM), |ui| {
                let can_send = !model.input.trim().is_empty() || !model.attachments.is_empty();
                let can_stop = (phase == Some(ThreadRunPhase::Running) && items.is_empty())
                    || (phase == Some(ThreadRunPhase::Stopped) && model.running_children > 0);
                let queued_send = if can_stop && can_send && phase == Some(ThreadRunPhase::Running) {
                    Some(primary_button(ui, "Queue").on_hover_text("Queue (Enter)"))
                } else { None };
                let send = if can_stop {
                    stop_button(ui, phase == Some(ThreadRunPhase::Stopped), model.running_children)
                } else if can_send {
                    let send = primary_icon_button(ui, icons::PAPER_PLANE_RIGHT, "Send");
                    if phase == Some(ThreadRunPhase::Stopped) {
                        send.on_hover_text(crate::theme::tokens::STOPPED_HINT)
                    } else {
                        send
                    }
                } else {
                    let send = ui.add_enabled(false, ghost(icons::with_icon(icons::PAPER_PLANE_RIGHT, "Send")));
                    accessible(&send, "Send");
                    send
                };
                let input = egui::ScrollArea::vertical()
                    .id_salt("composer-scroll")
                    .min_scrolled_width(0.0)
                    .min_scrolled_height(viewport)
                    .max_height(viewport)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.with_layout(egui::Layout::top_down(egui::Align::LEFT), |ui| {
                        if let Some(cursor) = model.pending_cursor.take() {
                            let mut state = egui::text_edit::TextEditState::load(ui.ctx(), input_id).unwrap_or_default();
                            state.cursor.set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(cursor))));
                            state.store(ui.ctx(), input_id);
                        }
                        ui.add(egui::TextEdit::multiline(&mut model.input)
                            .id(input_id)
                            .hint_text("Message, /command or @file  (Shift+Enter: newline)")
                            // raw_input_hook captures Tab for completion while focused;
                            // this keeps any leaked Tab from moving focus.
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
                measured = input.rect.height().clamp(min_input, COMPOSER_MAX_HEIGHT);
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
                if std::mem::take(&mut model.focus_requested) {
                    input.request_focus();
                }
                model.focused = input.has_focus();
                model.cursor = egui::text_edit::TextEditState::load(ui.ctx(), input_id)
                    .and_then(|state| state.cursor.char_range())
                    .map(|range| range.primary.index.0);
                ui.data_mut(|data| data.insert_temp(ime_id, ime_composing && model.focused));
                if !picker_was_open && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    if !items.is_empty() {
                        model.dismiss_completions();
                    } else if can_stop {
                        action = Some(ComposerAction::Stop);
                    }
                    input.request_focus();
                } else if can_stop && send.clicked() {
                    action = Some(ComposerAction::Stop);
                    input.request_focus();
                } else if can_send && ((!can_stop && send.clicked()) || queued_send.is_some_and(|button| button.clicked()) || enter) {
                    action = Some(ComposerAction::Send);
                    input.request_focus();
                }
            }); });
        });
    });
    if (measured - wanted).abs() > 0.5 {
        ui.data_mut(|data| data.insert_temp(measured_id, measured));
        ui.ctx().request_discard("composer input rows changed");
        ui.ctx().request_repaint();
    }
    ComposerStripOutput {
        action,
        input_viewport: viewport,
        input_wanted: measured,
    }
}

/// Stop halts only the current run; once it is stopped with subagents still
/// running, the same button becomes Stop subagents and halts the whole subtree.
fn stop_button(ui: &mut egui::Ui, stopped: bool, running_children: usize) -> egui::Response {
    let (label, color, fill, hover) = if stopped {
        (
            "Stop subagents",
            palette().WARNING_FG,
            palette().SURFACE_RAISED,
            format!(
                "Stop {} (Esc)",
                if running_children == 1 {
                    "the running subagent".to_owned()
                } else {
                    format!("all {running_children} running subagents")
                }
            ),
        )
    } else {
        (
            "Stop",
            palette().ERROR_FG,
            palette().ERROR_SURFACE,
            "Stop the run (Esc). Its subagents keep running until stopped separately.".to_owned(),
        )
    };
    ui.add(egui::Button::new(egui::RichText::new(label).color(color)).fill(fill))
        .on_hover_text(hover)
}

/// Tracks IME preedit across frames so Enter can confirm a conversion without sending.
fn ime_composing(ui: &egui::Ui, id: egui::Id) -> bool {
    let mut composing = ui.data(|data| data.get_temp::<bool>(id).unwrap_or_default());
    ui.input(|input| {
        for event in &input.events {
            if let egui::Event::Ime(event) = event {
                #[allow(deprecated)] // Accept legacy backend Enabled/Disabled events too.
                match event {
                    egui::ImeEvent::Preedit { text, .. } => composing = !text.is_empty(),
                    egui::ImeEvent::Commit(_) | egui::ImeEvent::Disabled => composing = false,
                    egui::ImeEvent::Enabled | egui::ImeEvent::DeleteSurrounding { .. } => {}
                }
            }
        }
    });
    composing
}

/// Tab / Shift+Tab and arrows cycle the highlight, Tab accepts a sole
/// candidate, and Enter accepts only when that changes the draft, so a fully
/// typed `/new` still sends.
fn completion_keys(
    ui: &mut egui::Ui,
    model: &mut ComposerModel,
    items: &[CompletionItem],
    ime_composing: bool,
    focused: bool,
) -> Option<CompletionItem> {
    let tabs = std::mem::take(&mut model.tab_presses);
    if items.is_empty() || ime_composing {
        return None;
    }
    let len = items.len();
    let mut selected = model.completion_selected;
    let mut accept = len == 1 && tabs.iter().any(|shift| !shift);
    if len > 1 {
        for shift in tabs {
            selected = if shift {
                (selected + len - 1) % len
            } else {
                (selected + 1) % len
            };
        }
    }
    if focused && !accept {
        ui.input_mut(|input| {
            if input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown) {
                selected = (selected + 1) % len;
            }
            if input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp) {
                selected = (selected + len - 1) % len;
            }
            accept = items[selected].changes(&model.input)
                && input.consume_key(egui::Modifiers::NONE, egui::Key::Enter);
        });
    }
    model.completion_selected = selected;
    accept.then(|| items[selected].clone())
}

fn completion_list(
    ui: &mut egui::Ui,
    items: &[CompletionItem],
    selected: usize,
) -> Option<CompletionItem> {
    let mut picked = None;
    for (index, item) in items.iter().enumerate() {
        ui.push_id(("completion", index), |ui| {
            ui.horizontal(|ui| {
                let icon = match item.kind {
                    CompletionKind::Command => icons::TERMINAL_WINDOW,
                    CompletionKind::File => icons::FILE,
                    CompletionKind::Dir => icons::FOLDER,
                    CompletionKind::Skill => icons::SPARKLE,
                };
                ui.label(egui::RichText::new(icon).color(palette().TEXT_MUTED));
                // The theme's selection fill hides default text; keep the label legible.
                let response = ui.selectable_label(
                    index == selected,
                    egui::RichText::new(&item.label).color(palette().TEXT),
                );
                if index == selected {
                    response.scroll_to_me(None);
                }
                if response.clicked() {
                    picked = Some(item.clone());
                }
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(&item.detail)
                            .small()
                            .color(palette().TEXT_MUTED),
                    )
                    .truncate(),
                );
            });
        });
    }
    picked
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
        profiles: Vec<runtime::compose::ProfileSummary>,
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
                        profiles: &state.profiles,
                        preference: None,
                        default_model: None,
                        enabled: !state.profiles.is_empty(),
                    },
                    &mut state.picker_state,
                    state.phase,
                    SandboxPickerContext::default(),
                ) {
                    state.action = Some(action);
                }
            },
            Fixture {
                profiles: Vec::new(),
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
    fn esc_without_completions_emits_stop_when_running() {
        let mut h = harness("draft");
        h.state_mut().phase = Some(workspace_ui::ThreadRunPhase::Running);
        h.get_by_label("Message or /command").focus();
        h.run();
        h.key_press(egui::Key::Escape);
        h.run();
        assert_eq!(h.state().action, Some(ComposerAction::Stop));
        assert_eq!(h.state().model.input, "draft");
    }

    #[test]
    fn esc_while_the_model_picker_is_open_never_stops_a_running_turn() {
        use crate::panes::model_picker::{EFFORT_HEADING, SEARCH_LABEL};
        // Given: a running turn and a picker whose only model offers effort levels.
        let mut h = harness("draft");
        h.state_mut().phase = Some(workspace_ui::ThreadRunPhase::Running);
        h.state_mut().profiles = vec![runtime::compose::ProfileSummary {
            name: "p".into(),
            provider_type: model::ProviderType::OpenAiCompatible,
            models: vec!["m".into()],
            default_model: Some("m".into()),
            effort_levels: [("m".to_owned(), vec!["low".to_owned(), "high".to_owned()])].into(),
        }];
        h.run();
        let open = |h: &mut Harness<'static, Fixture>| {
            h.get_by_label("Select model").click();
            h.run();
            assert!(h.query_by_label(SEARCH_LABEL).is_some());
        };
        let escape = |h: &mut Harness<'static, Fixture>| {
            h.key_press(egui::Key::Escape);
            h.run();
        };
        // When/Then: Escape in the model list only closes the picker.
        open(&mut h);
        escape(&mut h);
        assert!(!h.state().picker_state.open);
        assert_eq!(h.state().action, None);
        // When/Then: in the effort step it goes back to the list, then closes it.
        open(&mut h);
        h.get_by_label("p / m").click();
        h.run();
        assert!(h.query_by_label(EFFORT_HEADING).is_some());
        escape(&mut h);
        assert!(h.query_by_label(SEARCH_LABEL).is_some());
        assert_eq!(h.state().action, None);
        escape(&mut h);
        assert!(!h.state().picker_state.open);
        assert_eq!(h.state().action, None);
        // Then: with the picker closed, Escape stops the turn again.
        escape(&mut h);
        assert_eq!(h.state().action, Some(ComposerAction::Stop));
    }

    // Given: a stopped run / When: it is drawn / Then: no banner appears, and the
    // subtree stop button is offered only while subagents are still running.
    #[test]
    fn stopped_run_shows_no_banner_and_keeps_subtree_stop_until_children_settle() {
        let mut h = harness("draft");
        h.state_mut().phase = Some(ThreadRunPhase::Stopped);
        h.state_mut().model.running_children = 2;
        h.run();
        assert!(h.query_by_label_contains("still running").is_none());
        assert!(h.query_by_label_contains("resume").is_none());
        h.get_by_label("Stop subagents").click();
        h.run();
        assert_eq!(h.state().action, Some(ComposerAction::Stop));
        assert_eq!(h.state().model.input, "draft");
        h.state_mut().model.running_children = 0;
        h.state_mut().action = None;
        h.run();
        assert!(h.query_by_label_contains("Stopped").is_none());
        assert!(h.query_by_label("Stop subagents").is_none());
        assert!(h.query_by_label("Discard").is_none());
        h.get_by_label("Send").click();
        h.run();
        assert_eq!(h.state().action, Some(ComposerAction::Send));
    }

    // Given: a stopped run without running subagents / When: Send is hovered
    // Then: the hover tells how to resume, though no banner does
    #[test]
    fn send_hover_explains_resuming_a_stopped_run() {
        let mut h = harness("draft");
        h.state_mut().phase = Some(ThreadRunPhase::Stopped);
        h.run();
        h.get_by_label("Send").hover();
        h.run();
        h.get_by_label(crate::theme::tokens::STOPPED_HINT);
    }

    #[test]
    fn stop_tooltips_name_what_each_press_stops() {
        let mut h = harness("");
        h.state_mut().phase = Some(ThreadRunPhase::Running);
        h.run();
        h.get_by_label("Stop").hover();
        h.run();
        h.get_by_label("Stop the run (Esc). Its subagents keep running until stopped separately.");
        h.state_mut().phase = Some(ThreadRunPhase::Stopped);
        h.state_mut().model.running_children = 3;
        h.run();
        h.get_by_label("Stop subagents").hover();
        h.run();
        h.get_by_label("Stop all 3 running subagents (Esc)");
    }

    #[test]
    fn queue_tooltip_names_only_the_shortcut() {
        let mut h = harness("draft");
        h.state_mut().phase = Some(ThreadRunPhase::Running);
        h.state_mut().model.follow_ups.pending = 2;
        h.run();
        h.get_by_label("Queue").hover();
        h.run();
        h.get_by_label("Queue (Enter)");
        assert!(h.query_by_label_contains("queued").is_none());
        assert!(h.query_by_label("Deliver next turn").is_none());
    }

    #[test]
    fn running_composer_omits_queue_explanation() {
        for input in ["", "follow-up draft"] {
            let mut h = harness(input);
            h.state_mut().phase = Some(ThreadRunPhase::Running);
            h.run();
            assert!(h.query_by_label_contains("queued").is_none());
            h.get_by_label("Stop");
        }
    }

    #[test]
    fn running_draft_has_queue_and_stop_without_ambiguous_send() {
        let mut h = harness("follow-up draft");
        h.state_mut().phase = Some(ThreadRunPhase::Running);
        h.run();
        h.get_by_label("Stop");
        assert!(h.query_by_label("Send").is_none());
        h.get_by_label("Queue").click();
        h.run();
        assert_eq!(h.state().action, Some(ComposerAction::Send));
    }

    #[test]
    fn escape_stops_children_and_is_inert_once_everything_stopped() {
        let mut h = harness("draft");
        h.state_mut().phase = Some(ThreadRunPhase::Stopped);
        h.state_mut().model.running_children = 1;
        h.get_by_label("Message or /command").focus();
        h.run();
        h.key_press(egui::Key::Escape);
        h.run();
        assert_eq!(h.state().action, Some(ComposerAction::Stop));
        h.state_mut().model.running_children = 0;
        h.state_mut().action = None;
        h.key_press(egui::Key::Escape);
        h.run();
        assert_eq!(h.state().action, None);
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
    fn esc_dismisses_completions_before_stop_even_when_running() {
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
        assert_eq!(h.state().action, Some(ComposerAction::Stop));
    }

    #[test]
    fn stop_button_replaces_send_while_running() {
        let mut h = harness("");
        h.state_mut().phase = Some(workspace_ui::ThreadRunPhase::Running);
        h.run();
        assert!(h.query_by_label("Send").is_none());
        h.get_by_label("Stop").click();
        h.run();
        assert_eq!(h.state().action, Some(ComposerAction::Stop));
    }

    #[test]
    fn send_button_shown_while_waiting() {
        let mut h = harness("draft");
        h.state_mut().phase = Some(workspace_ui::ThreadRunPhase::Waiting);
        h.run();
        assert!(h.query_by_label("Stop").is_none());
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
        assert!(matches!(&harness.state().action,
            Some(ComposerAction::Complete(item)) if item.replacement == "/goal "));
        assert_eq!(harness.state().model.input, "/");
    }

    fn mention_harness(input: &str, files: &[&str]) -> Harness<'static, Fixture> {
        let mut h = harness(input);
        let files: Vec<_> = files.iter().map(|file| (*file).to_owned()).collect();
        h.state_mut().model.mentions =
            crate::model::composer::MentionIndex::from_parts(None, &files, []);
        h
    }

    fn accept(h: &mut Harness<'static, Fixture>) {
        if let Some(ComposerAction::Complete(item)) = h.state_mut().action.take() {
            h.state_mut().model.apply_completion(&item);
        }
        h.run();
    }

    #[test]
    fn tab_cycles_completions_forward_and_shift_tab_backward() {
        let mut h = harness("/");
        h.get_by_label("Message or /command").focus();
        h.run();
        let last = crate::model::composer::SLASH_COMMANDS.len() - 1;
        // Tab moves forward and wraps; the draft stays as typed while cycling.
        for expected in (1..=last).chain([0]) {
            h.state_mut().model.tab_presses = vec![false];
            h.run();
            assert_eq!(h.state().model.completion_selected, expected);
        }
        assert_eq!(h.state().action, None);
        assert_eq!(h.state().model.input, "/");
        // Shift+Tab moves backward and wraps to the last candidate.
        h.state_mut().model.tab_presses = vec![true];
        h.run();
        assert_eq!(h.state().model.completion_selected, last);
        assert_eq!(h.state().action, None);
        h.key_press(egui::Key::Enter);
        h.run();
        accept(&mut h);
        assert_eq!(h.state().model.input, "/run ");
        assert!(h.query_by_label("/team").is_none());
        // Without candidates Tab is a no-op that keeps the draft.
        h.state_mut().model.tab_presses = vec![false];
        h.run();
        assert_eq!(h.state().action, None);
        assert_eq!(h.state().model.input, "/run ");
    }

    #[test]
    fn tab_accepts_a_sole_completion() {
        let mut h = harness("/ru");
        h.get_by_label("Message or /command").focus();
        h.run();
        h.state_mut().model.tab_presses = vec![true];
        h.run();
        assert_eq!(h.state().action, None);
        h.state_mut().model.tab_presses = vec![false];
        h.run();
        accept(&mut h);
        assert_eq!(h.state().model.input, "/run ");
    }

    #[test]
    fn arrows_and_enter_complete_mentions_but_exact_tokens_still_send() {
        let mut h = mention_harness("open @", &["src/lib.rs", "README.md"]);
        h.get_by_label("Message or /command").focus();
        h.run();
        h.get_by_label("@README.md");
        h.key_press(egui::Key::ArrowDown);
        h.run();
        h.key_press(egui::Key::Enter);
        h.run();
        accept(&mut h);
        assert_eq!(h.state().model.input, "open @src/");
        h.get_by_label("@src/lib.rs");
        h.key_press(egui::Key::Enter);
        h.run();
        accept(&mut h);
        assert_eq!(h.state().model.input, "open @src/lib.rs ");
        // The finished token no longer offers completions, so Enter sends.
        h.key_press(egui::Key::Enter);
        h.run();
        assert_eq!(h.state().action, Some(ComposerAction::Send));
    }

    #[test]
    fn role_button_toggles_unless_the_thread_locked_it() {
        let mut h = harness("");
        h.run();
        h.get_by_label("Role: worker").click();
        h.run();
        assert_eq!(h.state().action, Some(ComposerAction::ToggleRole));
        h.state_mut().action = None;
        h.state_mut().model.role_locked = true;
        h.run();
        assert!(
            h.get_by_label("Role: worker")
                .accesskit_node()
                .is_disabled()
        );
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
