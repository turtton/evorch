use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::{SettingsError, Workspace};

pub const UI_SETTINGS_VERSION: u32 = 1;

/// UI 設定ファイルのルート構造。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiSettings {
    pub version: u32,
    pub layout: LayoutSettings,
    pub keybinds: KeybindSettings,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            version: UI_SETTINGS_VERSION,
            layout: LayoutSettings::default(),
            keybinds: KeybindSettings::default(),
        }
    }
}

/// 保存済み Workspace の設定。`None` は v0.1 既定レイアウトを表します。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LayoutSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<Workspace>,
}

/// GUI 操作に割り当て可能なアクション。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyAction {
    FocusAgentPane,
    FocusTerminalPane,
    FocusTasksPane,
    SaveLayout,
    ResetLayout,
}

/// Workspace 保存要求として event bus の payload に利用するイベント。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveLayout;

/// Framework 非依存のキー入力表現。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct KeyChord {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub key: String,
}

impl FromStr for KeyChord {
    type Err = SettingsError;

    fn from_str(source: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = source.split('+').collect();
        let Some((key, modifiers)) = parts.split_last() else {
            return Err(SettingsError::InvalidKeyChord(source.to_owned()));
        };
        if key.trim().is_empty() {
            return Err(SettingsError::InvalidKeyChord(source.to_owned()));
        }

        let mut chord = Self {
            ctrl: false,
            shift: false,
            alt: false,
            key: key.trim().to_owned(),
        };
        for modifier in modifiers {
            if modifier.eq_ignore_ascii_case("ctrl") && !chord.ctrl {
                chord.ctrl = true;
            } else if modifier.eq_ignore_ascii_case("shift") && !chord.shift {
                chord.shift = true;
            } else if modifier.eq_ignore_ascii_case("alt") && !chord.alt {
                chord.alt = true;
            } else {
                return Err(SettingsError::InvalidKeyChord(source.to_owned()));
            }
        }
        Ok(chord)
    }
}

impl fmt::Display for KeyChord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.ctrl {
            formatter.write_str("Ctrl+")?;
        }
        if self.shift {
            formatter.write_str("Shift+")?;
        }
        if self.alt {
            formatter.write_str("Alt+")?;
        }
        formatter.write_str(&self.key)
    }
}

impl From<KeyChord> for String {
    fn from(chord: KeyChord) -> Self {
        chord.to_string()
    }
}

impl TryFrom<String> for KeyChord {
    type Error = SettingsError;

    fn try_from(source: String) -> Result<Self, Self::Error> {
        Self::from_str(&source)
    }
}

/// アクションからキーコードへの公開設定。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct KeybindSettings {
    pub bindings: BTreeMap<KeyAction, KeyChord>,
}

impl Default for KeybindSettings {
    fn default() -> Self {
        let bindings = [
            (
                KeyAction::FocusAgentPane,
                KeyChord {
                    ctrl: true,
                    shift: false,
                    alt: false,
                    key: "1".to_owned(),
                },
            ),
            (
                KeyAction::FocusTerminalPane,
                KeyChord {
                    ctrl: true,
                    shift: false,
                    alt: false,
                    key: "2".to_owned(),
                },
            ),
            (
                KeyAction::FocusTasksPane,
                KeyChord {
                    ctrl: true,
                    shift: false,
                    alt: false,
                    key: "3".to_owned(),
                },
            ),
            (
                KeyAction::SaveLayout,
                KeyChord {
                    ctrl: true,
                    shift: false,
                    alt: false,
                    key: "S".to_owned(),
                },
            ),
            (
                KeyAction::ResetLayout,
                KeyChord {
                    ctrl: true,
                    shift: true,
                    alt: false,
                    key: "R".to_owned(),
                },
            ),
        ]
        .into_iter()
        .collect();
        Self { bindings }
    }
}
