//! Agent Kernel と GUI renderer の間に置く framework 非依存 Workspace Model。
//!
//! UI Event Bus が運ぶ状態を永続化可能なレイアウトへ写し、renderer 固有型を
//! 公開 API に持ち込まないことで GUI framework の交換可能性を保ちます。
//!
//! sidebar の状態（project / thread / family / merge / 永続化）は GUI に依存しない
//! `workspace-state` crate にあり、ここから同じ名前で再 export します。コア側の
//! workspace service も同じ crate を使うため、定義は 1 つです。

mod errors;
mod migrate;
mod panels;
mod persist;
mod reconcile;
mod settings;
mod sidebar_view;
mod types;
mod validate;

pub use workspace_state::lane_management;
pub use workspace_state::{
    AllowedDirectory, FamilyState, FamilyUnresolved, ForkPoint, LaneLink, LanePolicy, LineageIndex,
    LineageKind, Membership, MigrationReport, ModelPreference, ProjectError, ProjectId,
    ProjectRecord, ResolvedState, SIDEBAR_SCHEMA_VERSION, SidebarError, SidebarState,
    ThreadChatRole, ThreadError, ThreadId, ThreadLineage, ThreadRecord, ThreadRunPhase,
    ThreadState, TrustState, display_parent, load_sidebar, load_sidebar_report, merge_keyed,
    normalize_versions, point_versions, save_sidebar, show_version, sidebar_from_json,
    sidebar_from_json_report, sidebar_to_json, version_root, visible_version,
};

pub use errors::{LayoutError, PersistError, SettingsError};
pub use panels::{Panel, PanelId, PanelKind, default_panels, default_panels_v02};
pub use persist::{
    from_json, load_from, load_settings, load_workspace, save_settings, save_to, save_workspace,
    to_json,
};
pub use settings::{
    KeyAction, KeyChord, KeybindSettings, LayoutSettings, SaveLayout, ThemePresetName,
    UI_SETTINGS_VERSION, UiSettings,
};
pub use sidebar_view::{ProjectSort, SidebarView, ordered_projects};
pub use types::{
    Floating, FloatingPane, InsertPosition, LayoutNode, Split, SplitDirection, Tabs,
    WORKSPACE_SCHEMA_VERSION, Window, WindowRect, WindowState, Workspace,
};
pub use validate::validate;
