//! Host services of the areas that are filled in one PR each (ADR 0045).
//!
//! Every protocol namespace that is not a plain router has one file here with
//! the service behind it. The files of the namespaces that are still reserved
//! hold an empty placeholder service, constructed from the same handles as the
//! others. A PR implementing an
//! area edits only its own files and nothing shared:
//!
//! | what | where |
//! |---|---|
//! | the wire types and operation names | `workbench-protocol/src/namespaces/<area>.rs` |
//! | its samples and golden file | `workbench-protocol/tests/common/areas/<area>.rs`, `tests/golden/areas/<area>.json` |
//! | the service, its state and its tasks | `areas/<area>.rs` (this directory) |
//! | the translation of its commands and queries | `ingress/namespaces/<area>.rs` |
//!
//! Each `<area>.rs` may grow into submodules in `<area>/`; those are the
//! area's too. The registration here (`mod <area>;`, the `Areas` field and
//! [`AreaHandles`]) is shared and changes only in an integration PR.
//!
//! The service is constructed once, from [`AreaHandles`]: the handles the
//! bootstrap already owns (runtime, configuration, storage writer handle, MCP
//! services, event bus, credential store, the model reload, the evaluation
//! service). An area never builds a runtime, a database writer or
//! an MCP pool of its own. It starts background work only through
//! [`AreaTasks`] and publishes events only through a [`ProjectionSource`].
//!
//! Handlers run synchronously under the host's mutex. They must not wait: work
//! that has to wait becomes an [`AreaTasks`] task, whose outcome reaches
//! clients as an event or a later query.
pub mod artifacts;
pub mod auth;
pub mod browser;
pub mod diagnostics;
pub mod evaluation;
pub mod execution;
pub mod improvement;
pub mod lanes;
pub mod maintenance;
pub mod mcp;
pub mod memory;
pub mod pull_requests;
pub mod settings;
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
    /// The credential store the runs read (PR 10: `settings`, `auth`).
    pub credentials: Arc<dyn sandbox::CredentialStore>,
    /// Recomposes the startup model and every project model after a settings
    /// save; the one the Evaluator's reload tool uses (PR 10).
    pub reload_models: ReloadModels,
    /// The evaluation service and the host callbacks of its tools (PR 13 reads,
    /// PR 17c runs). An area never builds an evaluation service of its own.
    pub evaluations: eval::evaluator::EvaluationService,
    pub evaluator: Arc<crate::bootstrap::evaluation::EvaluatorHost>,
    /// The event database file (PR 13, 17d). Opening it is the storage layer's
    /// business: read through `runtime` or write through `storage`.
    pub database: PathBuf,
    /// The thread owner; the workspace area hints it after an archive is saved.
    pub owner: Arc<runtime::ownership::OwnerHost>,
    /// The connections of the clients and the hooks to run when one leaves; an
    /// area that starts work for a client registers one (ADR 0045 §10).
    pub connections: Arc<crate::connections::ConnectionHooks>,
    /// The artifact store the runtime already writes to (PR 16a-2). The
    /// `artifacts` and `browser` areas read through it; they never open a second
    /// store on the same directory.
    pub artifacts: Arc<runtime::artifacts::ArtifactStore>,
    /// The ledger every spawned child of this core is recorded in. A renderer
    /// helper is spawned and reclaimed through it too; an area never builds a
    /// ledger of its own (PR 16a-2).
    pub spawn_ledger: sandbox::SpawnLedger,
    /// The command service's own restart gate (a clone shares its state). An
    /// area that must stop its work for a restart watches this one through
    /// `subscribe()` and `phase()`; it never builds a gate or a notification
    /// of its own (PR 16a-2).
    pub restart_gate: crate::bootstrap::restart::HostRestartGate,
}

/// See [`AreaHandles::reload_models`].
pub type ReloadModels = Arc<dyn Fn() -> Result<(), String> + Send + Sync>;

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
    /// See [`AreaHandles::connections`].
    pub connections: Arc<crate::connections::ConnectionHooks>,
    pub workspace: workspace::WorkspaceArea,
    pub mcp: mcp::McpArea,
    pub execution: execution::ExecutionArea,
    pub teams: teams::TeamsArea,
    pub lanes: lanes::LanesArea,
    pub settings: settings::SettingsArea,
    pub auth: auth::AuthArea,
    pub diagnostics: diagnostics::DiagnosticsArea,
    pub memory: memory::MemoryArea,
    pub improvement: improvement::ImprovementArea,
    pub evaluation: evaluation::EvaluationArea,
    pub pull_requests: pull_requests::PullRequestsArea,
    pub maintenance: maintenance::MaintenanceArea,
    pub artifacts: artifacts::ArtifactsArea,
    pub browser: browser::BrowserArea,
}

impl Areas {
    /// Construct every area from the bootstrap's handles.
    ///
    /// # Errors
    /// The first area that cannot be constructed; the bootstrap then fails.
    pub fn new(handles: &AreaHandles) -> Result<Self, BootstrapError> {
        let mut areas = Self {
            connections: handles.connections.clone(),
            workspace: workspace::WorkspaceArea::new(handles)?,
            mcp: mcp::McpArea::new(handles)?,
            execution: execution::ExecutionArea::new(handles)?,
            teams: teams::TeamsArea::new(handles)?,
            lanes: lanes::LanesArea::new(handles)?,
            settings: settings::SettingsArea::new(handles)?,
            auth: auth::AuthArea::new(handles)?,
            diagnostics: diagnostics::DiagnosticsArea::new(handles)?,
            memory: memory::MemoryArea::new(handles)?,
            improvement: improvement::ImprovementArea::new(handles)?,
            evaluation: evaluation::EvaluationArea::new(handles)?,
            pull_requests: pull_requests::PullRequestsArea::new(handles)?,
            maintenance: maintenance::MaintenanceArea::new(handles)?,
            artifacts: artifacts::ArtifactsArea::default(),
            browser: browser::BrowserArea::new(handles)?,
        };
        // Capture the same store/owner now; F2 changes only the leaf provider.
        areas.bind_artifacts(handles.artifacts.clone());
        Ok(areas)
    }
    fn bind_artifacts(&mut self, store: Arc<runtime::artifacts::ArtifactStore>) {
        self.artifacts = artifacts::ArtifactsArea::new(store, self.browser.renderer.clone());
    }

    /// The page provider the runtime offers `inspect_artifact` on, or `None`.
    ///
    /// The bootstrap asks once, after the areas exist and before startup
    /// recovery resumes any run, and hands a `Some` to
    /// [`AgentRuntime::set_artifact_pages`]. Asking before recovery means a
    /// restored run sees the same tool catalog as a new one from its first
    /// request. The answer must not depend on whether a helper is running: the
    /// catalog is stable across helper start, stop and crash. Until the
    /// `artifacts` area implements it this is `None`, and the catalog and the
    /// capabilities stay as they were.
    pub fn artifact_pages(&self) -> Option<Arc<dyn runtime::artifacts::ArtifactPages>> {
        self.artifacts.artifact_pages()
    }
}
