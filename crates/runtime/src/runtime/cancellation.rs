use super::*;

#[derive(Default)]
pub(super) struct SpawnIntent {
    pub(super) parent: Option<RunId>,
    pub(super) cancelled: bool,
}

impl AgentRuntime {
    pub(crate) fn track_goal_run(&self, run: RunId, owner: &str) {
        let Some(parent) = owner
            .strip_prefix("run-")
            .and_then(|id| id.parse().ok())
            .map(RunId::new)
        else {
            return;
        };
        let mut intents = self
            .shared
            .spawn_intents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cancelled = intents.get(&parent).is_some_and(|intent| intent.cancelled);
        intents.insert(
            run,
            SpawnIntent {
                parent: Some(parent),
                cancelled,
            },
        );
    }

    pub(crate) fn spawn_cancelled(&self, run: RunId) -> bool {
        self.shared
            .spawn_intents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&run)
            .is_some_and(|intent| intent.cancelled)
    }

    pub(crate) fn workspace_configuration_failed(&self, run: RunId) -> bool {
        self.shared.workspace.is_none()
            && lock_runs(&self.shared.runs)
                .get(&run)
                .is_some_and(|entry| entry.config.workspace_mode == WorkspaceMode::Isolated)
    }

    pub(crate) fn cancel_goal_runs(&self, snapshot: &crate::orchestration::ledger::GoalSnapshot) {
        let mut intents = self
            .shared
            .spawn_intents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut runs: std::collections::HashSet<RunId> = snapshot
            .attached_runs
            .iter()
            .filter_map(|run| {
                run.run_id
                    .strip_prefix("run-")?
                    .parse()
                    .ok()
                    .map(RunId::new)
            })
            .collect();
        loop {
            let descendants: Vec<_> = intents
                .iter()
                .filter_map(|(run, intent)| {
                    intent
                        .parent
                        .filter(|parent| runs.contains(parent))
                        .map(|_| *run)
                })
                .collect();
            let before = runs.len();
            runs.extend(descendants);
            if runs.len() == before {
                break;
            }
        }
        for run in runs {
            intents.entry(run).or_default().cancelled = true;
            let _ = self.cancel(run);
        }
    }
}
