//! Reusable owner construction, without GUI state or background-service wiring.
use event_bus::EventBus;
use recorder::{
    pricing::SharedUsagePricing,
    storage_bridge::{DiagnosticPersistence, OwnedStorageBridge, StorageBridge},
};
use runtime::{ExecutionPolicy, Role, WorkspaceSeam};
use std::{path::PathBuf, sync::Arc, time::Duration};

/// The executor and per-run sandbox seam always use the same canonical root.
/// Host tools (e.g. evaluator tools) are explicit extensions, not standard tools.
pub struct ExecutionComponents {
    pub executor: Arc<tools::ToolExecutor>,
    pub workspace: WorkspaceSeam,
}

impl ExecutionComponents {
    pub fn production(
        bus: Arc<EventBus>,
        root: PathBuf,
        config: &config::Config,
        host_tools: Vec<Arc<dyn tools::Tool>>,
    ) -> Result<Self, runtime::RuntimeError> {
        let workspace = WorkspaceSeam::production(root)?;
        let executor = runtime::production_executor_with_host_tools(
            bus,
            &ExecutionPolicy::for_role(Role::Orchestrator),
            workspace.project_root().to_path_buf(),
            config,
            host_tools,
        )?;
        Ok(Self {
            executor,
            workspace,
        })
    }

    /// An unconfigured core can serve health without requiring bwrap or exposing tools.
    /// The legacy GUI deliberately keeps production tools, including in demo mode.
    pub fn unconfigured(bus: Arc<EventBus>, root: PathBuf) -> Result<Self, runtime::RuntimeError> {
        Ok(Self {
            executor: Arc::new(tools::ToolExecutor::new(bus)),
            workspace: WorkspaceSeam::production(root)?,
        })
    }
}

pub fn thread_index_settings(
    config: &config::Config,
) -> storage::thread_index::ThreadIndexSettings {
    storage::thread_index::ThreadIndexSettings {
        enabled: config.storage.thread_index.enabled,
        max_bytes: config.storage.thread_index.max_bytes,
    }
}

pub fn storage_config(config: &config::Config, db_path: PathBuf) -> storage::StorageConfig {
    storage::StorageConfig {
        db_path,
        usage_retention_days: config.metrics.retention_days,
        retain_terminal_task_progress: config.diagnostics.retain_terminal_task_progress,
        thread_index: thread_index_settings(config),
        ..Default::default()
    }
}

/// Process-specific persistence differences must be chosen by the caller.
pub struct BridgeOptions {
    pub session_id: String,
    pub flush_every: Duration,
    pub fault_spool: Option<PathBuf>,
}

pub fn spawn_storage_bridge(
    bus: Arc<EventBus>,
    storage: storage::Storage,
    diagnostics: config::DiagnosticPersistence,
    metrics_enabled: bool,
    pricing: SharedUsagePricing,
    options: BridgeOptions,
) -> Result<OwnedStorageBridge, std::io::Error> {
    let persistence = match diagnostics {
        config::DiagnosticPersistence::Off => DiagnosticPersistence::Off,
        config::DiagnosticPersistence::Warnings => DiagnosticPersistence::Warnings,
        config::DiagnosticPersistence::All => DiagnosticPersistence::All,
    };
    OwnedStorageBridge::spawn(
        bus,
        storage,
        |handle| {
            let bridge = StorageBridge::new(handle, options.session_id)
                .with_diagnostic_persistence(persistence)
                .with_metrics_enabled(metrics_enabled)
                .with_usage_ledger(pricing);
            match options.fault_spool {
                Some(dir) => bridge.with_fault_spool(dir),
                None => bridge,
            }
        },
        options.flush_every,
    )
}

/// Trusted host declarations, never read from repository configuration. Missing
/// directories are filtered by the runtime just as for a live GUI declaration.
#[derive(Clone, Default, serde::Deserialize)]
pub struct ProjectAccess {
    #[serde(default)]
    pub trust: Trust,
    #[serde(default)]
    pub allowed_directories: Vec<AllowedDirectory>,
}

#[derive(Clone, Copy, Default, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trust {
    Approved,
    #[default]
    Unapproved,
}

#[derive(Clone, serde::Deserialize)]
pub struct AllowedDirectory {
    pub path: PathBuf,
    pub trust: Trust,
}

impl ProjectAccess {
    pub fn apply(&self, runtime: &runtime::AgentRuntime, root: PathBuf) {
        runtime.set_project_trust(
            root.clone(),
            match self.trust {
                Trust::Approved => runtime::ProjectTrust::Approved,
                Trust::Unapproved => runtime::ProjectTrust::Unapproved,
            },
        );
        runtime.set_project_allowed_directories(
            root,
            self.allowed_directories
                .iter()
                .filter(|directory| {
                    directory.path.is_absolute() && matches!(directory.trust, Trust::Approved)
                })
                .map(|directory| directory.path.clone())
                .collect(),
        );
    }
}
