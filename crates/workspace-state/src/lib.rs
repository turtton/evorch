//! GUI-independent sidebar state: projects, threads, conversation families, the
//! three-way merge of two saved sidebars and their persistence.
//!
//! The desktop (`workspace-ui` re-exports everything here) and the core's workspace
//! service (`workbench-host`) share this crate, so there is one definition of what a
//! family, a lineage or a merge means. Nothing here draws, spawns or holds a handle
//! to a runtime, a database or a window.

mod errors;
mod family;
mod lane;
pub mod lane_management;
mod lineage;
mod merge;
mod persist;
mod project;
mod sidebar;
pub mod store;
mod thread;

pub use errors::{ProjectError, SidebarError, ThreadError};
pub use family::{FamilyState, FamilyUnresolved, MigrationReport, ResolvedState};
pub use lane::{LaneLink, LanePolicy};
pub use lineage::{
    LineageIndex, display_parent, normalize_versions, point_versions, show_version, version_root,
    visible_version,
};
pub use merge::merge_keyed;
pub use persist::{
    load_sidebar, load_sidebar_report, save_sidebar, sidebar_from_json, sidebar_from_json_report,
    sidebar_to_json,
};
pub use project::{AllowedDirectory, Membership, ProjectId, ProjectRecord, TrustState};
pub use sidebar::{SIDEBAR_SCHEMA_VERSION, SidebarState};
pub use store::{SidebarDocument, SidebarLock, SidebarStore, StoreError};
pub use thread::{
    ForkPoint, LineageKind, ModelPreference, ThreadChatRole, ThreadId, ThreadLineage, ThreadRecord,
    ThreadRunPhase, ThreadState,
};
