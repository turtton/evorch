use super::WorkbenchState;
use crate::model::tasks::AgentRunSource;

impl<S: AgentRunSource> WorkbenchState<S> {
    pub(super) fn settings_owns_input(&self) -> bool {
        self.settings.tab.is_some()
            || self.sandbox_settings.open
            || self.agent_features.open
            || self.self_improvement_settings.open
            || self.storage_settings.open
            || self.theme_settings.open
    }

    /// The model picker modal is up: every key belongs to it, not to the panes
    /// and shortcuts behind its backdrop.
    pub(super) fn picker_owns_input(&self) -> bool {
        self.model_picker.open
    }

    /// The thread search modal is up: like the model picker, it takes every key.
    pub(super) fn search_owns_input(&self) -> bool {
        self.thread_search.is_open()
    }

    /// Captures Tab for composer completion before egui derives focus and key state.
    ///
    /// Only a focused composer claims it, so other panes keep Tab.
    pub fn raw_input_hook(&mut self, raw_input: &mut egui::RawInput) {
        // Empty keyboard paste is an integration signal, never terminal data or
        // a paste into another editor. Consume it before every routing branch.
        let empty_paste = raw_input
            .events
            .iter()
            .any(|event| matches!(event, egui::Event::Paste(text) if text.is_empty()))
            && !raw_input
                .events
                .iter()
                .any(|event| matches!(event, egui::Event::Paste(text) if !text.is_empty()));
        raw_input
            .events
            .retain(|event| !matches!(event, egui::Event::Paste(text) if text.is_empty()));
        let composer = self.composer.focused
            && !self.terminals.focused
            && !self.settings_owns_input()
            && !self.picker_owns_input()
            && !self.search_owns_input();
        if composer && let Some(thread) = self.sidebar.active_thread.clone() {
            if empty_paste {
                self.attachment_work
                    .queued
                    .push((thread.clone(), crate::attachments::Intake::Clipboard));
            }
            if !raw_input.dropped_files.is_empty() {
                let files = std::mem::take(&mut raw_input.dropped_files)
                    .into_iter()
                    .map(|file| file.path().to_path_buf())
                    .collect();
                self.attachment_work
                    .queued
                    .push((thread, crate::attachments::Intake::Files(files)));
            }
        }
        if self.settings_owns_input() || self.picker_owns_input() || self.search_owns_input() {
            return;
        }
        if self.terminals.focused {
            self.capture_terminal_input(raw_input);
            return;
        }
        if !self.composer.focused {
            return;
        }
        raw_input.events.retain(|event| match event {
            egui::Event::Key {
                key: egui::Key::Tab,
                pressed,
                repeat,
                modifiers,
                ..
            } if modifiers.is_none() || modifiers.shift_only() => {
                if *pressed && !repeat {
                    self.composer.tab_presses.push(modifiers.shift);
                }
                false
            }
            egui::Event::Text(text) | egui::Event::Ime(egui::ImeEvent::Commit(text))
                if text == "\t" =>
            {
                false
            }
            _ => true,
        });
    }

    /// Routes keyboard input to the focused terminal before egui sees it.
    ///
    /// Tab, arrows and Escape would otherwise move or drop focus, and global shortcuts
    /// would steal control keys the shell needs. Workbench shortcuts that a terminal has no
    /// use for (see [`crate::keymap::Keymap::passes_through_terminal`]) still reach the keymap.
    ///
    /// A button press outside the terminal grid hands input ownership back: it and every
    /// later event of the batch stay with egui (so Ctrl+C right after it copies a selection),
    /// while earlier events still belong to the terminal.
    fn capture_terminal_input(&mut self, raw_input: &mut egui::RawInput) {
        let keymap = &self.keymap;
        let pending = &mut self.terminals.pending_events;
        let grid = self.terminals.grid_rect;
        let mut released = false;
        raw_input.events.retain(|event| {
            if let egui::Event::PointerButton {
                pos, pressed: true, ..
            } = event
                && grid.is_some_and(|grid| !grid.contains(*pos))
            {
                released = true;
            }
            if released {
                return true;
            }
            let captured = match event {
                egui::Event::Key { key, modifiers, .. } => {
                    !keymap.passes_through_terminal(*key, *modifiers)
                }
                egui::Event::Text(_)
                | egui::Event::Paste(_)
                | egui::Event::Copy
                | egui::Event::Cut
                | egui::Event::Ime(_) => true,
                _ => false,
            };
            if captured {
                pending.push(event.clone());
            }
            !captured
        });
    }
}

#[cfg(test)]
mod tests {
    use egui::{Event, Modifiers, PointerButton, Pos2, Rect};
    use workspace_ui::UiSettings;

    use super::*;
    use crate::fixture::DemoSource;

    const INSIDE: Pos2 = Pos2::new(150.0, 150.0);
    const OUTSIDE: Pos2 = Pos2::new(10.0, 10.0);

    fn focused_terminal() -> WorkbenchState<DemoSource> {
        let mut state = WorkbenchState::new(DemoSource(vec![]), &UiSettings::default()).unwrap();
        state.terminals.focused = true;
        state.terminals.grid_rect = Some(Rect::from_min_max(
            Pos2::new(100.0, 100.0),
            Pos2::new(300.0, 300.0),
        ));
        state
    }

    #[test]
    fn image_only_paste_signal_never_reaches_a_focused_terminal() {
        let mut state = focused_terminal();
        assert!(route(&mut state, vec![Event::Paste(String::new())]).is_empty());
        assert!(state.terminals.pending_events.is_empty());
        assert!(state.attachment_work.queued.is_empty());
    }

    #[test]
    fn nonempty_text_paste_suppresses_a_batched_image_fallback() {
        let mut state = focused_terminal();
        state.terminals.focused = false;
        state.composer.focused = true;
        state.sidebar.active_thread = Some(workspace_ui::ThreadId::new("thread"));
        let text = Event::Paste("ordinary text".into());
        assert_eq!(
            route(&mut state, vec![Event::Paste(String::new()), text.clone()]),
            [text]
        );
        assert!(state.attachment_work.queued.is_empty());
    }

    fn press(pos: Pos2) -> Event {
        Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed: true,
            modifiers: Modifiers::NONE,
        }
    }

    fn route(state: &mut WorkbenchState<DemoSource>, events: Vec<Event>) -> Vec<Event> {
        let mut raw = egui::RawInput {
            events,
            ..egui::RawInput::default()
        };
        state.raw_input_hook(&mut raw);
        raw.events
    }

    // Given: a focused terminal / When: a press outside the grid and Ctrl+C share one batch / Then: the copy stays with egui instead of being swallowed.
    #[test]
    fn a_press_outside_hands_the_rest_of_the_batch_back_to_egui() {
        let mut state = focused_terminal();

        let remaining = route(
            &mut state,
            vec![
                Event::Text("a".into()),
                press(OUTSIDE),
                Event::Copy,
                Event::Text("b".into()),
            ],
        );

        assert_eq!(
            state.terminals.pending_events,
            [Event::Text("a".into())],
            "input typed before the press still belongs to the terminal"
        );
        assert_eq!(
            remaining,
            [press(OUTSIDE), Event::Copy, Event::Text("b".into())]
        );
    }

    // Given: a focused terminal / When: the press lands inside the grid / Then: the terminal keeps owning the whole batch.
    #[test]
    fn a_press_inside_keeps_the_batch_with_the_terminal() {
        let mut state = focused_terminal();

        let remaining = route(&mut state, vec![press(INSIDE), Event::Copy]);

        assert_eq!(state.terminals.pending_events, [Event::Copy]);
        assert_eq!(remaining, [press(INSIDE)]);
    }

    // Given: label text selected outside a focused terminal / When: a press outside the grid and Ctrl+C share one input batch / Then: the selection is copied, not swallowed with the terminal's events.
    #[test]
    fn copy_batched_with_an_outside_press_still_copies_the_selection() {
        const LABEL: &str = "selected words";
        let mut state = focused_terminal();
        state.terminals.focused = false;
        let ctx = egui::Context::default();
        let label_rect = std::cell::Cell::new(Rect::NOTHING);
        let frame = |state: &mut WorkbenchState<DemoSource>, events: Vec<Event>| {
            let mut raw = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(400.0, 400.0))),
                time: Some(0.1),
                events,
                ..egui::RawInput::default()
            };
            state.raw_input_hook(&mut raw);
            let mut output = ctx.run_ui(raw, |ui| {
                label_rect.set(ui.label(LABEL).rect);
            });
            output.textures_delta.clear();
            output
                .platform_output
                .commands
                .iter()
                .filter_map(|command| match command {
                    egui::OutputCommand::CopyText(text) => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        for _ in 0..4 {
            frame(&mut state, Vec::new());
        }
        let rect = label_rect.get();
        let (start, end) = (
            Pos2::new(rect.left() + 1.0, rect.center().y),
            Pos2::new(rect.right() - 1.0, rect.center().y),
        );
        let button = |pos, pressed| Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        };
        frame(&mut state, vec![Event::PointerMoved(start)]);
        frame(&mut state, vec![button(start, true)]);
        frame(&mut state, vec![Event::PointerMoved(end)]);
        frame(&mut state, vec![button(end, false)]);
        assert!(
            ctx.with_plugin(
                |selection: &mut egui::text_selection::LabelSelectionState| {
                    selection.has_selection()
                }
            )
            .unwrap(),
            "dragging selected the label text"
        );

        state.terminals.focused = true;
        let copied = frame(
            &mut state,
            vec![Event::PointerMoved(end), button(end, true), Event::Copy],
        );

        assert_eq!(copied, [LABEL]);
        assert!(state.terminals.pending_events.is_empty());
    }
}
