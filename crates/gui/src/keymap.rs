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
            "cycle_agent_role" => KeyAction::CycleAgentRole,
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
    ///
    /// 修飾なし Tab は composer の補完専用のため、旧既定の role 切替割当は無効化します。
    pub fn from_settings(settings: &KeybindSettings) -> Self {
        let bindings = settings
            .bindings
            .iter()
            .filter_map(|(action, chord)| {
                let key = egui::Key::from_name(&chord.key)?;
                if *action == KeyAction::CycleAgentRole
                    && key == egui::Key::Tab
                    && !chord.ctrl
                    && !chord.alt
                {
                    return None;
                }
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

    /// 端末フォーカス中でも workbench へ渡すキーかどうかを返します。
    ///
    /// Ctrl+英字は端末の制御文字 (Ctrl+S = XOFF など) として使うため、割り当てがあっても端末を優先します。
    pub fn passes_through_terminal(&self, key: egui::Key, modifiers: egui::Modifiers) -> bool {
        let ctrl = modifiers.command || modifiers.ctrl;
        self.action_for_key(key, modifiers).is_some()
            && !(ctrl && !modifiers.shift && !modifiers.alt && is_letter(key))
    }

    /// 現在の egui 入力状態に対応するアクションを返します。
    ///
    /// 修飾キーはフレーム終了時点の状態ではなく、各キーイベントが押下時に持っていた値で判定します。
    /// アイドル中のウィンドウでは Ctrl の押下と S の押下・Ctrl の解放が別フレームに届くことがあり、
    /// フレーム終了時点の状態では Ctrl+S を取りこぼすためです。
    pub fn action_for_input(&self, input: &egui::InputState) -> Option<KeyAction> {
        input.events.iter().find_map(|event| match event {
            egui::Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } => self.action_for_key(*key, *modifiers),
            _ => None,
        })
    }

    fn action_for_key(&self, key: egui::Key, modifiers: egui::Modifiers) -> Option<KeyAction> {
        let ctrl = modifiers.command || modifiers.ctrl;
        self.bindings
            .iter()
            .find(|(_, resolved)| {
                resolved.key == key
                    && resolved.ctrl == ctrl
                    && resolved.shift == modifiers.shift
                    && resolved.alt == modifiers.alt
            })
            .map(|(action, _)| *action)
    }
}

fn is_letter(key: egui::Key) -> bool {
    let name = key.name();
    name.len() == 1 && name.as_bytes()[0].is_ascii_uppercase()
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
    fn shortcut_resolves_when_ctrl_is_released_in_the_same_frame() {
        // Given: an idle window that received Ctrl in one frame, then S press,
        // S release and Ctrl release together in the next frame.
        let keymap = Keymap::from_settings(&KeybindSettings::default());
        let ctx = egui::Context::default();
        let frame = |events| {
            let mut actual = None;
            let raw = egui::RawInput {
                events,
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw, |ui| {
                actual = ui.input(|input| keymap.action_for_input(input));
            });
            output.textures_delta.clear();
            actual
        };
        let key = |pressed| egui::Event::Key {
            key: Key::S,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers: Modifiers::CTRL,
        };
        assert_eq!(
            frame(vec![egui::Event::ModifiersChanged(Modifiers::CTRL)]),
            None
        );

        // When: the second frame is resolved
        let actual = frame(vec![
            key(true),
            key(false),
            egui::Event::ModifiersChanged(Modifiers::NONE),
        ]);

        // Then: the chord uses the modifiers held when S was pressed
        assert_eq!(actual, Some(KeyAction::SaveLayout));
    }

    #[test]
    fn unmatched_input_returns_none() {
        let keymap = Keymap::from_settings(&KeybindSettings::default());

        let ctx = run_with_key(Key::Num4, Modifiers::COMMAND);
        assert_eq!(keymap.action_for_input(&ctx.input(|i| i.clone())), None);
    }

    #[test]
    fn terminal_keeps_control_letters_but_releases_workbench_chords() {
        // Given: the default bindings (Ctrl+1/2/3, Ctrl+S, Ctrl+Shift+R).
        let keymap = Keymap::from_settings(&KeybindSettings::default());
        let ctrl_shift = Modifiers::CTRL | Modifiers::SHIFT;

        // Then: pane focus and layout reset still reach the workbench,
        // while Ctrl+S stays a terminal control key (XOFF / readline search).
        assert!(keymap.passes_through_terminal(Key::Num1, Modifiers::CTRL));
        assert!(keymap.passes_through_terminal(Key::R, ctrl_shift));
        assert!(!keymap.passes_through_terminal(Key::S, Modifiers::CTRL));
        assert!(!keymap.passes_through_terminal(Key::C, Modifiers::CTRL));
    }
}
