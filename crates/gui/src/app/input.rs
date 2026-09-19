use super::WorkbenchState;
use crate::model::tasks::AgentRunSource;

impl<S: AgentRunSource> WorkbenchState<S> {
    pub(super) fn settings_owns_input(&self) -> bool {
        self.provider_settings.open
            || self.routing_settings.open
            || self.role_settings.open
            || self.sandbox_settings.open
            || self.theme_settings.open
    }

    /// Filters physical role shortcuts before egui derives focus and key state.
    pub fn raw_input_hook(&mut self, raw_input: &mut egui::RawInput) {
        if self.settings_owns_input() {
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
                    self.pending_role_toggles += 1;
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
}
