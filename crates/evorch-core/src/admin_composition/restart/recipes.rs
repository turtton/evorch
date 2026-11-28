//! Current read-only recipe evidence. Installation belongs to V1b-2b.
//! No binding or execution authority is restored from a saved recipe.
#![cfg(target_os = "linux")]
use crate::restart::public::{
    ObservedRecipes, RecipeLedgers, RecipeSource, RecipeThreadLedger, RecipeThreadState, Recipes,
};
use runtime::{
    AgentRuntime, RestartRecipeMember, RunId,
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
    storage: storage::StorageConfig,
    reader: Arc<AdminReader>,
}
struct Bindings {
    observed: BindingObservation,
    chats: BTreeMap<String, RunId>,
    goals: BTreeMap<String, RunId>,
    projects: BTreeMap<String, String>,
    ledgers: RecipeLedgers,
}
impl HostRecipeSource {
    pub fn new(host: &BootstrappedHost) -> Self {
        Self {
            runtime: host.runtime.clone(),
            host: host.host.clone(),
            storage: host.storage_config().clone(),
            reader: host.admin.reader().clone(),
        }
    }
    fn bindings(&self) -> Result<Bindings, AdminFailure> {
        let host = self.host.lock().map_err(|_| unavailable())?;
        let service = host.service();
        let observed = service.lane_bindings().observe();
        // Only memory copies under CommandHost; all storage I/O is outside.
        Ok(Bindings {
            observed,
            chats: service.bindings.chat_runs.clone(),
            goals: service.bindings.goal_runs.clone(),
            projects: service.bindings.goal_projects.clone(),
            ledgers: BTreeMap::new(),
        })
    }
    fn check_inventory(&self, expected: &[RestartRecipeMember]) -> Result<(), AdminFailure> {
        let current = self
            .runtime
            .restart_recipe_inventory()
            .map_err(|error| match error {
                runtime::RuntimeError::RestartRunUnavailable { run_id, .. } => conflict_for(run_id),
                _ => conflict(),
            })?;
        inventory_unchanged(expected, &current)
    }
    async fn collect_current(&self) -> Result<ObservedRecipes, AdminFailure> {
        let bindings = self.bindings()?;
        let initial = self
            .runtime
            .restart_recipe_inventory()
            .map_err(inventory_error)?;
        // Lanes need not have a GUI/chat binding. Validate the canonical
        // root/generation ledger outside CommandHost.
        let storage = self.storage.clone();
        let checked = initial.clone();
        let bindings = self
            .reader
            .read(move || {
                let mut bindings = bindings;
                let db = open_database(&storage)?;
                for member in checked.iter().filter(|m| m.parent.is_none()) {
                    let thread = member
                        .thread
                        .as_deref()
                        .ok_or_else(|| unsupported(member.run, "unbound root"))?;
                    let lane = db
                        .lane_by_thread(thread)
                        .map_err(|_| unavailable_for(member.run))?;
                    check_lane(member, lane.as_ref())?;
                    bindings
                        .ledgers
                        .insert(thread.to_owned(), read_ledger(&db, thread, lane.as_ref())?);
                }
                Ok(bindings)
            })
            .await;
        let bindings = match bindings {
            Ok(bindings) => bindings,
            Err(failure) => {
                self.check_inventory(&initial)?;
                return Err(failure);
            }
        };
        let members = self
            .runtime
            .restart_recipe_inventory()
            .map_err(inventory_error)?;
        inventory_unchanged(&initial, &members)?;
        if !bindings.observed.is_current() {
            return Err(binding_changed());
        }
        for member in &members {
            if member.team {
                return Err(unsupported(member.run, "team"));
            }
            if member.goal {
                return Err(unsupported(member.run, "goal"));
            }
            if member.internal {
                return Err(unsupported(member.run, "internal run"));
            }
            let thread = member
                .thread
                .as_ref()
                .ok_or_else(|| unsupported(member.run, "unbound root"))?;
            if bindings.goals.contains_key(thread)
                || (!member.lane && bindings.projects.contains_key(thread))
                || bindings
                    .goals
                    .values()
                    .any(|run| *run == member.root || *run == member.run)
            {
                return Err(unsupported(member.run, "goal"));
            }
            if !member.lane
                && (bindings.chats.get(thread) != Some(&member.root)
                    || bindings
                        .chats
                        .values()
                        .filter(|run| **run == member.root)
                        .count()
                        != 1)
            {
                return Err(unsupported(member.run, "unconfirmed current root binding"));
            }
            if member.parent.is_none() && !member.conversation {
                return Err(unsupported(member.run, "non-conversation root"));
            }
        }
        let storage = self.storage.clone();
        let checked = members.clone();
        let recipes = self
            .reader
            .read(move || read_recipes(&storage, &bindings, &checked))
            .await;
        let recipes = match recipes {
            Ok(recipes) => recipes,
            // A run that finished a turn between the inventory and the read has a
            // newer snapshot than the one the inventory carries. That is a change
            // of the observation (observe again), not an unsupported run: only an
            // inventory that is still the same keeps the refusal.
            Err(failure) => {
                self.check_inventory(&members)?;
                return Err(failure);
            }
        };
        // Evidence, not an admission reservation. Public preflight independently
        // binds this inventory to its opaque runtime observation.
        self.check_inventory(&members)?;
        if !recipes.bindings.is_current() {
            return Err(binding_changed());
        }
        Ok(recipes)
    }
}
impl RecipeSource for HostRecipeSource {
    fn collect(&self) -> AdminIo<'_, ObservedRecipes> {
        Box::pin(self.collect_current())
    }

    fn verify_ledgers(&self, expected: RecipeLedgers) -> AdminIo<'_, ()> {
        let storage = self.storage.clone();
        Box::pin(async move {
            self.reader
                .read(move || {
                    let db = open_database(&storage)?;
                    for (thread, expected) in expected {
                        let lane = db.lane_by_thread(&thread).map_err(|_| unavailable())?;
                        check_ledger(
                            &thread,
                            expected
                                .thread
                                .as_ref()
                                .and_then(|row| row.root.as_ref())
                                .and_then(|run| run.parse().ok()),
                            &expected,
                            &read_ledger(&db, &thread, lane.as_ref())?,
                        )?;
                    }
                    Ok(())
                })
                .await
        })
    }
}
fn inventory_unchanged(
    expected: &[RestartRecipeMember],
    current: &[RestartRecipeMember],
) -> Result<(), AdminFailure> {
    let common = expected.len().min(current.len());
    if let Some(member) = expected.get(common).or_else(|| current.get(common)) {
        return Err(member_changed(member.run));
    }
    for (before, after) in expected.iter().zip(current) {
        if before != after {
            let mut prior = before.clone();
            prior.snapshots.clone_from(&after.snapshots);
            prior
                .context_fingerprint
                .clone_from(&after.context_fingerprint);
            return Err(if prior == *after {
                conflict_for(before.run)
            } else {
                member_changed(before.run)
            });
        }
    }
    Ok(())
}
fn inventory_error(error: runtime::RuntimeError) -> AdminFailure {
    match error {
        runtime::RuntimeError::RestartObservationChanged => conflict(),
        runtime::RuntimeError::RestartRunUnavailable { run_id, reason } => {
            unsupported(run_id, &reason)
        }
        _ => unavailable(),
    }
}
fn open_database(storage: &storage::StorageConfig) -> Result<storage::Database, AdminFailure> {
    if !storage.db_path.is_file() {
        return Err(unavailable());
    }
    storage::Database::open(storage).map_err(|_| unavailable())
}
fn check_lane(
    member: &RestartRecipeMember,
    lane: Option<&storage::lanes::LaneRecord>,
) -> Result<(), AdminFailure> {
    if member.lane != lane.is_some() {
        return Err(unsupported(member.run, "lane scope mismatch"));
    }
    if let Some(lane) = lane
        && (lane.thread_id != member.thread.as_deref().unwrap_or_default()
            || lane.root_run_id.as_deref() != Some(member.root.to_string().as_str())
            || (member.parent.is_none()
                && (member.role != runtime::Role::Orchestrator.name() || !member.conversation)))
    {
        return Err(unsupported(member.run, "lane ledger mismatch"));
    }
    Ok(())
}
fn unavailable_for(run: RunId) -> AdminFailure {
    unsupported(run, "source unavailable")
}
fn read_recipes(
    storage: &storage::StorageConfig,
    bindings: &Bindings,
    members: &[RestartRecipeMember],
) -> Result<ObservedRecipes, AdminFailure> {
    // Do not create an empty DB and mistake a missing source for no runs.
    let db = open_database(storage)?;
    let mut recipes = Recipes::new();
    for member in members {
        let thread = member
            .thread
            .as_deref()
            .ok_or_else(|| unsupported(member.run, "unbound root"))?;
        let lane = db
            .lane_by_thread(thread)
            .map_err(|_| unavailable_for(member.run))?;
        let expected = bindings.ledgers.get(thread).ok_or_else(binding_changed)?;
        check_ledger(
            thread,
            Some(member.run),
            expected,
            &read_ledger(&db, thread, lane.as_ref())?,
        )?;
        check_lane(member, lane.as_ref())?;
        let record = db
            .run_context(&member.run.to_string())
            .map_err(|_| unavailable_for(member.run))?
            .ok_or_else(|| unsupported(member.run, "missing snapshot"))?;
        let descriptor: RunRestoreDescriptor = serde_json::from_str(&record.config_json)
            .map_err(|_| unsupported(member.run, "corrupt descriptor"))?;
        if descriptor.lane_scope.is_some() != member.lane {
            return Err(unsupported(member.run, "lane"));
        }
        if descriptor.renewable_team.is_some() {
            return Err(unsupported(member.run, "team"));
        }
        if descriptor.durable_task_id.is_some() {
            return Err(unsupported(member.run, "task-associated run"));
        }
        if descriptor.parent_run_id != member.parent
            || descriptor.thread_id.as_deref() != Some(thread)
            || descriptor.role != member.role
            || record.role != member.role
            || record.parent_run_id != member.parent.map(|run| run.to_string())
        {
            return Err(unsupported(member.run, "descriptor mismatch"));
        }
        let context_fingerprint = storage::recovery::restart::restart_digest(&(
            &descriptor.project_root,
            &descriptor.model_preference,
            descriptor.interactive,
            descriptor.keep_alive,
        ))
        .map_err(|_| unsupported(member.run, "descriptor mismatch"))?;
        if context_fingerprint != member.context_fingerprint {
            return Err(unsupported(member.run, "descriptor mismatch"));
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
            return Err(unsupported(member.run, "missing or changed snapshot"));
        }
        recipes.insert(
            member.run,
            RecoveryRecipe {
                settings_reference: CORE_RECOVERY_SETTINGS_REFERENCE.into(),
                policy: ResumePolicy::CoreOwned,
                goal_id: None,
                team: None,
                lane: descriptor.lane_scope,
                descriptor: Some(descriptor),
            },
        );
    }
    if !bindings.observed.is_current() {
        return Err(binding_changed());
    }
    Ok(ObservedRecipes {
        recipes,
        bindings: bindings.observed.clone(),
        ledgers: bindings.ledgers.clone(),
    })
}
fn read_ledger(
    db: &storage::Database,
    thread: &str,
    lane: Option<&storage::lanes::LaneRecord>,
) -> Result<RecipeThreadLedger, AdminFailure> {
    let thread = db
        .thread(thread)
        .map_err(|_| unavailable())?
        .map(|row| RecipeThreadState {
            root: row.root_run_id,
            project_id: row.project_id,
            project_root: row.project_root,
            archived: row.archived,
            archive_revision: row.archive_revision,
            superseded: row.superseded,
        });
    let lane = lane.map(|row| {
        (
            row.lane_id.clone(),
            row.root_run_id.clone().unwrap_or_default(),
            row.generation,
            row.management_revision,
        )
    });
    Ok(RecipeThreadLedger { thread, lane })
}
fn check_ledger(
    thread: &str,
    run: Option<RunId>,
    expected: &RecipeThreadLedger,
    actual: &RecipeThreadLedger,
) -> Result<(), AdminFailure> {
    let reason = if expected.lane != actual.lane {
        "lane identity, management revision or generation changed"
    } else if expected.thread != actual.thread {
        "thread root, project or archive ledger changed"
    } else {
        return Ok(());
    };
    let thread: String = thread
        .chars()
        .filter(|c| !c.is_control())
        .take(64)
        .collect();
    let subject = run.map(|run| format!("{thread}/{run}")).unwrap_or(thread);
    Err(WorkbenchError::new(
        ErrorCode::Conflict,
        format!("restart refused: {reason}: {subject}"),
        RetryAdvice::Never,
    ))
}
fn binding_changed() -> AdminFailure {
    WorkbenchError::new(
        ErrorCode::Conflict,
        "restart chat, goal or project binding changed",
        RetryAdvice::Never,
    )
}
fn member_changed(run: RunId) -> AdminFailure {
    WorkbenchError::new(
        ErrorCode::Conflict,
        format!("restart authority or membership changed: {run}"),
        RetryAdvice::Never,
    )
}
fn conflict_for(run: RunId) -> AdminFailure {
    WorkbenchError::new(
        ErrorCode::Conflict,
        format!("restart recipe inventory changed: {run}; inspect again"),
        RetryAdvice::AfterResync,
    )
}
fn unsupported(run: RunId, kind: &str) -> AdminFailure {
    WorkbenchError::new(
        ErrorCode::UnsupportedCapability,
        format!(
            "restart unsupported: {run}: {kind}; finish the affected run normally before retrying"
        ),
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
        RetryAdvice::AfterResync,
    )
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
