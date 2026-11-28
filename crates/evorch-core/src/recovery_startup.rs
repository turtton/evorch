//! Core-only startup coordinator, installed by the production core bootstrap.
//! Legacy embedded hosts do not run graph recovery.
//! The caller must complete maintenance, downgrade/reopen under shared leases,
//! issue startup evidence, attach RunStore (high watermarks), then preload catalog.
use runtime::{
    AgentRuntime, CurrentRecoveryAuthority, PausedRecoveryComponent, RecoveryModels, RunId,
    quiesce::{RecoveryReport, RunRecoveryRecord},
};
use std::collections::BTreeSet;
use storage::recovery::{RecoveryClaimState, RecoveryManifestRecord};
use workbench_host::{
    bootstrap::{
        BootstrapOptions,
        recovery::{
            authority::{CurrentAuthorityProblem, CurrentAuthorityProvider},
            bindings,
            catalog::RecoveryCatalog,
        },
    },
    projects::ProjectDirectory,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    AwaitingAuthority,
    Prepared,
    AlreadyActivated,
    Released,
    Superseded,
    NeedsAttention,
}
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ComponentStatus {
    pub manifest_id: String,
    pub runs: Vec<RunId>,
    pub disposition: Disposition,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct StartupScan {
    pub planning: RecoveryReport,
    pub components: Vec<ComponentStatus>,
    pub unreadable_claims: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupBlocked {
    CurrentBindingRequired,
    CurrentSettingsRequired,
    CliContinuityRequired,
    TeamReconciliationRequired,
    StorageUnavailable,
    CatalogUnavailable,
    AlreadyActivated,
    Released,
    NeedsAttention,
}
impl From<CurrentAuthorityProblem> for StartupBlocked {
    fn from(value: CurrentAuthorityProblem) -> Self {
        match value {
            CurrentAuthorityProblem::CurrentBindingRequired => Self::CurrentBindingRequired,
            CurrentAuthorityProblem::CurrentSettingsRequired => Self::CurrentSettingsRequired,
            CurrentAuthorityProblem::CliContinuityRequired => Self::CliContinuityRequired,
            CurrentAuthorityProblem::TeamReconciliationRequired => Self::TeamReconciliationRequired,
            CurrentAuthorityProblem::StorageUnavailable => Self::StorageUnavailable,
        }
    }
}
pub struct StartupInputs<'a> {
    pub runtime: &'a AgentRuntime,
    pub storage: &'a storage::StorageConfig,
    pub writer: &'a storage::StorageHandle,
    pub options: &'a BootstrapOptions,
    pub projects: &'a (dyn ProjectDirectory + Sync),
}
/// Owns the process-local lease proof and frozen lane exclusions. No polling task
/// is detached; the core's managed service owns bounded activation jobs.
pub struct StartupCoordinator<'a> {
    inputs: StartupInputs<'a>,
    startup: storage::RecoveryStartupComplete,
    catalog: RecoveryCatalog,
}
/// Owned, single-job inputs issued only after the startup coordinator installed
/// the lane exclusion fence. They do not grant any new recovery authority.
pub struct StartupJobContext {
    pub runtime: AgentRuntime,
    pub storage: storage::StorageConfig,
    pub writer: storage::StorageHandle,
    pub options: BootstrapOptions,
    pub projects: workbench_host::projects::StaticProjects,
    startup: storage::RecoveryStartupComplete,
    catalog: RecoveryCatalog,
}
impl StartupJobContext {
    pub fn coordinator(&self) -> Result<StartupCoordinator<'_>, StartupBlocked> {
        let startup = self
            .startup
            .try_clone()
            .map_err(|_| StartupBlocked::StorageUnavailable)?;
        self.writer
            .validate_recovery_startup(
                &startup,
                &self.storage.db_path,
                &self.options.ownership_root,
            )
            .map_err(|_| StartupBlocked::StorageUnavailable)?;
        self.runtime
            .validate_recovery_startup(&startup, &self.options.ownership_root)
            .map_err(|_| StartupBlocked::NeedsAttention)?;
        // The issuing coordinator already installed the immutable lane fence.
        // Reinstalling it here would reject every later manual job.
        Ok(StartupCoordinator {
            inputs: StartupInputs {
                runtime: &self.runtime,
                storage: &self.storage,
                writer: &self.writer,
                options: &self.options,
                projects: &self.projects,
            },
            startup,
            catalog: self.catalog.clone(),
        })
    }
}
impl<'a> StartupCoordinator<'a> {
    pub fn new(
        inputs: StartupInputs<'a>,
        startup: storage::RecoveryStartupComplete,
        catalog: RecoveryCatalog,
    ) -> Result<Self, StartupBlocked> {
        inputs
            .writer
            .validate_recovery_startup(
                &startup,
                &inputs.storage.db_path,
                &inputs.options.ownership_root,
            )
            .map_err(|_| StartupBlocked::StorageUnavailable)?;
        inputs
            .runtime
            .validate_recovery_startup(&startup, &inputs.options.ownership_root)
            .map_err(|_| StartupBlocked::NeedsAttention)?;
        inputs
            .runtime
            .fence_graph_lane_recovery()
            .map_err(|_| StartupBlocked::NeedsAttention)?;
        Ok(Self {
            inputs,
            startup,
            catalog,
        })
    }
    /// The actor creates a fresh context for each admitted job. This factory is
    /// not available to host callers and cannot bypass initial fence installation.
    pub(crate) fn job_context(&self) -> Result<StartupJobContext, StartupBlocked> {
        let mut projects = workbench_host::projects::StaticProjects::default();
        for (id, root) in self.inputs.projects.entries() {
            projects.insert(&id, root);
        }
        Ok(StartupJobContext {
            runtime: self.inputs.runtime.clone(),
            storage: self.inputs.storage.clone(),
            writer: self.inputs.writer.clone(),
            options: self.inputs.options.clone(),
            projects,
            startup: self
                .startup
                .try_clone()
                .map_err(|_| StartupBlocked::StorageUnavailable)?,
            catalog: self.catalog.clone(),
        })
    }
    /// Header-first observation. Activated is never treated as retryable and a
    /// corrupt claim blocks mutation rather than disappearing behind its decoder.
    pub fn scan(&self) -> Result<StartupScan, StartupBlocked> {
        let db = storage::Database::open(self.inputs.storage)
            .map_err(|_| StartupBlocked::StorageUnavailable)?;
        let headers = db
            .recovery_graph_headers()
            .map_err(|_| StartupBlocked::StorageUnavailable)?;
        let claims = db
            .recovery_claim_loads()
            .map_err(|_| StartupBlocked::StorageUnavailable)?;
        let plan = self
            .inputs
            .runtime
            .recovery_plan()
            .map_err(|_| StartupBlocked::NeedsAttention)?;
        let unreadable_claims = claims.iter().any(|(_, claim)| claim.is_err());
        let manifests = db
            .recovery_manifests()
            .map_err(|_| StartupBlocked::StorageUnavailable)?;
        let mut components = vec![];
        // Decide format and terminal disposition before looking at any recipe.
        // In particular, completed old transactions need no compatible decoder.
        for header in headers {
            if header.restart.as_ref().is_some_and(|restart| {
                restart.phase == storage::recovery::restart::RestartPhase::Completed
            }) {
                continue;
            }
            let id = &header.manifest_id;
            let record = if header.is_current() {
                manifests
                    .iter()
                    .find(|load| load.manifest_id == *id)
                    .and_then(|load| load.record.as_ref().ok())
            } else {
                None
            };
            let disposition = if header.superseded {
                Disposition::Superseded
            } else {
                match &record {
                    Some(record) => match claims
                        .iter()
                        .find(|(manifest, _)| manifest == id)
                        .map(|(_, claim)| claim.as_ref().map(|c| &c.state))
                    {
                        Some(Ok(RecoveryClaimState::Prepared))
                            if self.inputs.runtime.require_planned_manifest(id).is_ok() =>
                        {
                            Disposition::Prepared
                        }
                        Some(Ok(RecoveryClaimState::Prepared)) => Disposition::NeedsAttention,
                        Some(Ok(RecoveryClaimState::Activated)) => Disposition::AlreadyActivated,
                        Some(Ok(RecoveryClaimState::Released { .. })) => Disposition::Released,
                        Some(Err(_)) => Disposition::NeedsAttention,
                        None if record.superseded => Disposition::Superseded,
                        None if plan.components.iter().any(|c| c.manifest_id == *id) => {
                            Disposition::AwaitingAuthority
                        }
                        None => Disposition::NeedsAttention,
                    },
                    None => Disposition::NeedsAttention,
                }
            };
            let runs = db
                .recovery_manifest_run_ids(id)
                .map_err(|_| StartupBlocked::StorageUnavailable)?
                .into_iter()
                .filter_map(|run| run.parse().ok())
                .collect();
            components.push(ComponentStatus {
                manifest_id: header.manifest_id,
                runs,
                disposition,
            });
        }
        Ok(StartupScan {
            planning: plan.report,
            components,
            unreadable_claims,
        })
    }
    /// Read back a consumed activation handle and clean only coherent, local,
    /// unpublished Activated state. This never grants authority to retry/release.
    pub fn cleanup_activated(&self, id: &str) -> Result<(), StartupBlocked> {
        let claim = self.claim(id)?.ok_or(StartupBlocked::NeedsAttention)?;
        if claim.state != RecoveryClaimState::Activated {
            return Err(StartupBlocked::NeedsAttention);
        }
        let observed = self
            .inputs
            .runtime
            .recover_recovery_claim(
                &claim.claim_id,
                self.startup
                    .try_clone()
                    .map_err(|_| StartupBlocked::StorageUnavailable)?,
                &self.inputs.options.ownership_root,
                // Observation/cleanup changes no durable state. No saved or current
                // capability is needed or inferred; a race to Prepared cannot adopt.
                &CurrentRecoveryAuthority {
                    members: Default::default(),
                },
            )
            .map_err(|_| StartupBlocked::NeedsAttention)?;
        match observed {
            runtime::RecoveryClaimHandle::Quarantined(handle)
                if handle.state() == runtime::RecoveryCleanupState::ActivatedUnpublished =>
            {
                self.inputs
                    .runtime
                    .cleanup_recovery_claim(&handle)
                    .map_err(|_| StartupBlocked::NeedsAttention)
            }
            _ => Err(StartupBlocked::NeedsAttention),
        }
    }
    /// Conditional durable release even when adoption/descriptor validation failed.
    /// Only current binding grants mutation authority; the writer atomically
    /// proves every member unexecuted. Activated/mixed/corrupt claims stay quarantined.
    pub fn decline(&self, id: &str) -> Result<(), StartupBlocked> {
        let scan = self.scan()?;
        if !scan
            .components
            .iter()
            .any(|c| c.manifest_id == id && c.disposition == Disposition::Prepared)
        {
            return Err(StartupBlocked::NeedsAttention);
        }
        let manifest = self.manifest(id)?;
        let records = manifest
            .members
            .iter()
            .map(|m| {
                serde_json::from_str::<RunRecoveryRecord>(&m.record_json)
                    .map_err(|_| StartupBlocked::NeedsAttention)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let resume = manifest
            .members
            .iter()
            .filter(|m| m.resume)
            .map(|m| {
                m.run_id
                    .parse::<RunId>()
                    .map_err(|_| StartupBlocked::NeedsAttention)
            })
            .collect::<Result<BTreeSet<_>, _>>()?;
        let bindings = bindings::load(self.inputs.storage, self.inputs.projects)
            .map_err(|_| StartupBlocked::StorageUnavailable)?;
        let authority = CurrentAuthorityProvider {
            runtime: self.inputs.runtime,
            bindings: &bindings,
            storage: self.inputs.storage,
            writer: self.inputs.writer,
            config_dir: &self.inputs.options.config_dir,
        }
        .resolve_for_release(&records, &resume)?;
        let claim = self.claim(id)?.ok_or(StartupBlocked::NeedsAttention)?;
        self.inputs
            .writer
            .release_prepared_claim_if_unexecuted(
                claim.claim_id.clone(),
                "core_startup_declined".into(),
            )
            .map_err(|_| StartupBlocked::NeedsAttention)?;
        if let Ok(runtime::RecoveryClaimHandle::Quarantined(handle)) =
            self.inputs.runtime.recover_recovery_claim(
                &claim.claim_id,
                self.startup
                    .try_clone()
                    .map_err(|_| StartupBlocked::StorageUnavailable)?,
                &self.inputs.options.ownership_root,
                &authority,
            )
        {
            self.inputs
                .runtime
                .cleanup_recovery_claim(&handle)
                .map_err(|_| StartupBlocked::NeedsAttention)?;
        }
        Ok(())
    }
    fn claim(&self, id: &str) -> Result<Option<storage::recovery::RecoveryClaim>, StartupBlocked> {
        let claims = storage::Database::open(self.inputs.storage)
            .map_err(|_| StartupBlocked::StorageUnavailable)?
            .recovery_claim_loads()
            .map_err(|_| StartupBlocked::StorageUnavailable)?;
        // An unknown relational identity cannot safely be assigned to a peer.
        // Known corrupt claims quarantine only their own component.
        if claims.iter().any(|(manifest, claim)| {
            (manifest == "unreadable" || manifest.is_empty()) && claim.is_err()
        }) {
            return Err(StartupBlocked::NeedsAttention);
        }
        claims
            .into_iter()
            .find(|(manifest, _)| manifest == id)
            .map(|(_, claim)| claim.map_err(|_| StartupBlocked::NeedsAttention))
            .transpose()
    }
    fn manifest(&self, id: &str) -> Result<RecoveryManifestRecord, StartupBlocked> {
        storage::Database::open(self.inputs.storage)
            .map_err(|_| StartupBlocked::StorageUnavailable)?
            .recovery_manifests()
            .map_err(|_| StartupBlocked::StorageUnavailable)?
            .into_iter()
            .find(|load| load.manifest_id == id)
            .ok_or(StartupBlocked::NeedsAttention)?
            .record
            .map_err(|_| StartupBlocked::NeedsAttention)
    }
    /// Explicit staged prepare/adopt. The same current project model pins are
    /// published and retained through compatibility validation and later execution.
    /// Prepared claims are adopted with their existing revision/incarnation pair.
    pub fn prepare(&self, id: &str) -> Result<PreparedStartupComponent, StartupBlocked> {
        self.prepare_current(id, false)
    }

    /// Stage only an existing Prepared claim for explicit, reserved Resume.
    /// This creates neither a claim nor a thread-root binding. Current execution authority and planned gates still apply.
    /// The startup proof and catalog remain private to this coordinator.
    pub fn prepare_existing(&self, id: &str) -> Result<PreparedStartupComponent, StartupBlocked> {
        self.prepare_current(id, true)
    }

    fn prepare_current(
        &self,
        id: &str,
        existing_only: bool,
    ) -> Result<PreparedStartupComponent, StartupBlocked> {
        // Select once before any preparation side effect. A missing or changed
        // claim must not fall through to the automatic new-Prepared branch.
        let selected = if existing_only {
            let claim = self.claim(id)?.ok_or(StartupBlocked::NeedsAttention)?;
            match claim.state {
                RecoveryClaimState::Prepared => Some(claim),
                RecoveryClaimState::Activated => return Err(StartupBlocked::AlreadyActivated),
                RecoveryClaimState::Released { .. } => return Err(StartupBlocked::Released),
            }
        } else {
            None
        };
        let scan = self.scan()?;
        let status = scan
            .components
            .iter()
            .find(|c| c.manifest_id == id)
            .ok_or(StartupBlocked::NeedsAttention)?;
        match status.disposition {
            Disposition::AlreadyActivated => return Err(StartupBlocked::AlreadyActivated),
            Disposition::Released => return Err(StartupBlocked::Released),
            Disposition::Prepared | Disposition::AwaitingAuthority => {}
            _ => return Err(StartupBlocked::NeedsAttention),
        }
        if !self.catalog.is_available() {
            return Err(StartupBlocked::CatalogUnavailable);
        }
        let manifest = self.manifest(id)?;
        let records = manifest
            .members
            .iter()
            .map(|m| {
                serde_json::from_str::<RunRecoveryRecord>(&m.record_json)
                    .map_err(|_| StartupBlocked::NeedsAttention)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let resume = manifest
            .members
            .iter()
            .filter(|m| m.resume)
            .map(|m| {
                m.run_id
                    .parse::<RunId>()
                    .map_err(|_| StartupBlocked::NeedsAttention)
            })
            .collect::<Result<BTreeSet<_>, _>>()?;
        let bindings = bindings::load(self.inputs.storage, self.inputs.projects)
            .map_err(|_| StartupBlocked::StorageUnavailable)?;
        let provider = CurrentAuthorityProvider {
            runtime: self.inputs.runtime,
            bindings: &bindings,
            storage: self.inputs.storage,
            writer: self.inputs.writer,
            config_dir: &self.inputs.options.config_dir,
        };
        let authority = provider.resolve_current(&records, &resume)?;
        if authority.members.values().any(|config| {
            !self
                .catalog
                .is_available_for_project(config.project_root.as_deref())
        }) {
            return Err(StartupBlocked::CatalogUnavailable);
        }
        let mut command_bindings = Vec::new();
        // The current host directory (validated by the authority provider), not
        // the saved recipe alone, grants the command binding.
        for record in records.iter().filter(|record| record.parent.is_none()) {
            if authority.members.contains_key(&record.run_id) {
                let thread = record
                    .thread
                    .as_deref()
                    .ok_or(StartupBlocked::CurrentBindingRequired)?;
                let binding = bindings
                    .get(thread)
                    .ok_or(StartupBlocked::CurrentBindingRequired)?;
                if binding.root != record.run_id || binding.project_root != record.project {
                    return Err(StartupBlocked::CurrentBindingRequired);
                }
                command_bindings.push((thread.to_owned(), binding));
                if !existing_only {
                    self.inputs
                        .runtime
                        .bind_thread_root(thread, record.run_id)
                        .map_err(|_| StartupBlocked::NeedsAttention)?;
                }
            }
        }
        let models = self.catalog.pin(self.inputs.runtime, &authority);
        let existing = if existing_only {
            selected
        } else {
            self.claim(id)?
        };
        let claim = if let Some(claim) = existing {
            match claim.state {
                RecoveryClaimState::Prepared => claim,
                RecoveryClaimState::Activated => return Err(StartupBlocked::AlreadyActivated),
                RecoveryClaimState::Released { .. } => return Err(StartupBlocked::Released),
            }
        } else {
            if existing_only {
                return Err(StartupBlocked::NeedsAttention);
            }
            let component = self
                .inputs
                .runtime
                .recovery_plan()
                .map_err(|_| StartupBlocked::NeedsAttention)?
                .components
                .into_iter()
                .find(|component| component.manifest_id == id)
                .ok_or(StartupBlocked::NeedsAttention)?;
            let paused = self
                .inputs
                .runtime
                .prepare_component(
                    &component,
                    self.startup
                        .try_clone()
                        .map_err(|_| StartupBlocked::StorageUnavailable)?,
                    &self.inputs.options.ownership_root,
                )
                .map_err(|_| StartupBlocked::NeedsAttention)?;
            paused.claim().clone()
        };
        let proof = self
            .inputs
            .writer
            .validate_prepared_claim(
                claim.claim_id,
                &self.startup,
                &self.inputs.options.ownership_root,
            )
            .map_err(|_| StartupBlocked::NeedsAttention)?;
        let mut paused = self
            .inputs
            .runtime
            .adopt_prepared_claim_with_models(
                proof,
                self.startup
                    .try_clone()
                    .map_err(|_| StartupBlocked::StorageUnavailable)?,
                &self.inputs.options.ownership_root,
                &authority,
                &models,
            )
            .map_err(|_| StartupBlocked::NeedsAttention)?;
        self.inputs
            .runtime
            .restore_relations_and_continuations(&mut paused)
            .map_err(|_| StartupBlocked::NeedsAttention)?;
        self.inputs
            .runtime
            .prepare_planned_component_tool_evidence(&paused, &authority, &models)
            .inspect_err(|error| tracing::warn!(%error, "planned startup tool evidence rejected"))
            .map_err(|_| StartupBlocked::NeedsAttention)?;
        Ok(PreparedStartupComponent {
            paused,
            authority,
            models,
            command_bindings,
        })
    }
}
/// Opaque, non-serializable process-local staged activation capability. It is
/// never produced by deserializing a scan/report or CLI continuity assertion.
pub struct PreparedStartupComponent {
    paused: PausedRecoveryComponent,
    authority: CurrentRecoveryAuthority,
    models: RecoveryModels,
    command_bindings: Vec<(String, workbench_host::lane::ThreadBinding)>,
}
impl PreparedStartupComponent {
    pub fn command_bindings(&self) -> Vec<(String, workbench_host::lane::ThreadBinding)> {
        self.command_bindings.clone()
    }
    pub fn claim_id(&self) -> &str {
        &self.paused.claim().claim_id
    }
    pub fn release(&self, runtime: &AgentRuntime) -> Result<(), StartupBlocked> {
        runtime
            .release_prepared_component_with_authority(
                &self.paused,
                &self.authority,
                "core_startup_declined",
            )
            .map_err(|_| StartupBlocked::NeedsAttention)
    }
    /// Activate only after current authority and full-component preparation.
    /// Keep the success reservation until command binding publication settles.
    /// A definite failure returns the original reservation: retain it through
    /// decline/cleanup. On Unknown, the runtime supervisor retains it instead.
    pub fn activate(
        self,
        runtime: &AgentRuntime,
        reservation: impl Into<runtime::ActivationReservation>,
    ) -> Result<(Vec<RunId>, runtime::ActivatedReservation), runtime::ActivationFailure> {
        runtime.activate_component_with_models(
            self.paused,
            self.authority,
            self.models,
            reservation,
        )
    }
}

#[cfg(test)]
#[path = "recovery_startup_tests.rs"]
mod tests;
