//! GUI-free owner composition and reusable construction parts (ADR 0045 P8a).
//! Legacy GUI wiring stays in place; background service wiring follows in P8b.
//!
//! Deliberate legacy GUI/core differences:
//! - Core loads strict configuration from an explicit directory without env
//!   overrides; GUI can recover from invalid configuration for its settings UI.
//! - GUI wraps the same routed model in a live-reload switch; core uses Configured.
//! - Unconfigured core exposes no tools; GUI/demo keeps production tools. GUI
//!   evaluator tools are explicit host extensions, deferred on core until PR03.
//! - GUI owns retention/index maintenance, fault spooling and a 60s bridge tick;
//!   core only holds shared leases and keeps its 5s tick. Both share DB settings.
//! - Saved project access is read-only here; live settings updates remain with
//!   the existing GUI. No project configuration can manufacture trust.
//! - PR03 owns lane/PR/learning/MCP service factories. Register their stop and
//!   completion futures in `services`, never detach a storage-using task.
pub mod components;
pub mod models;
mod project_models;
pub mod services;
#[cfg(test)]
mod tests;
use crate::projects::ProjectDirectory;
use crate::{CommandHost, commands::CommandService, projects::StaticProjects};
use event_bus::{AgentRunPhase, EventBus};
use recorder::{pricing::UsagePricing, storage_bridge::OwnedStorageBridge};
use runtime::{AgentRuntime, ModelSource, RunStore, RuntimeComposition, ownership::OwnerHost};
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

struct KnownProjects {
    directory: StaticProjects,
    access: Vec<(PathBuf, components::ProjectAccess)>,
}

fn known_projects(options: &BootstrapOptions) -> Result<KnownProjects, BootstrapError> {
    #[derive(serde::Deserialize)]
    struct Sidebar {
        projects: Vec<Project>,
    }
    #[derive(serde::Deserialize)]
    struct Project {
        id: workbench_protocol::ids::ProjectId,
        repo_root: PathBuf,
        #[serde(flatten)]
        access: components::ProjectAccess,
    }
    let mut projects = StaticProjects::default();
    let mut access = Vec::new();
    let path = options.config_dir.join("sidebar.json");
    match std::fs::read(path) {
        Ok(bytes) => {
            // Read only project declarations, not GUI drafts/family state. Never write
            // sidebar.json or infer permission to claim a thread from its presence.
            for project in serde_json::from_slice::<Sidebar>(&bytes)?.projects {
                if project.repo_root.is_absolute() {
                    access.push((project.repo_root.clone(), project.access));
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
        access.push((root.clone(), components::ProjectAccess::default()));
        projects.insert(
            &workbench_protocol::ids::ProjectId::new(format!("core-{}", &hash[..32]))?,
            root,
        );
    }
    Ok(KnownProjects {
        directory: projects,
        access,
    })
}

/// Keep the bridge, owner and shared maintenance leases alive until the runtime
/// has checkpointed. Transports must stop accepting requests before shutdown.
pub struct BootstrappedHost {
    pub host: Arc<Mutex<CommandHost>>,
    pub runtime: AgentRuntime,
    pub owner: Arc<OwnerHost>,
    pub config: config::Config,
    pub services: services::BackgroundServices,
    #[cfg(any(test, feature = "test-support"))]
    pub composition: CompositionInventory,
    bridge: Option<OwnedStorageBridge>,
    reclaim: Option<runtime::thread_retention::ReclaimExecutor>,
    executor: Arc<tools::ToolExecutor>,
    _leases: storage::SharedLeases,
}

/// Actual construction output, exposed only to composition contract tests.
#[cfg(any(test, feature = "test-support"))]
pub struct CompositionInventory {
    pub tools: Vec<tools::executor::ToolSpec>,
    pub storage: storage::StorageConfig,
    pub model: runtime::compose::ModelIdentity,
}

impl BootstrappedHost {
    pub async fn open(options: BootstrapOptions) -> Result<Self, BootstrapError> {
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
        let storage_config = components::storage_config(&config, db_path);
        let storage =
            storage::Storage::open_with_lease(storage_config.clone(), leases.db_lease()?)?;
        let handle = storage.handle();
        let tombstones = storage::Database::open(&storage_config)?.tombstones()?;
        let pricing = Arc::new(RwLock::new(UsagePricing::new(
            config.providers.clone(),
            None,
        )));
        let bridge = components::spawn_storage_bridge(
            bus.clone(),
            storage,
            config.diagnostics.persistence,
            config.metrics.enabled,
            pricing,
            components::BridgeOptions {
                session_id: "evorch-core".into(),
                flush_every: Duration::from_secs(5),
                fault_spool: None,
            },
        )?;
        let credential_dir = options.config_dir.join("credentials");
        let credentials =
            tokio::task::spawn_blocking(move || sandbox::credential::open_default(credential_dir))
                .await??;
        // Health stays available without providers, tools or a production sandbox.
        let components::ExecutionComponents {
            executor,
            workspace,
        } = if config.providers.is_empty() {
            components::ExecutionComponents::unconfigured(
                bus.clone(),
                options.project_root.clone(),
            )?
        } else {
            components::ExecutionComponents::production(
                bus.clone(),
                options.project_root.clone(),
                &config,
                Vec::new(),
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
            model_source: if config.providers.is_empty() {
                ModelSource::Fixed(Arc::new(runtime::compose::UnconfiguredModel))
            } else {
                ModelSource::Configured
            },
            workspace: Some(workspace),
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
        for (root, access) in projects.access {
            access.apply(&runtime, root);
        }
        let host = Arc::new(Mutex::new(
            CommandHost::new(service)
                .with_projects(projects.directory)
                .with_lane_sidebar_path(options.config_dir.join("sidebar.json")),
        ));
        #[cfg(any(test, feature = "test-support"))]
        let composition = CompositionInventory {
            tools: executor.tool_specs(),
            storage: storage_config.clone(),
            model: composed.model_identity,
        };
        let reclaim = runtime::thread_retention::ReclaimExecutor::spawn(
            runtime::thread_retention::ExecutorConfig::new(handle, storage_config, Some(artifacts)),
        )?;
        Ok(Self {
            host,
            runtime,
            owner,
            config,
            services: services::BackgroundServices::default(),
            #[cfg(any(test, feature = "test-support"))]
            composition,
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
        self.services.request_stop();
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
        let services_result = self.services.shutdown().await;
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
        services_result
    }
}
