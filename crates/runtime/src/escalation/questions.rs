//! Handoff-specific question authority; ordinary forwarding stays independent.
use crate::{AgentRuntime, RunId};
use event_bus::UserQuestion;
impl AgentRuntime {
    pub(crate) fn handoff_answer_recipients(
        &self,
        question: &UserQuestion,
        mut recipients: Vec<String>,
    ) -> Result<Vec<String>, String> {
        let store = self
            .shared
            .run_store
            .get()
            .ok_or("question storage is not configured")?;
        let owner = store
            .handle
            .handoff_question_owner(question.id.clone())
            .map_err(|e| e.to_string())?;
        if let Some(owner) = owner.filter(|o| o.committed) {
            let receipt = store
                .handle
                .handoff_for_target(owner.target_run_id.clone())
                .map_err(|e| e.to_string())?;
            let valid = receipt.as_ref().is_some_and(|r| {
                r.state.is_committed()
                    && r.source_run_id == owner.source_run_id
                    && r.source_incarnation == owner.source_incarnation
            }) && recipients.contains(&owner.target_run_id)
                && self.deny_deleted_run(&owner.target_run_id).is_ok();
            if !valid {
                return Err(
                    "committed handoff question has no surviving authorized recipient".into(),
                );
            }
            let lane = store
                .handle
                .lane(&receipt.expect("validated receipt").lane_id)
                .map_err(|e| e.to_string())?
                .ok_or("handoff lane is missing")?;
            let seed: serde_json::Value =
                serde_json::from_str(&lane.start_seed).map_err(|e| e.to_string())?;
            let root = lane
                .root_run_id
                .as_deref()
                .or_else(|| seed.get("run_id").and_then(serde_json::Value::as_str));
            if root != Some(owner.target_run_id.as_str())
                || self.deny_deleted_thread(&lane.thread_id).is_err()
            {
                return Err("handoff lane is not live".into());
            }
            // Requester IDs remain immutable provenance, never evidence destinations
            // or authority for a later Worker incarnation.
            recipients.retain(|r| {
                r != &question.run_id
                    && r != &owner.source_run_id
                    && self.deny_deleted_run(r).is_ok()
            });
        } else {
            self.deny_deleted_run(&question.run_id)
                .map_err(|e| e.to_string())?;
            recipients.push(question.run_id.clone());
        }
        recipients.sort();
        recipients.dedup();
        Ok(recipients)
    }
    pub(crate) fn filter_handoff_questions(
        &self,
        run: RunId,
        questions: Vec<UserQuestion>,
    ) -> Result<Vec<UserQuestion>, String> {
        let Some(store) = self.shared.run_store.get() else {
            return Ok(questions);
        };
        let run = run.to_string();
        let mut visible = Vec::new();
        for q in questions {
            let transferred = store
                .handle
                .handoff_question_owner(q.id.clone())
                .map_err(|e| e.to_string())?
                .is_some_and(|o| {
                    o.committed
                        && o.target_run_id != run
                        && (o.source_run_id == run || q.run_id == run)
                });
            if !transferred {
                visible.push(q);
            }
        }
        Ok(visible)
    }
}
