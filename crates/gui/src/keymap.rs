//! workspace-ui の KeyChord を egui の入力状態へ解決します。

use std::collections::BTreeMap;

use workspace_ui::{KeyAction, KeybindSettings};

#[derive(Debug, thiserror::Error)]
pub enum PanelKeybindError {
    #[error("unknown panel key action: {0}")]
    Action(String),
    #[error("invalid panel key chord: {0}")]
    Chord(String),
}

pub fn panel_keybinds(
    base: &KeybindSettings,
    panel: &config::PanelConfig,
) -> Result<KeybindSettings, PanelKeybindError> {
    let mut settings = base.clone();
    for (name, value) in &panel.keybinds {
        let action = match name.as_str() {
            "focus_agent_pane" => KeyAction::FocusAgentPane,
            "focus_terminal_pane" => KeyAction::FocusTerminalPane,
            "focus_tasks_pane" => KeyAction::FocusTasksPane,
            "save_layout" => KeyAction::SaveLayout,
            "reset_layout" => KeyAction::ResetLayout,
            _ => return Err(PanelKeybindError::Action(name.clone())),
        };
        let chord: workspace_ui::KeyChord = value
            .parse()
            .map_err(|_| PanelKeybindError::Chord(value.clone()))?;
        if egui::Key::from_name(&chord.key).is_none() {
            return Err(PanelKeybindError::Chord(value.clone()));
        }
        settings.bindings.insert(action, chord);
    }
    Ok(settings)
}

/// 設定されたキーバインドを egui 入力に解決するマッパーです。
#[derive(Debug, Clone)]
pub struct Keymap {
    bindings: BTreeMap<KeyAction, ResolvedKey>,
}

#[derive(Debug, Clone, Copy)]
struct ResolvedKey {
    key: egui::Key,
    ctrl: bool,
    shift: bool,
    alt: bool,
}

impl Keymap {
    /// 設定からキーマップを構築します。解決不能なキーは無視されます。
    pub fn from_settings(settings: &KeybindSettings) -> Self {
        let bindings = settings
            .bindings
            .iter()
            .filter_map(|(action, chord)| {
                let key = egui::Key::from_name(&chord.key)?;
                Some((
                    *action,
                    ResolvedKey {
                        key,
                        ctrl: chord.ctrl,
                        shift: chord.shift,
                        alt: chord.alt,
                    },
                ))
            })
            .collect();
        Self { bindings }
    }

    /// 現在の egui 入力状態に対応するアクションを返します。
    pub fn action_for_input(&self, input: &egui::InputState) -> Option<KeyAction> {
        for (action, resolved) in &self.bindings {
            if input.key_pressed(resolved.key)
                && input.modifiers.command == resolved.ctrl
                && input.modifiers.shift == resolved.shift
                && input.modifiers.alt == resolved.alt
            {
                return Some(*action);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use egui::{Key, Modifiers};
    use workspace_ui::{KeyAction, KeybindSettings};

    use super::Keymap;

    #[test]
    fn panel_overrides_preserve_unconfigured_bindings() {
        // Given: a panel override and saved UI defaults.
        let panel = config::PanelConfig {
            keybinds: [("save_layout".into(), "Alt+S".into())].into(),
            ..Default::default()
        };
        // When: resolve the panel map.
        let resolved = super::panel_keybinds(&KeybindSettings::default(), &panel).expect("keys");
        // Then: configured actions override while other bindings survive.
        assert_eq!(
            resolved.bindings[&KeyAction::SaveLayout].to_string(),
            "Alt+S"
        );
        assert_eq!(
            resolved.bindings[&KeyAction::FocusAgentPane].to_string(),
            "Ctrl+1"
        );
    }

    #[test]
    fn panel_rejects_unknown_actions_and_keys() {
        // Given: invalid action or key names.
        for (action, key) in [("unknown", "Ctrl+S"), ("save_layout", "Ctrl+Unknown")] {
            let panel = config::PanelConfig {
                keybinds: [(action.into(), key.into())].into(),
                ..Default::default()
            };
            // When / Then: no silently ignored binding is accepted.
            assert!(super::panel_keybinds(&KeybindSettings::default(), &panel).is_err());
        }
    }

    fn run_with_key(key: Key, modifiers: Modifiers) -> egui::Context {
        let ctx = egui::Context::default();
        let mut raw_input = egui::RawInput::default();
        raw_input
            .events
            .push(egui::Event::ModifiersChanged(modifiers));
        raw_input.events.push(egui::Event::Key {
            key,
            pressed: true,
            modifiers,
            repeat: false,
            physical_key: None,
        });
        let mut output = ctx.run_ui(raw_input, |_ui| {});
        output.textures_delta.clear();
        ctx
    }

    #[test]
    fn default_keybinds_resolve_focus_actions() {
        // Given: default keybind settings
        let keymap = Keymap::from_settings(&KeybindSettings::default());

        // When / Then: each focus action resolves with the expected chord
        let ctx = run_with_key(Key::Num1, Modifiers::COMMAND);
        assert_eq!(
            keymap.action_for_input(&ctx.input(|i| i.clone())),
            Some(KeyAction::FocusAgentPane)
        );

        let ctx = run_with_key(Key::Num2, Modifiers::COMMAND);
        assert_eq!(
            keymap.action_for_input(&ctx.input(|i| i.clone())),
            Some(KeyAction::FocusTerminalPane)
        );

        let ctx = run_with_key(Key::Num3, Modifiers::COMMAND);
        assert_eq!(
            keymap.action_for_input(&ctx.input(|i| i.clone())),
            Some(KeyAction::FocusTasksPane)
        );
    }

    #[test]
    fn default_keybinds_resolve_save_and_reset() {
        let keymap = Keymap::from_settings(&KeybindSettings::default());

        let ctx = run_with_key(Key::S, Modifiers::COMMAND);
        assert_eq!(
            keymap.action_for_input(&ctx.input(|i| i.clone())),
            Some(KeyAction::SaveLayout)
        );

        let ctx = run_with_key(Key::R, Modifiers::COMMAND | Modifiers::SHIFT);
        assert_eq!(
            keymap.action_for_input(&ctx.input(|i| i.clone())),
            Some(KeyAction::ResetLayout)
        );
    }

    #[test]
    fn unmatched_input_returns_none() {
        let keymap = Keymap::from_settings(&KeybindSettings::default());

        let ctx = run_with_key(Key::Num4, Modifiers::COMMAND);
        assert_eq!(keymap.action_for_input(&ctx.input(|i| i.clone())), None);
    }
}
