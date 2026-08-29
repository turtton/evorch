//! Agent Kernel と GUI renderer の間に置く framework 非依存 Workspace Model。
//!
//! UI Event Bus が運ぶ状態を永続化可能なレイアウトへ写し、renderer 固有型を
//! 公開 API に持ち込まないことで GUI framework の交換可能性を保ちます。

mod errors;
mod panels;
mod persist;
mod settings;
mod types;
mod validate;

pub use errors::{LayoutError, PersistError, SettingsError};
pub use panels::{Panel, PanelId, PanelKind, default_panels};
pub use persist::{
    from_json, load_from, load_settings, load_workspace, save_settings, save_to, save_workspace,
    to_json,
};
pub use settings::{
    KeyAction, KeyChord, KeybindSettings, LayoutSettings, SaveLayout, UI_SETTINGS_VERSION,
    UiSettings,
};
pub use types::{
    Floating, FloatingPane, InsertPosition, LayoutNode, Split, SplitDirection, Tabs,
    WORKSPACE_SCHEMA_VERSION, Window, WindowRect, WindowState, Workspace,
};
pub use validate::validate;
