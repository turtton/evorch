use super::WorkbenchState;
use crate::model::tasks::AgentRunSource;

impl<S: AgentRunSource> WorkbenchState<S> {
    pub(super) fn settings_owns_input(&self) -> bool {
        self.provider_settings.open
            || self.routing_settings.open
            || self.role_settings.open
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
    fn capture_terminal_input(&mut self, raw_input: &mut egui::RawInput) {
        let keymap = &self.keymap;
        let pending = &mut self.terminals.pending_events;
        raw_input.events.retain(|event| {
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
