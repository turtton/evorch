//! Hands the skills a run reads to its escalation reviewer.

use super::LoopState;
use crate::escalation_review::{LoadedBy, LoadedSkill, LoadedSkillRef};

impl LoopState {
    /// Records `text` as read from the run's registry. A skill from an allowed
    /// directory is never recorded: its author is not an approved source.
    pub(crate) fn record_review_skill(&self, name: &str, loaded_by: LoadedBy, text: String) {
        if self.allowed_skill_origins.contains_key(name) {
            return;
        }
        let Some(scope) = self
            .skills()
            .and_then(|registry| registry.get(name))
            .map(|entry| entry.scope)
        else {
            return;
        };
        let Some(runtime) = self.runtime() else {
            return;
        };
        let mut runs = runtime
            .shared
            .review_runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(run) = runs.get_mut(&self.caller_run_id()) {
            run.record_loaded_skill(LoadedSkill {
                name: name.to_owned(),
                scope,
                loaded_by,
                text,
            });
        }
    }

    /// Reads `refs` again, skipping any the registry no longer resolves.
    ///
    /// A restored run starts from `refs` alone: what the run's live record held
    /// before is dropped, so a skill that cannot be read now, or whose text
    /// changed, never reaches the reviewer from memory. A new run keeps the
    /// snapshot it inherited from its parent.
    pub(crate) fn record_review_skills_again(&self, refs: Vec<LoadedSkillRef>, restored: bool) {
        if restored
            && let Some(runtime) = self.runtime()
            && let Some(run) = runtime
                .shared
                .review_runs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get_mut(&self.caller_run_id())
        {
            run.loaded_skills.clear();
        }
        let Some(registry) = self.skills() else {
            return;
        };
        for skill in refs {
            if let Ok(text) = registry.load_body(&skill.name) {
                self.record_review_skill(&skill.name, skill.loaded_by, text);
            }
        }
    }

    /// The skills this run has handed its reviewer, for a restore snapshot.
    pub(crate) fn review_skill_refs(&self) -> Vec<LoadedSkillRef> {
        let Some(runtime) = self.runtime() else {
            return Vec::new();
        };
        runtime
            .shared
            .review_runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&self.caller_run_id())
            .map(|run| run.loaded_skill_refs())
            .unwrap_or_default()
    }
}

impl crate::AgentRuntime {
    /// The skills `run_id` has handed its reviewer, for a seed that starts a
    /// successor from it. Empty when the run is not live.
    pub(crate) fn review_skill_refs_of(&self, run_id: crate::RunId) -> Vec<LoadedSkillRef> {
        self.shared
            .review_runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&run_id)
            .map(|run| run.loaded_skill_refs())
            .unwrap_or_default()
    }
}
