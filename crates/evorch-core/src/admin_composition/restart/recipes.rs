//! Current read-only recipe evidence. Installation belongs to V1b-2b.
//! No owner is started/claimed and no binding is restored from a saved recipe.
#![cfg(target_os = "linux")]
use crate::restart::public::{ObservedRecipes, RecipeSource, Recipes};
use runtime::{
    AgentRuntime, RestartRecipeMember, RunId,
    ownership::{OwnerHost, OwnerPermit},
    quiesce::{CORE_RECOVERY_SETTINGS_REFERENCE, RecoveryRecipe, ResumePolicy},
    restore::RunRestoreDescriptor,
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use workbench_host::{
    admin::{AdminFailure, AdminIo, AdminReader},
    bootstrap::BootstrappedHost,
    ingress::CommandHost,
    lane::BindingObservation,
};
use workbench_protocol::errors::{ErrorCode, RetryAdvice, WorkbenchError};

/// Reads only this host's current conversation bindings and capture inventory.
pub struct HostRecipeSource {
    runtime: AgentRuntime,
    host: Arc<Mutex<CommandHost>>,
    owner: Arc<OwnerHost>,
    storage: storage::StorageConfig,
    reader: Arc<AdminReader>,
}
struct Bindings {
    observed: BindingObservation,
    chats: BTreeMap<String, RunId>,
    goals: BTreeMap<String, RunId>,
    projects: BTreeMap<String, String>,
    permits: BTreeMap<String, OwnerPermit>,
}
impl HostRecipeSource {
    pub fn new(host: &BootstrappedHost) -> Self {
        Self {
            runtime: host.runtime.clone(),
            host: host.host.clone(),
            owner: host.owner.clone(),
            storage: host.storage_config().clone(),
            reader: host.admin.reader().clone(),
        }
    }
    fn bindings(&self) -> Result<Bindings, AdminFailure> {
        let host = self.host.lock().map_err(|_| unavailable())?;
        let service = host.service();
        let observed = service.lane_bindings().observe();
        // Only memory copies under CommandHost. permit_for_thread would open
        // the owner database, so must never be called under this lock.
        Ok(Bindings {
            observed,
            chats: service.bindings.chat_runs.clone(),
            goals: service.bindings.goal_runs.clone(),
            projects: service.bindings.goal_projects.clone(),
            permits: service.bindings.chat_permits.clone(),
        })
    }
    async fn collect_current(&self) -> Result<ObservedRecipes, AdminFailure> {
        let bindings = self.bindings()?;
        let held: BTreeMap<_, _> = bindings
            .chats
            .iter()
            .filter_map(|(thread, run)| bindings.permits.get(thread).map(|p| (*run, p.clone())))
            .collect();
        let members =
            self.runtime
                .restart_recipe_inventory(&held)
                .map_err(|error| match error {
                    runtime::RuntimeError::RestartObservationChanged => conflict(),
                    _ => unsupported("unknown member"),
                })?;
        for member in &members {
            if member.lane {
                return Err(unsupported("lane"));
            }
            if member.team {
                return Err(unsupported("team"));
            }
            if member.goal {
                return Err(unsupported("goal"));
            }
            if member.internal {
                return Err(unsupported("internal run"));
            }
            let thread = member
                .thread
                .as_ref()
                .ok_or_else(|| unsupported("unbound root"))?;
            if bindings.goals.contains_key(thread)
                || bindings.projects.contains_key(thread)
                || bindings
                    .goals
                    .values()
                    .any(|run| *run == member.root || *run == member.run)
            {
                return Err(unsupported("goal"));
            }
            if bindings.chats.get(thread) != Some(&member.root)
                || bindings
                    .chats
                    .values()
                    .filter(|run| **run == member.root)
                    .count()
                    != 1
                || !member.permit_matches
            {
                return Err(unsupported("unconfirmed core ownership"));
            }
            if member.parent.is_none() && !member.conversation {
                return Err(unsupported("non-conversation root"));
            }
        }
        let storage = self.storage.clone();
        let owner = self.owner.clone();
        let checked = members.clone();
        let recipes = self
            .reader
            .read(move || read_recipes(&storage, &owner, &bindings, &checked))
            .await;
        let recipes = match recipes {
            Ok(recipes) => recipes,
            // A run that finished a turn between the inventory and the read has a
            // newer snapshot than the one the inventory carries. That is a change
            // of the observation (observe again), not an unsupported run: only an
            // inventory that is still the same keeps the refusal.
            Err(failure) => {
                return Err(
                    if self.runtime.restart_recipe_inventory(&held).ok().as_ref() != Some(&members)
                    {
                        conflict()
                    } else {
                        failure
                    },
                );
            }
        };
        // Evidence, not an admission reservation. Public preflight independently
        // binds this inventory to its opaque runtime observation.
        if self
            .runtime
            .restart_recipe_inventory(&held)
            .map_err(|_| conflict())?
            != members
            || !recipes.bindings.is_current()
        {
            return Err(conflict());
        }
        Ok(recipes)
    }
}
impl RecipeSource for HostRecipeSource {
    fn collect(&self) -> AdminIo<'_, ObservedRecipes> {
        Box::pin(self.collect_current())
    }
}
fn same_permit(left: &OwnerPermit, right: &OwnerPermit) -> bool {
    left.registry_path == right.registry_path
        && left.thread_id == right.thread_id
        && left.lease.owner_id == right.lease.owner_id
        && left.lease.generation == right.lease.generation
}
fn read_recipes(
    storage: &storage::StorageConfig,
    owner: &OwnerHost,
    bindings: &Bindings,
    members: &[RestartRecipeMember],
) -> Result<ObservedRecipes, AdminFailure> {
    // Do not create an empty DB and mistake a missing source for no runs.
    if !storage.db_path.is_file() {
        return Err(unavailable());
    }
    let db = storage::Database::open(storage).map_err(|_| unavailable())?;
    let mut recipes = Recipes::new();
    for member in members {
        let thread = member
            .thread
            .as_deref()
            .ok_or_else(|| unsupported("unbound root"))?;
        let held = bindings
            .permits
            .get(thread)
            .ok_or_else(|| unsupported("missing permit"))?;
        let current = owner
            .owned_permit(thread)
            .map_err(|_| unsupported("foreign or fenced owner"))?;
        if !same_permit(held, &current) {
            return Err(unsupported("foreign or fenced owner"));
        }
        if db
            .lane_by_thread(thread)
            .map_err(|_| unavailable())?
            .is_some()
        {
            return Err(unsupported("lane"));
        }
        let record = db
            .run_context(&member.run.to_string())
            .map_err(|_| unavailable())?
            .ok_or_else(|| unsupported("missing snapshot"))?;
        let descriptor: RunRestoreDescriptor = serde_json::from_str(&record.config_json)
            .map_err(|_| unsupported("corrupt descriptor"))?;
        if descriptor.lane_scope.is_some() {
            return Err(unsupported("lane"));
        }
        if descriptor.renewable_team.is_some() {
            return Err(unsupported("team"));
        }
        if descriptor.durable_task_id.is_some() {
            return Err(unsupported("task-associated run"));
        }
        if descriptor.parent_run_id != member.parent
            || descriptor.thread_id.as_deref() != Some(thread)
            || descriptor.role != member.role
            || record.role != member.role
            || record.parent_run_id != member.parent.map(|run| run.to_string())
        {
            return Err(unsupported("descriptor mismatch"));
        }
        let context_fingerprint = storage::recovery::restart::restart_digest(&(
            &descriptor.project_root,
            &descriptor.model_preference,
            descriptor.interactive,
            descriptor.keep_alive,
        ))
        .map_err(|_| unsupported("descriptor mismatch"))?;
        if context_fingerprint != member.context_fingerprint {
            return Err(unsupported("descriptor mismatch"));
        }
        let references: Vec<_> = member
            .snapshots
            .iter()
            .filter(|reference| {
                matches!(
                    reference,
                    runtime::checkpoint::SnapshotReference::RunContext { .. }
                )
            })
            .collect();
        if references.is_empty()
            || references
                .iter()
                .any(|reference| !reference.available(Some(&record)))
        {
            return Err(unsupported("missing or changed snapshot"));
        }
        recipes.insert(
            member.run,
            RecoveryRecipe {
                settings_reference: CORE_RECOVERY_SETTINGS_REFERENCE.into(),
                policy: ResumePolicy::CoreOwned,
                goal_id: None,
                team: None,
                lane: None,
                descriptor: Some(descriptor),
            },
        );
    }
    if !bindings.observed.is_current() {
        return Err(conflict());
    }
    Ok(ObservedRecipes {
        recipes,
        bindings: bindings.observed.clone(),
    })
}
fn unsupported(kind: &str) -> AdminFailure {
    WorkbenchError::new(
        ErrorCode::UnsupportedCapability,
        format!("restart unsupported: {kind}; finish the affected run normally before retrying"),
        RetryAdvice::Never,
    )
}
fn unavailable() -> AdminFailure {
    WorkbenchError::new(
        ErrorCode::Busy,
        "restart recipe source unavailable",
        RetryAdvice::Never,
    )
}
fn conflict() -> AdminFailure {
    WorkbenchError::new(
        ErrorCode::Conflict,
        "restart recipe inventory changed; inspect again",
        RetryAdvice::Never,
    )
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
