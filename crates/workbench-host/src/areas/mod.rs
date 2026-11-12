//! Host services of the areas that are filled in one PR each (ADR 0045).
//!
//! The protocol namespaces `workspace`, `mcp`, `execution`, `teams` and `lanes`
//! each get one file here with the service behind them. A PR implementing an
//! area edits only its own files and nothing shared:
//!
//! | what | where |
//! |---|---|
//! | the wire types and operation names | `workbench-protocol/src/namespaces/<area>.rs` |
//! | its samples and golden file | `workbench-protocol/tests/common/areas/<area>.rs`, `tests/golden/areas/<area>.json` |
//! | the service, its state and its tasks | `areas/<area>.rs` (this directory) |
//! | the translation of its commands and queries | `ingress/namespaces/<area>.rs` |
//!
//! The service is constructed once, from [`AreaHandles`]: the handles the
//! bootstrap already owns (runtime, configuration, storage writer handle, MCP
//! services, event bus). An area never builds a runtime, a database writer or
//! an MCP pool of its own. It starts background work only through
//! [`AreaTasks`] and publishes events only through a [`ProjectionSource`].
//!
//! Handlers run synchronously under the host's mutex. They must not wait: work
//! that has to wait becomes an [`AreaTasks`] task, whose outcome reaches
//! clients as an event or a later query.
pub mod execution;
pub mod lanes;
pub mod mcp;
pub mod tasks;
pub mod teams;
pub mod workspace;

use crate::bootstrap::BootstrapError;
use crate::projection::Projection;
pub use crate::projection::ProjectionSource;
use event_bus::EventBus;
use runtime::{AgentRuntime, mcp::McpHostServices};
use std::{path::PathBuf, sync::Arc};
pub use tasks::{AreaTasks, SpawnError, StopSignal};
use workbench_protocol::errors::WorkbenchError;

/// What an area service may use, all of it already owned by the bootstrap.
pub struct AreaHandles {
    pub runtime: AgentRuntime,
    pub config: Arc<config::Config>,
    pub config_dir: PathBuf,
    pub project_root: PathBuf,
    /// The ownership root that pairs the database with the sidebar (retention).
    pub ownership_root: PathBuf,
    pub bus: Arc<EventBus>,
    pub storage: storage::StorageHandle,
    pub mcp: McpHostServices,
    pub tasks: AreaTasks,
    pub projection: Arc<Projection>,
}

impl AreaHandles {
    /// The event source of `namespace` (one of the area's own namespaces).
    ///
    /// # Errors
    /// See [`Projection::source`].
    pub fn source(&self, namespace: &str) -> Result<ProjectionSource, WorkbenchError> {
        self.projection.source(namespace)
    }
}

/// The services of every area. `Default` is the unwired form that tests of the
/// host use: every area then has nothing behind it.
#[derive(Default)]
pub struct Areas {
    pub workspace: workspace::WorkspaceArea,
    pub mcp: mcp::McpArea,
    pub execution: execution::ExecutionArea,
    pub teams: teams::TeamsArea,
    pub lanes: lanes::LanesArea,
}

impl Areas {
    /// Construct every area from the bootstrap's handles.
    ///
    /// # Errors
    /// The first area that cannot be constructed; the bootstrap then fails.
    pub fn new(handles: &AreaHandles) -> Result<Self, BootstrapError> {
        Ok(Self {
            workspace: workspace::WorkspaceArea::new(handles)?,
            mcp: mcp::McpArea::new(handles)?,
            execution: execution::ExecutionArea::new(handles)?,
            teams: teams::TeamsArea::new(handles)?,
            lanes: lanes::LanesArea::new(handles)?,
        })
    }
}
