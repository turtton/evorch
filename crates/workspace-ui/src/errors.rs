use thiserror::Error;

/// Workspace の不変条件違反。
#[derive(Debug, Clone, PartialEq, Error)]
pub enum LayoutError {
    #[error("workspace migration failed: {detail}")]
    Migration { detail: String },
    #[error("unsupported workspace version {found}; supported version is {supported}")]
    UnsupportedVersion { found: u32, supported: u32 },
    #[error("split fraction must be finite and within (0, 1), got {fraction}")]
    InvalidFraction { fraction: f32 },
    #[error("layout references unknown panel '{panel_id}'")]
    UnknownPanel { panel_id: String },
    #[error("panel '{panel_id}' is placed more than once")]
    DuplicatePanel { panel_id: String },
    #[error("tabs must contain at least one panel")]
    EmptyTabs,
    #[error("active tab index {active} is out of bounds for {len} tabs")]
    ActiveTabOutOfBounds { active: usize, len: usize },
    #[error("window rectangle must have finite positive size, got {width} x {height}")]
    InvalidRect { width: f32, height: f32 },
    #[error("agent transcript panel '{panel_id}' requires a target")]
    MissingTarget { panel_id: String },
    #[error("panel '{panel_id}' must not have a target")]
    UnexpectedTarget { panel_id: String },
    #[error("panel registry key '{key}' does not match panel id '{panel_id}'")]
    PanelIdMismatch { key: String, panel_id: String },
}

/// Project registration and allowed-directory boundary errors.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProjectError {
    #[error("path must be absolute")]
    NotAbsolute,
    #[error("path is not a directory")]
    NotADirectory,
    #[error("failed to canonicalize path '{0}'")]
    Canonicalize(String),
    #[error("project already exists")]
    DuplicateProject,
    #[error("allowed directory already exists")]
    DuplicateAllowedDirectory,
    #[error("allowed directory is nested in the project root")]
    NestedInProjectRoot,
    #[error("allowed directory is nested in an existing allowed directory")]
    NestedInExistingAllowed,
    #[error("unknown project")]
    UnknownProject,
}

/// Thread mutation errors.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ThreadError {
    #[error("unknown project")]
    UnknownProject,
    #[error("thread already exists")]
    DuplicateThread,
    #[error("unknown thread")]
    UnknownThread,
}

/// Sidebar persistence and validation errors.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SidebarError {
    #[error("sidebar serialization failed: {0}")]
    Serialization(String),
    #[error("sidebar I/O failed: {0}")]
    Io(String),
    #[error("sidebar validation failed: {0}")]
    Validation(String),
}

/// Workspace JSON の永続化エラー。
#[derive(Debug, Clone, PartialEq, Error)]
pub enum PersistError {
    #[error("workspace serialization failed: {0}")]
    Serialization(String),
    #[error("workspace I/O failed: {0}")]
    Io(String),
    #[error(transparent)]
    Layout(#[from] LayoutError),
}

/// UI 設定の解析・永続化エラー。
#[derive(Debug, Clone, PartialEq, Error)]
pub enum SettingsError {
    #[error("invalid key chord '{0}'")]
    InvalidKeyChord(String),
    #[error("settings serialization failed: {0}")]
    Serialization(String),
    #[error("settings I/O failed: {0}")]
    Io(String),
    #[error("unsupported settings version {found}; supported version is {supported}")]
    UnsupportedVersion { found: u32, supported: u32 },
    #[error(transparent)]
    Layout(#[from] LayoutError),
}
