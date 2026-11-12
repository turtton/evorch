//! Core-only startup coordinator, installed by the production core bootstrap.
//! Legacy embedded hosts do not run graph recovery.
//! The caller must complete maintenance, downgrade/reopen under shared leases,
//! issue startup evidence, attach RunStore (high watermarks), then preload catalog.
use runtime::{
    AgentRuntime, CurrentRecoveryAuthority, PausedRecoveryComponent, RecoveryModels, RunId,
    ownership::OwnerHost,
    quiesce::{RecoveryReport, RunRecoveryRecord},
};
use std::{collections::BTreeSet, sync::Arc};
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
    OwnerBlocked,
    CurrentBindingRequired,
    CurrentSettingsRequired,
    CliContinuityRequired,
    TeamReconciliationRequired,
    StorageUnavailable,
    AlreadyActivated,
    Released,
    NeedsAttention,
}
impl From<CurrentAuthorityProblem> for StartupBlocked {
    fn from(value: CurrentAuthorityProblem) -> Self {
        match value {
            CurrentAuthorityProblem::OwnerBlocked => Self::OwnerBlocked,
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
    pub owner: &'a Arc<OwnerHost>,
    pub storage: &'a storage::StorageConfig,
    pub writer: &'a storage::StorageHandle,
    pub options: &'a BootstrapOptions,
    pub projects: &'a (dyn ProjectDirectory + Sync),
}
/// Owns the process-local lease proof and frozen lane exclusions. No polling task
/// is detached; the core's managed service owns owner-blocked retries.
pub struct StartupCoordinator<'a> {
    inputs: StartupInputs<'a>,
    startup: storage::RecoveryStartupComplete,
    catalog: RecoveryCatalog,
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
    /// Payload-free observation. Activated is never treated as retryable and a
    /// corrupt claim blocks mutation rather than disappearing behind its decoder.
    pub fn scan(&self) -> Result<StartupScan, StartupBlocked> {
        let db = storage::Database::open(self.inputs.storage)
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
        let mut components = vec![];
        for load in db
            .recovery_manifests()
            .map_err(|_| StartupBlocked::StorageUnavailable)?
        {
            let disposition = match &load.record {
                Ok(record) => match claims
                    .iter()
                    .find(|(manifest, _)| manifest == &load.manifest_id)
                    .map(|(_, claim)| claim.as_ref().map(|c| &c.state))
                {
                    Some(Ok(RecoveryClaimState::Prepared)) => Disposition::Prepared,
                    Some(Ok(RecoveryClaimState::Activated)) => Disposition::AlreadyActivated,
                    Some(Ok(RecoveryClaimState::Released { .. })) => Disposition::Released,
                    Some(Err(_)) => Disposition::NeedsAttention,
                    None if record.superseded => Disposition::Superseded,
                    None if plan
                        .components
                        .iter()
                        .any(|c| c.manifest_id == load.manifest_id) =>
                    {
                        Disposition::AwaitingAuthority
                    }
                    None => Disposition::NeedsAttention,
                },
                Err(_) => Disposition::NeedsAttention,
            };
            let runs = load
                .record
                .as_ref()
                .map(|m| {
                    m.members
                        .iter()
                        .filter_map(|m| m.run_id.parse().ok())
                        .collect()
                })
                .unwrap_or_default();
            components.push(ComponentStatus {
                manifest_id: load.manifest_id,
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
    /// Conditional durable release even when adoption/descriptor validation failed.
    /// Only current binding/owner grants mutation authority; the writer atomically
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
            owner: self.inputs.owner,
            bindings: &bindings,
            storage: self.inputs.storage,
            writer: self.inputs.writer,
            config_dir: &self.inputs.options.config_dir,
        }
        .resolve_for_release(&records, &resume)?;
        let claim = self.claim(id)?.ok_or(StartupBlocked::NeedsAttention)?;
        // One registry transaction per path; repeated child permits on the same
        // registry must not deadlock by reacquiring its mutation lock.
        let permits = authority
            .members
            .values()
            .map(|c| c.ownership.as_ref().ok_or(StartupBlocked::OwnerBlocked))
            .collect::<Result<Vec<_>, _>>()?;
        let mut paths = std::collections::BTreeMap::new();
        for permit in &permits {
            paths.entry(permit.registry_path.clone()).or_insert(*permit);
        }
        {
            let mut guards = paths
                .values()
                .map(|p| p.mutation_guard().map_err(|_| StartupBlocked::OwnerBlocked))
                .collect::<Result<Vec<_>, _>>()?;
            // All distinct generations, not just the first thread, are checked
            // under the same registry guard before crossing the writer boundary.
            for permit in &permits {
                let position = paths
                    .keys()
                    .position(|path| path == &permit.registry_path)
                    .ok_or(StartupBlocked::OwnerBlocked)?;
                guards[position]
                    .guard_generation(permit)
                    .map_err(|_| StartupBlocked::OwnerBlocked)?;
            }
            self.inputs
                .writer
                .release_prepared_claim_if_unexecuted(
                    claim.claim_id.clone(),
                    "core_startup_declined".into(),
                )
                .map_err(|_| StartupBlocked::NeedsAttention)?;
        }
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
            owner: self.inputs.owner,
            bindings: &bindings,
            storage: self.inputs.storage,
            writer: self.inputs.writer,
            config_dir: &self.inputs.options.config_dir,
        }
        .resolve(&records, &resume)?;
        // This trusted binding comes from the current host directory/permit,
        // never from a saved recipe. Install it before the run loop freezes its
        // tools, or a restored conversation silently loses todo_write.
        for record in records.iter().filter(|record| record.parent.is_none()) {
            if let Some(config) = authority.members.get(&record.run_id) {
                let permit = config
                    .ownership
                    .as_ref()
                    .ok_or(StartupBlocked::OwnerBlocked)?;
                let _generation = permit
                    .mutation_guard()
                    .map_err(|_| StartupBlocked::OwnerBlocked)?;
                self.inputs
                    .runtime
                    .bind_thread_root(&permit.thread_id, record.run_id)
                    .map_err(|_| StartupBlocked::NeedsAttention)?;
            }
        }
        let models = self.catalog.pin(self.inputs.runtime, &authority);
        let existing = self.claim(id)?;
        let claim = if let Some(claim) = existing {
            match claim.state {
                RecoveryClaimState::Prepared => claim,
                RecoveryClaimState::Activated => return Err(StartupBlocked::AlreadyActivated),
                RecoveryClaimState::Released { .. } => return Err(StartupBlocked::Released),
            }
        } else {
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
        Ok(PreparedStartupComponent {
            paused,
            authority,
            models,
        })
    }
}
/// Opaque, non-serializable process-local staged activation capability. It is
/// never produced by deserializing a scan/report or CLI continuity assertion.
pub struct PreparedStartupComponent {
    paused: PausedRecoveryComponent,
    authority: CurrentRecoveryAuthority,
    models: RecoveryModels,
}
impl PreparedStartupComponent {
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
    pub fn activate(self, runtime: &AgentRuntime) -> Result<Vec<RunId>, StartupBlocked> {
        runtime
            .activate_component_with_models(self.paused, self.authority, self.models)
            .map_err(|_| StartupBlocked::NeedsAttention)
    }
}

#[cfg(test)]
#[path = "recovery_startup_tests.rs"]
mod tests;
