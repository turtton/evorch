//! Agent Kernel と GUI renderer の間に置く framework 非依存 Workspace Model。
//!
//! UI Event Bus が運ぶ状態を永続化可能なレイアウトへ写し、renderer 固有型を
//! 公開 API に持ち込まないことで GUI framework の交換可能性を保ちます。

mod errors;
mod lineage;
mod migrate;
mod panels;
mod persist;
mod project;
mod reconcile;
mod settings;
mod sidebar;
mod thread;
mod types;
mod validate;

pub use errors::{
    LayoutError, PersistError, ProjectError, SettingsError, SidebarError, ThreadError,
};
pub use lineage::{
    branch_versions, display_parent, normalize_versions, show_version, version_members,
    version_root, visible_version,
};
pub use panels::{Panel, PanelId, PanelKind, default_panels, default_panels_v02};
pub use persist::{
    from_json, load_from, load_settings, load_sidebar, load_workspace, save_settings, save_sidebar,
    save_to, save_workspace, sidebar_from_json, sidebar_to_json, to_json,
};
pub use project::{AllowedDirectory, Membership, ProjectId, ProjectRecord, TrustState};
pub use settings::{
    KeyAction, KeyChord, KeybindSettings, LayoutSettings, SaveLayout, ThemePresetName,
    UI_SETTINGS_VERSION, UiSettings,
};
pub use sidebar::{SIDEBAR_SCHEMA_VERSION, SidebarState};
pub use thread::{
    ForkPoint, LineageKind, ModelPreference, ThreadChatRole, ThreadId, ThreadLineage, ThreadRecord,
    ThreadRunPhase, ThreadState,
};
pub use types::{
    Floating, FloatingPane, InsertPosition, LayoutNode, Split, SplitDirection, Tabs,
    WORKSPACE_SCHEMA_VERSION, Window, WindowRect, WindowState, Workspace,
};
pub use validate::validate;
