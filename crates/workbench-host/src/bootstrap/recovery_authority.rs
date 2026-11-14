//! Current binding/owner/configuration, not a serialized recipe, grants authority.
use super::current::{CurrentTeamProblem, CurrentTeamSource};
use crate::lane::{BindingTable, memory_boundary};
use runtime::{
    AgentRuntime, CurrentRecoveryAuthority, RunConfig, RunId,
    ownership::{OwnerHost, OwnershipError, RegistryError},
    quiesce::{CORE_RECOVERY_SETTINGS_REFERENCE, ResumePolicy, RunRecoveryRecord},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CurrentAuthorityProblem {
    OwnerBlocked,
    CurrentBindingRequired,
    CurrentSettingsRequired,
    CliContinuityRequired,
    TeamReconciliationRequired,
    StorageUnavailable,
}

/// Supplied only by the current host, after catalog publication and the lane
/// exclusion fence. Bindings must be populated from the current project/thread
/// directory before resolving a component; recipes never populate this table.
pub struct CurrentAuthorityProvider<'a> {
    pub runtime: &'a AgentRuntime,
    pub owner: &'a Arc<OwnerHost>,
    pub bindings: &'a BindingTable,
    pub storage: &'a storage::StorageConfig,
    pub writer: &'a storage::StorageHandle,
    pub config_dir: &'a std::path::Path,
}

impl CurrentAuthorityProvider<'_> {
    pub fn resolve(
        &self,
        records: &[RunRecoveryRecord],
        resume: &BTreeSet<RunId>,
    ) -> Result<CurrentRecoveryAuthority, CurrentAuthorityProblem> {
        self.resolve_current(records, resume, true)
    }

    /// Release grants no execution capability. Current binding/team/owner checks
    /// still apply, but a rejected continuation selector is itself a release reason.
    pub fn resolve_for_release(
        &self,
        records: &[RunRecoveryRecord],
        resume: &BTreeSet<RunId>,
    ) -> Result<CurrentRecoveryAuthority, CurrentAuthorityProblem> {
        self.resolve_current(records, resume, false)
    }

    fn resolve_current(
        &self,
        records: &[RunRecoveryRecord],
        resume: &BTreeSet<RunId>,
        execution: bool,
    ) -> Result<CurrentRecoveryAuthority, CurrentAuthorityProblem> {
        let mut roots = BTreeMap::new();
        for record in records
            .iter()
            .filter(|record| resume.contains(&record.run_id))
        {
            if execution && record.recipe.settings_reference != CORE_RECOVERY_SETTINGS_REFERENCE {
                return Err(CurrentAuthorityProblem::CurrentSettingsRequired);
            }
            if execution && !matches!(record.recipe.policy, ResumePolicy::CoreOwned) {
                return Err(CurrentAuthorityProblem::CliContinuityRequired);
            }
            let thread = record
                .thread
                .as_deref()
                .ok_or(CurrentAuthorityProblem::CurrentBindingRequired)?;
            let binding = self
                .bindings
                .get(thread)
                .ok_or(CurrentAuthorityProblem::CurrentBindingRequired)?;
            if binding.root != record.root || binding.project_root != record.project {
                return Err(CurrentAuthorityProblem::CurrentBindingRequired);
            }
            let root_record = records
                .iter()
                .find(|candidate| candidate.run_id == binding.root && candidate.parent.is_none())
                .ok_or(CurrentAuthorityProblem::CurrentBindingRequired)?;
            if roots.contains_key(&binding.root) {
                continue;
            }
            let project = binding
                .project_root
                .as_ref()
                .ok_or(CurrentAuthorityProblem::CurrentBindingRequired)?;
            let config = config::Config::load_strict(&config::LoadOptions {
                user_config_dir: Some(self.config_dir.to_path_buf()),
                project_dir: Some(project.clone()),
                read_env: false,
                ..Default::default()
            })
            .map_err(|_| CurrentAuthorityProblem::CurrentSettingsRequired)?;
            let mut authority = RunConfig {
                project_root: Some(project.clone()),
                model_preference: binding.model_preference.clone(),
                interactive: true,
                keep_alive: true,
                conversation: true,
                memory: memory_boundary(self.runtime, Some(self.storage), Some(project)),
                ..Default::default()
            };
            // Scope comes from the current lane ledger, not recipe.lane.
            let db = storage::Database::open(self.storage)
                .map_err(|_| CurrentAuthorityProblem::StorageUnavailable)?;
            if let Some(lane) = db
                .lane_by_thread(thread)
                .map_err(|_| CurrentAuthorityProblem::StorageUnavailable)?
            {
                if lane.root_run_id.as_deref() != Some(binding.root.to_string().as_str()) {
                    return Err(CurrentAuthorityProblem::CurrentBindingRequired);
                }
                authority.lane_scope = Some(event_bus::LaneScope::Lane);
                authority.budget = runtime::budget_tracker::BudgetSettings::lane_scope();
            }
            if authority.lane_scope != root_record.recipe.lane {
                return Err(CurrentAuthorityProblem::CurrentBindingRequired);
            }
            if let Some(saved) = &root_record.recipe.team {
                CurrentTeamSource {
                    runtime: self.runtime,
                    binding: &binding,
                    team_config: &config.team,
                    storage: self.storage,
                    writer: self.writer,
                }
                .renew(thread, saved, &mut authority)
                .map_err(|problem| match problem {
                    CurrentTeamProblem::StorageUnavailable => {
                        CurrentAuthorityProblem::StorageUnavailable
                    }
                    _ => CurrentAuthorityProblem::TeamReconciliationRequired,
                })?;
            }
            authority.ownership = Some(
                match self.owner.attach(thread) {
                    Ok(owner) if owner.lease.owner_id == self.owner.owner_id() => {
                        self.owner.owned_permit(thread)
                    }
                    Ok(owner) => self.owner.claim(&owner),
                    Err(RegistryError::Absent) => self.owner.start(thread),
                    Err(error) => Err(error),
                }
                .map_err(|error| match error {
                    RegistryError::Ownership(
                        OwnershipError::NotClaimable
                        | OwnershipError::OwnerResponsive
                        | OwnershipError::Fenced
                        | OwnershipError::Active
                        | OwnershipError::Quiescing,
                    )
                    | RegistryError::Exists => CurrentAuthorityProblem::OwnerBlocked,
                    RegistryError::ThreadDeleted(_) => {
                        CurrentAuthorityProblem::CurrentBindingRequired
                    }
                    _ => CurrentAuthorityProblem::StorageUnavailable,
                })?,
            );
            roots.insert(binding.root, authority);
        }
        let members = records
            .iter()
            .filter(|record| resume.contains(&record.run_id))
            .map(|record| {
                let mut current = roots
                    .get(&record.root)
                    .cloned()
                    .ok_or(CurrentAuthorityProblem::CurrentBindingRequired)?;
                if record.run_id != record.root {
                    current.interactive = false;
                    current.keep_alive = false;
                    current.conversation = false;
                }
                Ok((record.run_id, current))
            })
            .collect::<Result<BTreeMap<_, _>, CurrentAuthorityProblem>>()?;
        if members.keys().copied().collect::<BTreeSet<_>>() != *resume {
            return Err(CurrentAuthorityProblem::CurrentBindingRequired);
        }
        Ok(CurrentRecoveryAuthority { members })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lane::ThreadBinding;
    use runtime::quiesce::RecoveryRecipe;
    fn record(run: RunId, root: &std::path::Path) -> RunRecoveryRecord {
        RunRecoveryRecord {
            version: 1,
            run_id: run,
            old_incarnation: "old".into(),
            intent_revision: 0,
            parent: None,
            root: run,
            thread: Some("thread".into()),
            project: Some(root.to_path_buf()),
            role: "orchestrator".into(),
            recipe: RecoveryRecipe {
                settings_reference: CORE_RECOVERY_SETTINGS_REFERENCE.into(),
                policy: ResumePolicy::CoreOwned,
                goal_id: None,
                team: None,
                lane: None,
                descriptor: None,
            },
            waits: vec![],
            deliveries: vec![],
            next_message_id: 1,
            completion: None,
            completion_generation: None,
            auto_wakes: 0,
            user_reset_generation: 0,
            input_ids: vec![],
        }
    }
    #[tokio::test]
    async fn recipes_cannot_create_a_binding_or_a_current_owner() {
        let dir = test_socket_dir::tempdir();
        let config = storage::StorageConfig {
            db_path: dir.path().join("current.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let writer = storage.handle();
        let bus = Arc::new(event_bus::EventBus::new(32));
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(tools::ToolExecutor::new(bus.clone())),
            Arc::new(runtime::compose::UnconfiguredModel),
        );
        let owner =
            Arc::new(OwnerHost::open(&dir.path().join("owner"), Default::default(), bus).unwrap());
        let bindings = BindingTable::default();
        let provider = CurrentAuthorityProvider {
            runtime: &runtime,
            owner: &owner,
            bindings: &bindings,
            storage: &config,
            writer: &writer,
            config_dir: dir.path(),
        };
        let run = RunId::new(1);
        let mut record = record(run, dir.path());
        let resume = BTreeSet::from([run]);
        assert_eq!(
            provider.resolve(&[record.clone()], &resume).err().unwrap(),
            CurrentAuthorityProblem::CurrentBindingRequired
        );
        assert!(matches!(owner.attach("thread"), Err(RegistryError::Absent)));
        bindings.replace(BTreeMap::from([(
            "thread".into(),
            ThreadBinding {
                root: run,
                project_root: record.project.clone(),
                goal_project: None,
                model_preference: None,
            },
        )]));
        record.recipe.settings_reference = "unknown".into();
        assert_eq!(
            provider.resolve(&[record.clone()], &resume).err().unwrap(),
            CurrentAuthorityProblem::CurrentSettingsRequired
        );
        assert!(matches!(owner.attach("thread"), Err(RegistryError::Absent)));
        record.recipe.settings_reference = CORE_RECOVERY_SETTINGS_REFERENCE.into();
        record.recipe.policy = ResumePolicy::CliInvocation {
            invocation_id: "invocation".into(),
            planned_handoff: Some("handoff".into()),
        };
        assert_eq!(
            provider.resolve(&[record.clone()], &resume).err().unwrap(),
            CurrentAuthorityProblem::CliContinuityRequired
        );
        assert!(matches!(owner.attach("thread"), Err(RegistryError::Absent)));
        record.recipe.policy = ResumePolicy::CoreOwned;
        let authority = provider.resolve(&[record], &resume).unwrap();
        assert_eq!(
            authority.members[&run].project_root.as_deref(),
            Some(dir.path())
        );
        assert_eq!(
            authority.members[&run]
                .ownership
                .as_ref()
                .unwrap()
                .thread_id,
            "thread"
        );
        assert!(authority.members[&run].interactive && authority.members[&run].keep_alive);
    }
}
