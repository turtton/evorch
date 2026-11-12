//! GUI-free owner composition and reusable construction parts (ADR 0045).
//! Learning, evaluation supervision and passive improvement/crash intake use
//! the same factories on core and legacy GUI. Every task has a stop/drain owner.
//!
//! Remaining GUI/core differences:
//! - Core loads strict explicit configuration, without environment overrides.
//! - Unconfigured core exposes no tools; GUI/demo keeps production tools.
//! - GUI owns retention/index policy; core only executes confirmed reclaims.
//! - GUI retains a 60s storage bridge tick; core retains its 5s tick.
//! - Project declarations are read-only: saved state cannot manufacture trust.
pub mod components;
pub mod evaluation;
mod host_services;
pub mod lane_pr;
pub mod learning;
#[cfg(test)]
mod learning_tests;
pub mod models;
mod project_models;
pub mod recovery;
#[cfg(test)]
mod recovery_06a_tests;
pub mod services;
#[cfg(test)]
mod tests;
use crate::areas::{AreaHandles, AreaTasks, Areas};
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
    pub owner: Arc<OwnerHost>,
    pub config: config::Config,
    pub services: services::BackgroundServices,
    pub mcp: runtime::mcp::McpHostServices,
    area_tasks: AreaTasks,
    pub usage_pricing: recorder::pricing::SharedUsagePricing,
    pub evaluator: Arc<evaluation::EvaluatorHost>,
    evaluations: eval::evaluator::EvaluationService,
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
        Self::open_composed(
            options,
            None,
            None,
            lane_pr::PullRequestAdapters::default(),
            None,
        )
        .await
    }

    /// The production composition with the model and the per-run sandbox replaced, so a test
    /// can drive the real owner, storage, command service and lane delivery wiring on a
    /// machine with neither a provider nor bwrap.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn open_with_test_seams(
        options: BootstrapOptions,
        seams: TestSeams,
    ) -> Result<Self, BootstrapError> {
        Self::open_composed(
            options,
            Some(seams),
            None,
            lane_pr::PullRequestAdapters::default(),
            None,
        )
        .await
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn open_with_mcp_test_seams(
        options: BootstrapOptions,
        seams: TestSeams,
        mcp: runtime::mcp::McpHostServices,
    ) -> Result<Self, BootstrapError> {
        Self::open_composed(
            options,
            Some(seams),
            Some(mcp),
            lane_pr::PullRequestAdapters::default(),
            None,
        )
        .await
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn open_with_service_seams(
        options: BootstrapOptions,
        seams: TestSeams,
        adapters: lane_pr::PullRequestAdapters,
    ) -> Result<Self, BootstrapError> {
        Self::open_composed(options, Some(seams), None, adapters, None).await
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
            adapters,
            Some(Box::new(observe)),
        )
        .await
    }

    async fn open_composed(
        options: BootstrapOptions,
        seams: Option<TestSeams>,
        mcp: Option<runtime::mcp::McpHostServices>,
        adapters: lane_pr::PullRequestAdapters,
        observe: Option<RuntimeObserver>,
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
            None => host_services::mcp(&options.config_dir).await?,
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
                evaluation_tools,
            )?
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
            model_catalog_dir: None,
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
        let composed = runtime::compose_runtime(RuntimeComposition {
            config: &config,
            user_config_dir: Some(options.config_dir.clone()),
            model_catalog_dir: Some(options.config_dir.join("catalog")),
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
            workspace: Some(workspace),
        })?;
        let artifacts = Arc::new(runtime::artifacts::store_with_refs(
            options.config_dir.join("artifacts"),
            handle.clone(),
        )?);
        let (project_models, reload_projects) =
            project_models::resolver(&options, credentials, bus.clone());
        let runtime = learning::configure(
            composed.runtime.with_mcp_service(mcp.runs.clone()),
            &config,
            storage_config.clone(),
            handle.clone(),
            &options.project_root,
        )
        .with_project_models(project_models)
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
        let _ = evaluator.runtime.set(runtime.downgrade());
        let _ = evaluator.reload.set(Box::new(move || {
            if let Some(live) = &live {
                let config = config::Config::load_strict(&production.load_options)
                    .map_err(|e| e.to_string())?;
                live.replace(
                    models::compose_production_model(&config, &production)
                        .map_err(|e| e.to_string())?,
                );
            }
            reload_projects()
        }));
        let service = CommandService::new(runtime.clone(), tokio::runtime::Handle::current())
            .with_event_bus(bus.clone())
            .with_memory_storage(storage_config.clone())
            .with_team_writer(handle.clone())
            .with_ownership(owner.clone());
        service.bind_lane_delivery();
        let projects = known_projects(&options)?;
        for (root, access) in projects.access {
            access.apply(&runtime, root);
        }
        let command_host = CommandHost::new(service)
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
        let areas = Areas::new(&AreaHandles {
            runtime: runtime.clone(),
            config: Arc::new(config.clone()),
            config_dir: options.config_dir.clone(),
            project_root: options.project_root.clone(),
            bus: bus.clone(),
            storage: handle.clone(),
            mcp: mcp.clone(),
            tasks: area_tasks.clone(),
            projection: command_host.projection().clone(),
        })?;
        let host = Arc::new(Mutex::new(command_host.with_areas(areas)));
        #[cfg(any(test, feature = "test-support"))]
        let composition = CompositionInventory {
            tools: executor.tool_specs(),
            storage: storage_config.clone(),
            model: routed_identity.unwrap_or(composed.model_identity),
        };
        let reclaim = runtime::thread_retention::ReclaimExecutor::spawn(
            runtime::thread_retention::ExecutorConfig::new(
                handle,
                storage_config.clone(),
                Some(artifacts),
            ),
        )?;
        let mut bootstrap = Self {
            host,
            runtime,
            owner,
            evaluator,
            evaluations: evaluations.clone(),
            config,
            services: services::BackgroundServices::default(),
            area_tasks,
            mcp,
            usage_pricing: pricing,
            #[cfg(any(test, feature = "test-support"))]
            composition,
            bridge: Some(bridge),
            reclaim: Some(reclaim),
            executor,
            _leases: leases,
        };
        host_services::start(
            &mut bootstrap.services,
            &options,
            &bootstrap.config,
            bus,
            bootstrap.usage_pricing.clone(),
            bootstrap.mcp.clone(),
        );
        bootstrap.services.start("evaluation", {
            let evaluations = evaluations.clone();
            |stop| async move {
                let _ = stop.await;
                evaluations.shutdown().await?;
                Ok(())
            }
        });
        if let Err(error) = learning::start(
            &mut bootstrap.services,
            &bootstrap.runtime,
            bootstrap.owner.clone(),
            &bootstrap.config,
            &storage_config,
            evaluations,
        ) {
            let _ = bootstrap.shutdown().await;
            return Err(error);
        }
        if let Some(observe) = observe {
            observe(&bootstrap.runtime);
        }
        if let Err(error) = lane_pr::start(
            &mut bootstrap.services,
            &bootstrap.runtime,
            &bootstrap.owner,
            bootstrap.config.pull_requests,
            adapters,
        ) {
            if let Err(drain) = bootstrap.shutdown().await {
                tracing::warn!(%drain, "lane/PR startup cleanup failed");
            }
            return Err(error);
        }
        Ok(bootstrap)
    }

    pub fn busy(&self) -> bool {
        // TODO(P9): once safe-point restart/auto-resume can restore an ordinary
        // chat's Waiting state, count that idle conversation as no work. Until
        // then retain all Waiting runs conservatively to avoid losing them.
        self.evaluations.is_busy()
            || self.runtime.has_paused_recovery()
            || self.runtime.pending_lane_stop_writes() != 0
            || !self.runtime.pending_lane_admissions().is_empty()
            || self.host.lock().unwrap().service().has_background_work()
            || self.runtime.list_agents().iter().any(|r| {
                self.executor.has_running_shell_jobs(&r.run_id.to_string())
                    || matches!(
                        r.phase,
                        AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting
                    )
            })
    }

    /// Planned restart preparation (ADR 0045 §8), separate from operator stop.
    /// Success parks runs and leaves services, ownership and the bridge alive;
    /// the restart transaction in PR 07 owns handoff/exit. Never call shutdown
    /// as a fallback when this fails or is still waiting for a provider/tool.
    pub async fn quiesce_for_restart(&self) -> Result<(), BootstrapError> {
        self.runtime.quiesce().await.map_err(Into::into)
    }

    /// Operator-stop drain (ADR 0024); not a planned safe-point restart.
    pub async fn shutdown(&mut self) -> Result<(), BootstrapError> {
        let mut failure = None;
        // Area tasks stop producing before the runtime drains; they are joined
        // below, with the other host-owned tasks.
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
            reclaim.shutdown();
        }
        // Events published before a drain failure still need their bridge ack.
        if let Some(bridge) = &mut self.bridge {
            lane_pr::remember(&mut failure, bridge.flush());
        }
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
        if let Some(mut bridge) = self.bridge.take() {
            bridge.shutdown();
        }
        failure.map_or(Ok(()), Err)
    }
}
