//! GUI-free owner composition and reusable construction parts (ADR 0045).
//! Learning, evaluation supervision and passive improvement/crash intake use
//! the same factories on core and legacy GUI. Every task has a stop/drain owner.
//!
//! Remaining GUI/core differences:
//! - Core loads strict explicit configuration, without environment overrides.
//! - Unconfigured core exposes no tools; GUI/demo keeps production tools.
//! - The core confirms and executes retention deletions (`retention`); a legacy
//!   GUI runs its own phase only when it holds the exclusive leases.
//! - GUI retains a 60s storage bridge tick; core retains its 5s tick.
//! - Project declarations are read-only: saved state cannot manufacture trust.
#[cfg(any(test, feature = "test-support"))]
mod agent_observation_seam;
pub mod components;
pub mod core_startup;
pub mod evaluation;
mod host_services;
mod idle;
pub mod lane_pr;
pub mod learning;
#[cfg(test)]
mod learning_tests;
pub mod models;
pub mod planned;
pub mod planned_exit;
mod project_models;
pub mod recovery;
#[cfg(test)]
mod recovery_06a_tests;
pub mod recovery_disposal;
pub mod restart;
pub mod retention;
#[cfg(test)]
mod scratch_archive_tests;
pub mod services;
#[cfg(test)]
mod tests;
mod thread_index;
use crate::areas::{AreaHandles, AreaTasks, Areas};
use crate::projects::ProjectDirectory;
use crate::{CommandHost, commands::CommandService, projects::StaticProjects};
#[cfg(test)]
use event_bus::AgentRunPhase;
use event_bus::EventBus;
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

/// What a test replaces in the production composition; production passes none of it.
pub struct TestSeams {
    pub model: Arc<dyn runtime::AgentModel>,
    pub sandbox: Arc<dyn runtime::SandboxFactory>,
}

type RuntimeObserver = Box<dyn FnOnce(&AgentRuntime) + Send>;

/// Keep the bridge, owner and shared maintenance leases alive until the runtime
/// has checkpointed. Transports must stop accepting requests before shutdown.
pub struct BootstrappedHost {
    pub host: Arc<Mutex<CommandHost>>,
    pub runtime: AgentRuntime,
    /// Central owner of core children and auxiliary workers (not GUI-owned children).
    pub spawn_ledger: sandbox::SpawnLedger,
    pub owner: Arc<OwnerHost>,
    pub config: config::Config,
    // The exact production DB configuration; composition inventory is test-only.
    restart_storage: storage::StorageConfig,
    // The handle every other owner of the writer was given; never a new writer.
    storage_writer: storage::StorageHandle,
    graph_collector: Arc<recovery_disposal::LiveGraphCollector>,
    pub services: services::BackgroundServices,
    pub mcp: runtime::mcp::McpHostServices,
    pub recovery_diagnostics: Option<Arc<dyn core_startup::CoreStartupDiagnostics>>,
    deferred: Option<DeferredStartup>,
    planned_sandbox: Option<Arc<sandbox::bwrap::PlannedBwrap>>,
    area_tasks: AreaTasks,
    pub admin: Arc<crate::admin::AdminService>,
    pub usage_pricing: recorder::pricing::SharedUsagePricing,
    pub evaluator: Arc<evaluation::EvaluatorHost>,
    evaluations: eval::evaluator::EvaluationService,
    #[cfg(any(test, feature = "test-support"))]
    pub composition: CompositionInventory,
    history_owner: Option<storage::history::background::Owner>,
    bridge: Option<OwnedStorageBridge>,
    planned_exit: Option<Arc<tokio::sync::Mutex<planned_exit::PlannedExit>>>,
    reclaim: Option<runtime::thread_retention::ReclaimExecutor>,
    thread_index_owner: Option<thread_index::ThreadIndexOwner>,
    executor: Arc<tools::ToolExecutor>,
    _leases: storage::SharedLeases,
}

/// Actual construction output, exposed only to composition contract tests.
#[cfg(any(test, feature = "test-support"))]
pub struct CompositionInventory {
    pub tools: Vec<tools::executor::ToolSpec>,
    pub storage: storage::StorageConfig,
    pub storage_writer: storage::StorageHandle,
    pub model: runtime::compose::ModelIdentity,
}

struct DeferredStartup {
    mode: core_startup::CoreBootMode,
    options: BootstrapOptions,
    storage: storage::StorageConfig,
    writer: storage::StorageHandle,
    projects: StaticProjects,
    startup: Option<storage::RecoveryStartupComplete>,
    bus: Arc<EventBus>,
    adapters: lane_pr::PullRequestAdapters,
    observe: Option<RuntimeObserver>,
    artifacts: Arc<runtime::artifacts::ArtifactStore>,
}

impl BootstrappedHost {
    /// The production database configuration, read-only. Leaves that read or
    /// write storage are built from this and `storage_writer`, nothing else.
    pub fn storage_config(&self) -> &storage::StorageConfig {
        &self.restart_storage
    }
    /// The single live collector installed before automatic recovery.
    pub fn graph_collector(&self) -> &Arc<recovery_disposal::LiveGraphCollector> {
        &self.graph_collector
    }
    /// The storage writer handle the host was composed with, read-only. Callers
    /// clone it to share the one writer; nothing here opens or replaces a writer.
    pub fn storage_writer(&self) -> &storage::StorageHandle {
        &self.storage_writer
    }
    pub async fn open(options: BootstrapOptions) -> Result<Self, BootstrapError> {
        Self::open_composed(options, None, None, None, None, None).await
    }

    /// The production composition with the model and the per-run sandbox replaced, so a test
    /// can drive the real owner, storage, command service and lane delivery wiring on a
    /// machine with neither a provider nor bwrap.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn open_with_test_seams(
        options: BootstrapOptions,
        seams: TestSeams,
    ) -> Result<Self, BootstrapError> {
        Self::open_composed(options, Some(seams), None, None, None, None).await
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn open_with_mcp_test_seams(
        options: BootstrapOptions,
        seams: TestSeams,
        mcp: runtime::mcp::McpHostServices,
    ) -> Result<Self, BootstrapError> {
        Self::open_composed(options, Some(seams), Some(mcp), None, None, None).await
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn open_with_service_seams(
        options: BootstrapOptions,
        seams: TestSeams,
        adapters: lane_pr::PullRequestAdapters,
    ) -> Result<Self, BootstrapError> {
        Self::open_composed(options, Some(seams), None, Some(adapters), None, None).await
    }

    /// Observe resources before recovery so failed-startup tests can audit cleanup.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn open_with_service_observer(
        options: BootstrapOptions,
        seams: TestSeams,
        adapters: lane_pr::PullRequestAdapters,
        observe: impl FnOnce(&AgentRuntime) + Send + 'static,
    ) -> Result<Self, BootstrapError> {
        Self::open_composed(
            options,
            Some(seams),
            None,
            Some(adapters),
            Some(Box::new(observe)),
            None,
        )
        .await
    }

    /// Core-only recovery runs after shared lease acquisition/reload and RunStore
    /// high-watermarks, but before learning or lane recovery can admit work.
    pub async fn open_for_core(
        options: BootstrapOptions,
        recovery: &dyn core_startup::CoreStartupHook,
    ) -> Result<Self, BootstrapError> {
        Self::open_composed(options, None, None, None, None, Some(recovery)).await
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn open_for_core_with_test_seams(
        options: BootstrapOptions,
        seams: TestSeams,
        recovery: &dyn core_startup::CoreStartupHook,
    ) -> Result<Self, BootstrapError> {
        Self::open_composed(options, Some(seams), None, None, None, Some(recovery)).await
    }

    async fn open_composed(
        options: BootstrapOptions,
        seams: Option<TestSeams>,
        mcp: Option<runtime::mcp::McpHostServices>,
        adapters: Option<lane_pr::PullRequestAdapters>,
        observe: Option<RuntimeObserver>,
        recovery: Option<&dyn core_startup::CoreStartupHook>,
    ) -> Result<Self, BootstrapError> {
        Self::open_staged(options, seams, mcp, adapters, observe, recovery, None).await
    }

    pub async fn open_claimed_for_core(
        claimed: planned::ClaimedBootstrap,
    ) -> Result<Self, BootstrapError> {
        let options = claimed.storage.options.clone();
        Self::open_staged(options, None, None, None, None, None, Some(claimed)).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn open_staged(
        options: BootstrapOptions,
        seams: Option<TestSeams>,
        mcp: Option<runtime::mcp::McpHostServices>,
        adapters: Option<lane_pr::PullRequestAdapters>,
        observe: Option<RuntimeObserver>,
        recovery: Option<&dyn core_startup::CoreStartupHook>,
        claimed: Option<planned::ClaimedBootstrap>,
    ) -> Result<Self, BootstrapError> {
        let planned = claimed.is_some();
        let successor_epoch = claimed.as_ref().map(|c| c.successor_epoch());
        let spawn_ledger = sandbox::SpawnLedger::new();
        let adapters =
            adapters.unwrap_or_else(|| lane_pr::PullRequestAdapters::owned(spawn_ledger.clone()));
        let (config, bus, storage_config, storage, leases, seed, claimed_owner, planned_collector) =
            if let Some(claimed) = claimed {
                let planned::PlannedStorage {
                    config,
                    bus,
                    storage_config,
                    storage,
                    leases,
                    seed,
                    collector,
                    ..
                } = claimed.storage;
                (
                    config,
                    bus,
                    storage_config,
                    storage,
                    leases,
                    Some(seed),
                    Some(claimed.owner),
                    Some(collector),
                )
            } else {
                let config = config::Config::load_strict(&config::LoadOptions {
                    user_config_dir: Some(options.config_dir.clone()),
                    project_dir: Some(options.project_root.clone()),
                    read_env: false,
                    ..Default::default()
                })?;
                let bus = Arc::new(EventBus::new(4096));
                let db_path = options.config_dir.join("evorch-events.db");
                // The core confirms the deletion of expired archived threads (see
                // `retention`): exclusive leases for the phase, then shared for its life.
                let plan = retention::RetentionPlan {
                    db_path: db_path.clone(),
                    ownership_root: options.ownership_root.clone(),
                    sidebar_path: options.config_dir.join("sidebar.json"),
                    artifacts_dir: Some(options.config_dir.join("artifacts")),
                    policy: runtime::thread_retention::policy_from_config(&config.storage),
                };
                let leases = tokio::task::spawn_blocking(move || {
                    retention::acquire_leases(&plan, Duration::from_secs(30))
                })
                .await??;
                let storage_config = components::storage_config(&config, db_path);
                let storage =
                    storage::Storage::open_with_lease(storage_config.clone(), leases.db_lease()?)?;
                (
                    config,
                    bus,
                    storage_config,
                    storage,
                    leases,
                    None,
                    None,
                    None,
                )
            };
        let handle = storage.handle();
        // This reload must stay AFTER acquire_leases (including any exclusive
        // maintenance/downgrade), regardless of which PR adds that phase first.
        let tombstones = storage::Database::open(&storage_config)?.tombstones()?;
        let startup = (recovery.is_some() || planned)
            .then_some(())
            .map(|_| {
                leases.recovery_startup_complete(&storage_config.db_path, &options.ownership_root)
            })
            .transpose()?;
        let pricing = Arc::new(RwLock::new(UsagePricing::new(
            config.providers.clone(),
            None,
        )));
        let bridge = components::spawn_storage_bridge(
            bus.clone(),
            storage,
            config.diagnostics.persistence,
            config.metrics.enabled,
            pricing.clone(),
            components::BridgeOptions {
                session_id: "evorch-core".into(),
                flush_every: Duration::from_secs(5),
                fault_spool: learning::draft_dir(&config, &storage_config.db_path)
                    .map(|d| d.join("crash-spool")),
            },
        )?;
        let credential_dir = options.config_dir.join("credentials");
        let credentials =
            tokio::task::spawn_blocking(move || sandbox::credential::open_default(credential_dir))
                .await??;
        let mcp = match mcp {
            Some(mcp) => mcp,
            None => host_services::mcp(&options.config_dir, spawn_ledger.clone()).await?,
        };
        let evaluator = Arc::new(evaluation::EvaluatorHost::default());
        let (evaluation_tools, _, evaluations) =
            eval::evaluator::tools_with_service(eval::evaluator::EvaluatorContext {
                database: storage_config.db_path.clone(),
                storage: handle.clone(),
                user_config_dir: options.config_dir.clone(),
                catalog_dir: None,
                bus: bus.clone(),
                host: evaluator.clone(),
                cli: None,
            });
        let evaluations = evaluations.with_spawn_ledger(spawn_ledger.clone());
        let planned_sandbox = if planned && !config.providers.is_empty() {
            let boundary =
                runtime::network::sandbox_boundary_for_startup(options.project_root.clone())?;
            Some(Arc::new(sandbox::bwrap::PlannedBwrap::new(
                sandbox::BwrapConfig::from_boundary(boundary),
                spawn_ledger.clone(),
            )))
        } else {
            None
        };
        // Health stays available without providers, tools or a production sandbox.
        let components::ExecutionComponents {
            executor,
            workspace,
        } = if config.providers.is_empty() {
            components::ExecutionComponents::unconfigured(
                bus.clone(),
                options.project_root.clone(),
            )?
        } else if let Some(sandbox) = &planned_sandbox {
            components::ExecutionComponents::planned(
                bus.clone(),
                options.project_root.clone(),
                &config,
                evaluation_tools,
                sandbox.clone(),
            )?
        } else {
            components::ExecutionComponents::production_owned(
                bus.clone(),
                options.project_root.clone(),
                &config,
                evaluation_tools,
                spawn_ledger.clone(),
            )?
        };
        let workspace = if config.providers.is_empty() {
            runtime::WorkspaceSeam::production_owned(
                options.project_root.clone(),
                spawn_ledger.clone(),
            )?
        } else {
            workspace
        };
        let (fixed_model, workspace) = match seams {
            Some(seams) => (
                Some(seams.model),
                runtime::WorkspaceSeam::with_factory(
                    workspace.project_root().to_path_buf(),
                    seams.sandbox,
                )?,
            ),
            None => (None, workspace),
        };
        let production = models::ProductionModel {
            load_options: config::LoadOptions {
                user_config_dir: Some(options.config_dir.clone()),
                project_dir: Some(options.project_root.clone()),
                read_env: false,
                ..Default::default()
            },
            credential_store: credentials.clone(),
            bus: bus.clone(),
            env: Arc::new(routing::ProcessEnv),
            model_catalog_dir: Some(
                project_models::ModelCatalogResolver::new(&options)
                    .for_project(Some(&options.project_root)),
            ),
        };
        let live = if fixed_model.is_none() && !config.providers.is_empty() {
            Some(Arc::new(runtime::compose::SwitchableModel::new(
                models::compose_production_model(&config, &production)?,
            )))
        } else {
            None
        };
        #[cfg(any(test, feature = "test-support"))]
        let routed_identity = live
            .as_ref()
            .map(|model| runtime::compose::ModelIdentity::Routed {
                profiles: model
                    .available_profiles()
                    .into_iter()
                    .map(|p| p.name)
                    .collect(),
                selected: [
                    ("orchestrator", runtime::Role::Orchestrator),
                    ("explorer", runtime::Role::Explorer),
                    ("worker", runtime::Role::Worker),
                    ("reviewer", runtime::Role::Reviewer),
                ]
                .into_iter()
                .map(|(name, role)| {
                    (
                        name.to_owned(),
                        runtime::AgentModel::selected_model(model.as_ref(), role, None),
                    )
                })
                .collect(),
            });
        let seeded = seed.is_some();
        let composition_input = RuntimeComposition {
            config: &config,
            user_config_dir: Some(options.config_dir.clone()),
            model_catalog_dir: Some(
                project_models::ModelCatalogResolver::new(&options)
                    .for_project(Some(&options.project_root)),
            ),
            bus: bus.clone(),
            executor: executor.clone(),
            credential_store: credentials.clone(),
            env: Arc::new(routing::ProcessEnv),
            model_source: match fixed_model {
                Some(model) => ModelSource::Fixed(model),
                None if config.providers.is_empty() => {
                    ModelSource::Fixed(Arc::new(runtime::compose::UnconfiguredModel))
                }
                None => ModelSource::Fixed(live.as_ref().unwrap().clone()),
            },
            workspace: if seeded { None } else { Some(workspace) },
        };
        let composed = match seed {
            Some(seed) => seed.compose(composition_input)?,
            None => runtime::compose_runtime(composition_input)?,
        };
        let artifacts = Arc::new(runtime::artifacts::store_with_refs(
            options.config_dir.join("artifacts"),
            handle.clone(),
        )?);
        let (project_models, reload_projects) =
            project_models::resolver(&options, credentials.clone(), bus.clone());
        let initial_runtime = if planned || recovery.is_some() {
            use std::os::unix::ffi::OsStrExt;
            let canonical = std::fs::canonicalize(&options.config_dir)?;
            let identity = format!("{:x}", Sha256::digest(canonical.as_os_str().as_bytes()));
            composed
                .runtime
                .with_learning_origin(runtime::learning_jobs::current_origin(
                    storage::learning_jobs::LearningOriginKind::Core,
                    Some(identity),
                ))
        } else {
            composed.runtime
        };
        let initial_runtime = initial_runtime.with_spawn_ledger(spawn_ledger.clone());
        // Learning configuration resolves a slug with git. Planned constructors
        // must not start even that child before Ready.
        let runtime = initial_runtime.with_mcp_service(mcp.runs.clone());
        let runtime = if planned {
            runtime
        } else {
            learning::configure(
                runtime,
                &config,
                storage_config.clone(),
                handle.clone(),
                &options.project_root,
            )
        }
        .with_project_models(project_models)
        .with_sandbox_root(options.project_root.clone())
        .with_artifact_store(artifacts.clone());
        let runtime = if seeded {
            runtime
        } else {
            runtime.with_run_store(RunStore::open(&storage_config, handle.clone())?)
        };
        let guard = runtime::thread_retention::TombstoneGuard::new(tombstones);
        if !seeded {
            runtime
                .set_tombstone_guard(guard.clone())
                .map_err(|_| "runtime tombstone guard already set")?;
        }
        let owner = if let Some(owner) = claimed_owner {
            owner
        } else {
            Arc::new(OwnerHost::open(
                &options.ownership_root,
                config.ownership.clone(),
                bus.clone(),
            )?)
        };
        if !seeded {
            owner
                .set_tombstone_guard(guard)
                .map_err(|_| "owner tombstone guard already set")?;
        }
        runtime.set_thread_scratch(owner.thread_scratch());
        // Archive-time scratch recovery confirms the archive from the saved
        // shared sidebar; the workspace area below sends the hints.
        crate::scratch_archive::install(&owner, Some(options.config_dir.join("sidebar.json")));
        let _ = evaluator.runtime.set(runtime.downgrade());
        let reload_models: crate::areas::ReloadModels = Arc::new(move || {
            if let Some(live) = &live {
                let config = config::Config::load_strict(&production.load_options)
                    .map_err(|e| e.to_string())?;
                live.replace(
                    models::compose_production_model(&config, &production)
                        .map_err(|e| e.to_string())?,
                );
            }
            reload_projects()
        });
        let _ = evaluator.reload.set({
            let reload = reload_models.clone();
            Box::new(move || reload())
        });
        let service = CommandService::new(runtime.clone(), tokio::runtime::Handle::current())
            .with_event_bus(bus.clone())
            .with_memory_storage(storage_config.clone())
            .with_team_writer(handle.clone())
            .with_ownership(owner.clone());
        service.bind_lane_delivery();
        let restart_gate = service.restart_gate();
        let projects = known_projects(&options)?;
        for (root, access) in projects.access {
            access.apply(&runtime, root);
        }
        let recovery_projects = projects.directory.clone();
        let command_host = CommandHost::new_with_epoch(service, successor_epoch)
            .with_projects(projects.directory)
            .with_legacy_import(
                options.ownership_root.clone(),
                options.config_dir.join("sidebar.json"),
            )
            .with_lane_sidebar_path(options.config_dir.join("sidebar.json"));
        // The area services get the handles built above, never new ones, and
        // start their background work only through `area_tasks`, which the
        // shutdown below stops and drains.
        let area_tasks = AreaTasks::new(tokio::runtime::Handle::current());
        let agent_observation = executor.agent_browser().map(|browser| {
            Arc::new(
                crate::areas::browser::observation::source::AgentObservationSource::new(
                    browser,
                    runtime.browser_execution_reader(),
                    command_host.projection().instance_epoch().get(),
                    area_tasks.clone(),
                ),
            ) as Arc<dyn crate::areas::browser::observation::AgentObservationPort>
        });
        #[cfg(any(test, feature = "test-support"))]
        let agent_observation = agent_observation_seam::injected().or(agent_observation);
        let areas = Areas::new(&AreaHandles {
            agent_observation,
            runtime: runtime.clone(),
            config: Arc::new(config.clone()),
            config_dir: options.config_dir.clone(),
            project_root: options.project_root.clone(),
            ownership_root: options.ownership_root.clone(),
            bus: bus.clone(),
            storage: handle.clone(),
            mcp: mcp.clone(),
            tasks: area_tasks.clone(),
            projection: command_host.projection().clone(),
            credentials,
            reload_models,
            evaluations: evaluations.clone(),
            evaluator: evaluator.clone(),
            database: storage_config.db_path.clone(),
            owner: owner.clone(),
            connections: Arc::default(),
            artifacts: artifacts.clone(),
            spawn_ledger: spawn_ledger.clone(),
            restart_gate,
        })?;
        // Before any run is restored: a restored run builds its first tool
        // catalog from the runtime as it is now. `None` offers nothing.
        if let Some(pages) = areas.artifact_pages() {
            runtime.set_artifact_pages(pages);
        }
        // What the saved sidebar grants (trust, allowed directories, the
        // read-only directories of threads) is declared before any run starts
        // or recovers, so a restart does not lose a permission the user gave.
        areas.workspace.declare_saved().map_err(|error| {
            format!("the saved workspace access could not be declared: {error:?}")
        })?;
        // Install the same live collector before any deferred automatic recovery.
        let graph_collector = if let Some(collector) = planned_collector {
            collector
        } else {
            let collector =
                recovery_disposal::LiveGraphCollector::new(&runtime, storage_config.clone());
            runtime.install_graph_observer(collector.clone())?;
            runtime.rebuild_graph_observation()?;
            collector
        };
        graph_collector
            .bind_epoch(command_host.projection().instance_epoch())
            .map_err(|_| "live graph collector epoch unavailable")?;
        let admin = crate::admin::AdminService::new(
            crate::admin::AdminAudit::new(
                Arc::new(handle.clone()),
                command_host.service().restart_gate(),
            ),
            area_tasks.clone(),
            command_host.projection().instance_epoch(),
        );
        admin.install_operation_reader(Arc::new(storage_config.clone()));
        let host = Arc::new(Mutex::new(
            command_host.with_areas(areas).with_admin(admin.clone()),
        ));
        #[cfg(any(test, feature = "test-support"))]
        let composition = CompositionInventory {
            tools: executor.tool_specs(),
            storage: storage_config.clone(),
            storage_writer: handle.clone(),
            model: routed_identity.unwrap_or(composed.model_identity),
        };
        let mut bootstrap = Self {
            host,
            runtime,
            owner,
            restart_storage: storage_config.clone(),
            storage_writer: handle.clone(),
            graph_collector,
            evaluator,
            evaluations: evaluations.clone(),
            config,
            services: services::BackgroundServices::default(),
            admin,
            area_tasks,
            deferred: None,
            planned_sandbox,
            spawn_ledger,
            mcp,
            recovery_diagnostics: None,
            usage_pricing: pricing,
            #[cfg(any(test, feature = "test-support"))]
            composition,
            bridge: Some(bridge),
            planned_exit: None,
            reclaim: None,
            thread_index_owner: None,
            history_owner: None,
            executor,
            _leases: leases,
        };
        if !planned {
            let live_core_epoch = bootstrap
                .host
                .lock()
                .unwrap()
                .projection()
                .instance_epoch()
                .get();
            if let Err(error) = bootstrap
                .admin
                .audit()
                .reconcile(storage::admin_audit::AuditReconcile::Cold { live_core_epoch })
                .await
            {
                tracing::warn!(?error, "management audit cold reconciliation deferred");
            }
        }
        bootstrap.deferred = Some(DeferredStartup {
            mode: if planned {
                core_startup::CoreBootMode::PlannedHandoff
            } else {
                core_startup::CoreBootMode::Cold
            },
            options,
            storage: storage_config,
            writer: handle,
            projects: recovery_projects,
            startup,
            bus,
            adapters,
            observe,
            artifacts,
        });
        if planned {
            bootstrap
                .host
                .lock()
                .unwrap()
                .service()
                .restart_gate()
                .transition(0, restart::HostRestartPhase::Preparing);
        } else {
            bootstrap.start_deferred(recovery).await?;
        }

        Ok(bootstrap)
    }

    /// Probe is owned by the same ledger as the successor runtime. No producers
    /// or graph activation may start until this explicit Ready-stage check passes.
    pub async fn probe_after_ready(&self) -> Result<(), BootstrapError> {
        if self
            .runtime
            .adopted_restart_transaction()?
            .ready_at_ms
            .is_none()
        {
            return Err("sandbox probe requires durable Ready".into());
        }
        if let Some(sandbox) = &self.planned_sandbox {
            let sandbox = sandbox.clone();
            tokio::task::spawn_blocking(move || sandbox.initialize()).await??;
        }
        Ok(())
    }
    pub fn planned_probe_stats(&self) -> Option<sandbox::ledger::ChildStats> {
        self.planned_sandbox
            .as_ref()
            .map(|s| s.ledger().child_stats())
    }

    /// The core calls this only after its durable Ready readback and boot guard.
    /// A pending host never scans or activates a graph, or starts a producer.
    pub async fn activate_after_ready(
        &mut self,
        recovery: &dyn core_startup::CoreStartupHook,
    ) -> Result<(), BootstrapError> {
        if self
            .runtime
            .adopted_restart_transaction()?
            .ready_at_ms
            .is_none()
        {
            return Err("planned activation requires durable Ready".into());
        }
        self.probe_after_ready().await?;
        self.owner.enable_normal_shutdown();
        self.start_deferred(Some(recovery)).await?;
        self.host
            .lock()
            .unwrap()
            .service()
            .restart_gate()
            .transition(0, restart::HostRestartPhase::Open);
        Ok(())
    }
    async fn start_deferred(
        &mut self,
        recovery: Option<&dyn core_startup::CoreStartupHook>,
    ) -> Result<(), BootstrapError> {
        let deferred = self.deferred.take().ok_or("startup already activated")?;
        // Composition only wires management capabilities. Their workers obey the
        // same activation boundary as every other producer, including after
        // durable Ready during a planned handoff. Taking deferred starts them once.
        self.admin.register_worker(&mut self.services);
        if matches!(deferred.mode, core_startup::CoreBootMode::PlannedHandoff) {
            self.runtime = learning::configure(
                self.runtime.clone(),
                &self.config,
                deferred.storage.clone(),
                deferred.writer.clone(),
                &deferred.options.project_root,
            );
        }
        if let (Some(recovery), Some(startup)) = (recovery, deferred.startup) {
            let work_changed = self.host.lock().unwrap().service().background_work_notify();
            let result = recovery
                .start(
                    core_startup::CoreStartupInputs {
                        mode: deferred.mode,
                        host: self.host.clone(),
                        options: deferred.options.clone(),
                        runtime: self.runtime.clone(),
                        owner: self.owner.clone(),
                        storage: deferred.storage.clone(),
                        writer: deferred.writer.clone(),
                        projects: deferred.projects,
                        startup,
                        work_changed,
                    },
                    &mut self.services,
                )
                .await;
            match result {
                Ok(diagnostics) => self.recovery_diagnostics = Some(diagnostics),
                Err(error) => {
                    let _ = self.shutdown().await;
                    return Err(error);
                }
            }
        }

        self.history_owner = Some(storage::history::background::Owner::start(
            deferred.storage.clone(),
            deferred.writer.clone(),
        )?);
        self.thread_index_owner = Some(thread_index::ThreadIndexOwner::start(
            deferred.writer.clone(),
        )?);
        self.reclaim = Some(runtime::thread_retention::ReclaimExecutor::spawn(
            runtime::thread_retention::ExecutorConfig::new(
                deferred.writer.clone(),
                deferred.storage.clone(),
                Some(deferred.artifacts),
            ),
        )?);
        host_services::start(
            &mut self.services,
            &deferred.options,
            &self.config,
            deferred.bus,
            self.usage_pricing.clone(),
            self.mcp.clone(),
        );
        self.services.start("evaluation", {
            let evaluations = self.evaluations.clone();
            |stop| async move {
                let _ = stop.await;
                evaluations.shutdown().await?;
                Ok(())
            }
        });
        if let Err(error) = learning::start(
            &mut self.services,
            &self.runtime,
            self.owner.clone(),
            &self.config,
            &deferred.storage,
            self.evaluations.clone(),
        ) {
            let _ = self.shutdown().await;
            return Err(error);
        }
        if let Some(observe) = deferred.observe {
            observe(&self.runtime);
        }
        if let Err(error) = lane_pr::start(
            &mut self.services,
            &self.runtime,
            &self.owner,
            self.config.pull_requests,
            deferred.adapters,
        ) {
            if let Err(drain) = self.shutdown().await {
                tracing::warn!(%drain, "lane/PR startup cleanup failed");
            }
            return Err(error);
        }
        Ok(())
    }

    pub fn restart_producers(&self) -> Vec<runtime::producer::ProducerGate> {
        let mut gates = vec![
            self.runtime.producer_gate(),
            self.evaluations.producers.clone(),
            self.area_tasks.producer_gate(),
            self.admin.producer_gate(),
        ];
        if let Some(pr) = self.runtime.pull_requests() {
            gates.push(pr.producer_gate());
        }
        gates
    }

    pub fn restart_preparing_services(&self) -> restart::PreparingServices {
        restart::PreparingServices {
            evaluations: self.evaluations.clone(),
        }
    }

    /// Woken when an area operation that counts as work ends, so the idle
    /// decision is taken again (ADR 0045 §2, §3).
    pub fn area_work_notify(&self) -> Arc<tokio::sync::Notify> {
        self.area_tasks.counted_ended_notify()
    }

    /// Runtime-only safe-point observation, separate from operator stop.
    /// This legacy observer does not close host ingress/producers and cannot
    /// issue a SealedRestart. Core callers must use RestartCoordinator for the
    /// authoritative host-level fence. Never use shutdown as a fallback.
    pub async fn quiesce_for_restart(&self) -> Result<(), BootstrapError> {
        self.runtime.quiesce().await.map_err(Into::into)
    }

    /// Operator-stop drain (ADR 0024); not a planned safe-point restart.
    pub async fn shutdown(&mut self) -> Result<(), BootstrapError> {
        // Publish unavailability before stopping successor/owner validity.
        self.graph_collector.invalidate();
        if let Some(mut history) = self.history_owner.take() {
            history.stop();
        }
        let mut failure = None;
        // Area tasks stop producing before the runtime drains; they are joined
        // below, with the other host-owned tasks.
        for producer in self.restart_producers() {
            producer.close();
        }
        self.host
            .lock()
            .unwrap()
            .service()
            .restart_gate()
            .transition(u64::MAX, restart::HostRestartPhase::ShuttingDown);
        if let Some(owner) = &mut self.thread_index_owner {
            lane_pr::remember(&mut failure, owner.stop());
        }
        self.area_tasks.request_stop();
        let tasks = self
            .host
            .lock()
            .unwrap()
            .service_mut()
            .take_background_tasks();
        lane_pr::remember(
            &mut failure,
            lane_pr::stop_and_drain(
                &mut self.services,
                &self.runtime,
                &self.owner,
                &self.executor,
            )
            .await,
        );
        for task in tasks {
            lane_pr::remember(&mut failure, task.await.map(|_| ()));
        }
        // Whether the runtime drained decides if the owner may be released; an
        // area task failing is reported but does not keep the owner.
        let drained = failure.is_none();
        lane_pr::remember(&mut failure, self.area_tasks.shutdown().await);
        if let Some(reclaim) = self.reclaim.take() {
            lane_pr::remember(&mut failure, reclaim.try_shutdown());
        }
        // Events published before a drain failure still need their bridge ack.
        if let Some(bridge) = &mut self.bridge {
            lane_pr::remember(&mut failure, bridge.flush());
        }
        lane_pr::remember(&mut failure, self.cleanup_planned_exit().await);
        if drained {
            match self.owner.release_ready() {
                Ok(true) => {
                    failure.get_or_insert_with(|| {
                        "owner still has active turns after runtime drain".into()
                    });
                }
                Ok(false) => {}
                Err(error) => lane_pr::remember(&mut failure, Err::<(), _>(error)),
            }
        }
        // Stop the archive worker and collector after producer/drain/release handling.
        lane_pr::remember(&mut failure, self.owner.stop_scratch_cleanup());
        if let Some(mut bridge) = self.bridge.take() {
            lane_pr::remember(&mut failure, bridge.try_shutdown());
        }
        failure.map_or(Ok(()), Err)
    }
}
