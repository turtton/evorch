//! GUI-free owner composition. TODO(P10/P11): migrate GUI/headless composition
//! here; do not change their ownership model as part of P7a.
mod project_models;
#[cfg(test)]
mod tests;
use crate::projects::ProjectDirectory;
use crate::{CommandHost, commands::CommandService, projects::StaticProjects};
use event_bus::{AgentRunPhase, EventBus};
use recorder::{
    pricing::UsagePricing,
    storage_bridge::{OwnedStorageBridge, StorageBridge},
};
use runtime::{
    AgentRuntime, ModelSource, RunStore, RuntimeComposition, WorkspaceSeam, ownership::OwnerHost,
};
use sha2::{Digest, Sha256};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};

pub type BootstrapError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Clone, Debug)]
pub struct BootstrapOptions {
    pub config_dir: PathBuf,
    pub ownership_root: PathBuf,
    pub project_root: PathBuf,
}

fn known_projects(options: &BootstrapOptions) -> Result<StaticProjects, BootstrapError> {
    #[derive(serde::Deserialize)]
    struct Sidebar {
        projects: Vec<Project>,
    }
    #[derive(serde::Deserialize)]
    struct Project {
        id: workbench_protocol::ids::ProjectId,
        repo_root: PathBuf,
    }
    let mut projects = StaticProjects::default();
    let path = options.config_dir.join("sidebar.json");
    match std::fs::read(path) {
        Ok(bytes) => {
            // Read only the directory, not GUI drafts/family state. Never write
            // sidebar.json or infer permission to claim a thread from its presence.
            for project in serde_json::from_slice::<Sidebar>(&bytes)?.projects {
                if project.repo_root.is_absolute() {
                    projects.insert(&project.id, project.repo_root);
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.into()),
    }
    let root = std::fs::canonicalize(&options.project_root)?;
    if !projects.entries().iter().any(|(_, p)| p == &root) {
        use std::os::unix::ffi::OsStrExt;
        let hash = format!("{:x}", Sha256::digest(root.as_os_str().as_bytes()));
        projects.insert(
            &workbench_protocol::ids::ProjectId::new(format!("core-{}", &hash[..32]))?,
            root,
        );
    }
    Ok(projects)
}

/// What a test replaces in the production composition; production passes none of it.
pub struct TestSeams {
    pub model: Arc<dyn runtime::AgentModel>,
    pub sandbox: Arc<dyn runtime::SandboxFactory>,
}

/// Keep the bridge, owner and shared maintenance leases alive until the runtime
/// has checkpointed. Transports must stop accepting requests before shutdown.
pub struct BootstrappedHost {
    pub host: Arc<Mutex<CommandHost>>,
    pub runtime: AgentRuntime,
    pub owner: Arc<OwnerHost>,
    pub config: config::Config,
    bridge: Option<OwnedStorageBridge>,
    reclaim: Option<runtime::thread_retention::ReclaimExecutor>,
    executor: Arc<tools::ToolExecutor>,
    _leases: storage::SharedLeases,
}

impl BootstrappedHost {
    pub async fn open(options: BootstrapOptions) -> Result<Self, BootstrapError> {
        Self::open_composed(options, None).await
    }

    /// The production composition with the model and the per-run sandbox replaced, so a test
    /// can drive the real owner, storage, command service and lane delivery wiring on a
    /// machine with neither a provider nor bwrap.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn open_with_test_seams(
        options: BootstrapOptions,
        seams: TestSeams,
    ) -> Result<Self, BootstrapError> {
        Self::open_composed(options, Some(seams)).await
    }

    async fn open_composed(
        options: BootstrapOptions,
        seams: Option<TestSeams>,
    ) -> Result<Self, BootstrapError> {
        let config = config::Config::load_strict(&config::LoadOptions {
            user_config_dir: Some(options.config_dir.clone()),
            project_dir: Some(options.project_root.clone()),
            read_env: false,
            ..Default::default()
        })?;
        let bus = Arc::new(EventBus::new(4096));
        let db_path = options.config_dir.join("evorch-events.db");
        // P7a is an additional owner, not the sidebar's retention authority.
        // Do not interpret/migrate GUI sidebar state or confirm new deletions.
        // Shared leases prevent another process doing exclusive maintenance.
        let lease_db = db_path.clone();
        let lease_root = options.ownership_root.clone();
        let leases = tokio::task::spawn_blocking(move || {
            storage::shared_with_wait(&lease_db, &lease_root, Duration::from_secs(30))
        })
        .await??;
        let storage_config = storage::StorageConfig {
            db_path,
            usage_retention_days: config.metrics.retention_days,
            retain_terminal_task_progress: config.diagnostics.retain_terminal_task_progress,
            thread_index: storage::thread_index::ThreadIndexSettings {
                max_bytes: config.storage.thread_index.max_bytes,
                enabled: config.storage.thread_index.enabled,
            },
            ..Default::default()
        };
        let storage =
            storage::Storage::open_with_lease(storage_config.clone(), leases.db_lease()?)?;
        let handle = storage.handle();
        let tombstones = storage::Database::open(&storage_config)?.tombstones()?;
        let pricing = Arc::new(RwLock::new(UsagePricing::new(
            config.providers.clone(),
            None,
        )));
        let diagnostics = match config.diagnostics.persistence {
            config::DiagnosticPersistence::Off => {
                recorder::storage_bridge::DiagnosticPersistence::Off
            }
            config::DiagnosticPersistence::Warnings => {
                recorder::storage_bridge::DiagnosticPersistence::Warnings
            }
            config::DiagnosticPersistence::All => {
                recorder::storage_bridge::DiagnosticPersistence::All
            }
        };
        let bridge = OwnedStorageBridge::spawn(
            bus.clone(),
            storage,
            |h| {
                StorageBridge::new(h, "evorch-core")
                    .with_diagnostic_persistence(diagnostics)
                    .with_metrics_enabled(config.metrics.enabled)
                    .with_usage_ledger(pricing)
            },
            Duration::from_secs(5),
        )?;
        let credential_dir = options.config_dir.join("credentials");
        let credentials =
            tokio::task::spawn_blocking(move || sandbox::credential::open_default(credential_dir))
                .await??;
        let (fixed_model, seam) = match seams {
            Some(seams) => (
                Some(seams.model),
                WorkspaceSeam::with_factory(options.project_root.clone(), seams.sandbox)?,
            ),
            None => (
                None,
                WorkspaceSeam::production(options.project_root.clone())?,
            ),
        };
        // No provider means a read-only, unconfigured host. Register NO tools in
        // that case, rather than weakening sandbox requirements to serve health.
        let executor = if config.providers.is_empty() {
            Arc::new(tools::ToolExecutor::new(bus.clone()))
        } else {
            runtime::production_executor_with_config(
                bus.clone(),
                &runtime::ExecutionPolicy::for_role(runtime::Role::Orchestrator),
                options.project_root.clone(),
                &config,
            )?
        };
        let composed = runtime::compose_runtime(RuntimeComposition {
            config: &config,
            user_config_dir: Some(options.config_dir.clone()),
            model_catalog_dir: None,
            bus: bus.clone(),
            executor: executor.clone(),
            credential_store: credentials.clone(),
            env: Arc::new(routing::ProcessEnv),
            model_source: match fixed_model {
                Some(model) => ModelSource::Fixed(model),
                None if config.providers.is_empty() => {
                    ModelSource::Fixed(Arc::new(runtime::compose::UnconfiguredModel))
                }
                None => ModelSource::Configured,
            },
            workspace: Some(seam),
        })?;
        let artifacts = Arc::new(runtime::artifacts::store_with_refs(
            options.config_dir.join("artifacts"),
            handle.clone(),
        )?);
        let runtime = composed
            .runtime
            .with_project_models(project_models::resolver(&options, credentials, bus.clone()))
            .with_sandbox_root(options.project_root.clone())
            .with_run_store(RunStore::open(&storage_config, handle.clone())?)
            .with_artifact_store(artifacts.clone());
        let guard = runtime::thread_retention::TombstoneGuard::new(tombstones);
        runtime
            .set_tombstone_guard(guard.clone())
            .map_err(|_| "runtime tombstone guard already set")?;
        let owner = Arc::new(OwnerHost::open(
            &options.ownership_root,
            config.ownership.clone(),
            bus.clone(),
        )?);
        owner
            .set_tombstone_guard(guard)
            .map_err(|_| "owner tombstone guard already set")?;
        runtime.set_thread_scratch(owner.thread_scratch());
        let service = CommandService::new(runtime.clone(), tokio::runtime::Handle::current())
            .with_event_bus(bus)
            .with_memory_storage(storage_config.clone())
            .with_team_writer(handle.clone())
            .with_ownership(owner.clone());
        service.bind_lane_delivery();
        let projects = known_projects(&options)?;
        let host = Arc::new(Mutex::new(
            CommandHost::new(service)
                .with_projects(projects)
                .with_lane_sidebar_path(options.config_dir.join("sidebar.json")),
        ));
        let reclaim = runtime::thread_retention::ReclaimExecutor::spawn(
            runtime::thread_retention::ExecutorConfig::new(handle, storage_config, Some(artifacts)),
        )?;
        Ok(Self {
            host,
            runtime,
            owner,
            config,
            bridge: Some(bridge),
            reclaim: Some(reclaim),
            executor,
            _leases: leases,
        })
    }

    pub fn busy(&self) -> bool {
        // TODO(P9): once safe-point restart/auto-resume can restore an ordinary
        // chat's Waiting state, count that idle conversation as no work. Until
        // then retain all Waiting runs conservatively to avoid losing them.
        self.host.lock().unwrap().service().has_background_work()
            || self.runtime.list_agents().iter().any(|r| {
                self.executor.has_running_shell_jobs(&r.run_id.to_string())
                    || matches!(
                        r.phase,
                        AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting
                    )
            })
    }

    /// ADR 0024 drain: quiesce, await runtime checkpoints, flush, then release.
    pub async fn shutdown(&mut self) -> Result<(), BootstrapError> {
        self.owner.begin_quiesce()?;
        let tasks = self
            .host
            .lock()
            .unwrap()
            .service_mut()
            .take_background_tasks();
        for run in self.runtime.list_agents() {
            if matches!(
                run.phase,
                AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting
            ) {
                self.runtime.stop(run.run_id, runtime::StopScope::Subtree)?;
            }
        }
        for run in self.runtime.list_agents() {
            self.runtime.wait(run.run_id).await?;
            self.executor.cancel_shell_jobs(&run.run_id.to_string());
            self.executor
                .drain_shell_jobs(&run.run_id.to_string())
                .await?;
        }
        for task in tasks {
            task.await?;
        }
        if let Some(reclaim) = self.reclaim.take() {
            reclaim.shutdown();
        }
        if let Some(bridge) = &mut self.bridge {
            bridge.flush()?;
        }
        if self.owner.release_ready()? {
            return Err("owner still has active turns after runtime drain".into());
        }
        if let Some(mut bridge) = self.bridge.take() {
            bridge.shutdown();
        }
        Ok(())
    }
}
