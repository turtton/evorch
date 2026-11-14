//! Current team capability construction. Saved identities only constrain a match.
use crate::lane::{TeamSource, ThreadBinding};
use runtime::{AgentRuntime, RunConfig, restore::TeamRestoreIdentity};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CurrentTeamProblem {
    BindingMismatch,
    TeamDisabled,
    StorageUnavailable,
    TeamReconciliationRequired,
}

/// Open the CURRENT board using the current binding and host storage. Never
/// derive a store path, coordinator, writer, or worker limit from a saved recipe.
/// All Claimed tasks (including expired ones) require explicit reconciliation;
/// opening this context never expires, steals or releases their leases.
pub struct CurrentTeamSource<'a> {
    pub runtime: &'a AgentRuntime,
    pub binding: &'a ThreadBinding,
    pub team_config: &'a config::types::TeamConfig,
    pub storage: &'a storage::StorageConfig,
    pub writer: &'a storage::StorageHandle,
}
impl CurrentTeamSource<'_> {
    pub fn renew(
        &self,
        thread: &str,
        saved: &TeamRestoreIdentity,
        authority: &mut RunConfig,
    ) -> Result<(), CurrentTeamProblem> {
        let Self {
            runtime,
            binding,
            team_config,
            storage,
            writer,
        } = *self;
        if !team_config.enabled {
            return Err(CurrentTeamProblem::TeamDisabled);
        }
        let project = binding
            .goal_project
            .as_deref()
            .ok_or(CurrentTeamProblem::BindingMismatch)?;
        let expected_id = format!("{project}:{thread}");
        if binding.root != saved.coordinator_run_id
            || saved.team_id != expected_id
            || authority.project_root != binding.project_root
            || authority.team_task.is_some()
        {
            return Err(CurrentTeamProblem::BindingMismatch);
        }
        // Require an existing current board. A missing board is not evidence of an
        // empty team, and opening a new one would hide erased or mismatched state.
        let before = storage::Database::open(storage)
            .and_then(|db| db.team_snapshot(&expected_id))
            .map_err(|_| CurrentTeamProblem::StorageUnavailable)?
            .ok_or(CurrentTeamProblem::TeamReconciliationRequired)?;
        let mut current = authority.clone();
        TeamSource {
            runtime,
            memory_config: Some(storage),
            team_writer: Some(writer),
            goal_project: Some(project),
        }
        .renew(thread, binding.root, None, &mut current)
        .map_err(|_| CurrentTeamProblem::TeamReconciliationRequired)?;
        let store = runtime::team_context::TeamStore {
            config: storage.clone(),
            writer: writer.clone(),
            id: expected_id,
        };
        let team = runtime::team_context::TeamContext::persistent(
            binding.root,
            &store,
            team_config.max_workers,
        )
        .map_err(|_| CurrentTeamProblem::TeamReconciliationRequired)?;
        let after = storage::Database::open(storage)
            .and_then(|db| db.team_snapshot(&store.id))
            .map_err(|_| CurrentTeamProblem::StorageUnavailable)?;
        if after.as_ref() != Some(&before)
            || team
                .board
                .snapshot()
                .map_err(|_| CurrentTeamProblem::TeamReconciliationRequired)?
                .iter()
                .any(|task| matches!(task.state, runtime::team::ClaimState::Claimed(_)))
        {
            return Err(CurrentTeamProblem::TeamReconciliationRequired);
        }
        current.topology = runtime::CoordinationTopology::DynamicTeam {
            max_workers: team_config.max_workers,
        };
        current.finding_store = Some(storage.db_path.clone());
        current.team_store = Some(store);
        current.team = Some(team);
        *authority = current;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use runtime::RunId;
    use std::{path::PathBuf, sync::Arc};
    #[test]
    fn current_board_is_explicit_and_expired_claims_are_not_inferred_ready() {
        let dir = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("team.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let bus = Arc::new(event_bus::EventBus::new(32));
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(tools::ToolExecutor::new(bus)),
            Arc::new(runtime::compose::UnconfiguredModel),
        );
        let root = RunId::new(1);
        let binding = ThreadBinding {
            root,
            project_root: Some(PathBuf::from("/current")),
            goal_project: Some("project".into()),
            model_preference: None,
        };
        let saved = TeamRestoreIdentity {
            team_id: "project:thread".into(),
            coordinator_run_id: root,
        };
        let store = runtime::team_context::TeamStore {
            config: config.clone(),
            writer: storage.handle(),
            id: saved.team_id.clone(),
        };
        let team = runtime::team_context::TeamContext::persistent(root, &store, 2).unwrap();
        team.board
            .enqueue(runtime::team::TaskSpec {
                id: "task".into(),
                paths: vec![PathBuf::from("artifact")],
            })
            .unwrap();
        let current = config::types::TeamConfig {
            enabled: true,
            max_workers: 2,
        };
        let mut authority = RunConfig {
            project_root: binding.project_root.clone(),
            ..Default::default()
        };
        let writer = storage.handle();
        let source = CurrentTeamSource {
            runtime: &runtime,
            binding: &binding,
            team_config: &current,
            storage: &config,
            writer: &writer,
        };
        source.renew("thread", &saved, &mut authority).unwrap();
        assert_eq!(authority.team.as_ref().unwrap().id, saved.team_id);
        assert_eq!(authority.team.as_ref().unwrap().coordinator, root);
        assert_eq!(
            authority.team.as_ref().unwrap().board.snapshot().unwrap(),
            team.board.snapshot().unwrap()
        );
        team.board.claim("task", "old-worker", 0).unwrap();
        let before = storage::Database::open(&config)
            .unwrap()
            .team_snapshot(&saved.team_id)
            .unwrap();
        let mut rejected = RunConfig {
            project_root: binding.project_root.clone(),
            ..Default::default()
        };
        assert_eq!(
            source.renew("thread", &saved, &mut rejected),
            Err(CurrentTeamProblem::TeamReconciliationRequired)
        );
        assert!(rejected.team.is_none());
        assert_eq!(
            storage::Database::open(&config)
                .unwrap()
                .team_snapshot(&saved.team_id)
                .unwrap(),
            before
        );
        let other = TeamRestoreIdentity {
            team_id: "other:thread".into(),
            coordinator_run_id: root,
        };
        assert_eq!(
            source.renew("thread", &other, &mut rejected),
            Err(CurrentTeamProblem::BindingMismatch)
        );
    }
}
