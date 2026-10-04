//! Thread objectives use the existing working context and never imply delivery authority.
mod boundary;
#[cfg(test)]
mod tests;
pub(crate) mod tools;

use crate::{AgentRunPhase, AgentRuntime, RunId};
use event_bus::{
    Event, OrchestratorEvent, ThreadGoalCheck, ThreadGoalPhase, ThreadGoalSnapshot, ThreadGoalUsage,
};
use serde::Deserialize;
use std::collections::HashMap;

const MAX_TEXT: usize = 8192;
const MAX_ITEMS: usize = 32;
pub(crate) const CHECKS_WAKE: &str = "[runtime thread-goal checks wake]";
pub(crate) const RECOVERY_HINT: &str =
    " Use /goal <objective> to explicitly start a new goal with a new budget.";

#[derive(Default)]
pub(crate) struct ThreadGoals {
    roots: HashMap<RunId, String>,
    requests: HashMap<RunId, String>,
    goals: HashMap<String, GoalEntry>,
}

pub(crate) struct GoalEntry {
    snapshot: ThreadGoalSnapshot,
    reviewer: Option<RunId>,
    review_result: Option<GoalReview>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GoalReview {
    pub epoch: u64,
    pub checks: Vec<ThreadGoalCheck>,
    pub findings: Vec<String>,
}

impl AgentRuntime {
    fn goal_lock(&self) -> std::sync::MutexGuard<'_, ThreadGoals> {
        self.shared
            .thread_goals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Register trusted host thread ownership before starting the reserved root.
    pub fn bind_thread_root(&self, thread_id: &str, run_id: RunId) -> Result<(), String> {
        if thread_id.is_empty() {
            return Err("thread_id must not be empty".into());
        }
        if self
            .list_agents()
            .iter()
            .any(|run| run.run_id == run_id && run.parent_run_id.is_some())
        {
            return Err("only a root run can own a thread goal".into());
        }
        let mut goals = self.goal_lock();
        if goals
            .roots
            .get(&run_id)
            .is_some_and(|thread| thread != thread_id)
        {
            return Err("run already belongs to another thread".into());
        }
        if let Some(entry) = goals.goals.get_mut(thread_id)
            && entry.snapshot.root_run_id != run_id.to_string()
            && entry.snapshot.phase != ThreadGoalPhase::Complete
        {
            if entry.snapshot.related_root_run_ids.len() >= 128 {
                return Err("goal root handoff limit reached".into());
            }
            let previous = entry.snapshot.root_run_id.clone();
            if !entry.snapshot.related_root_run_ids.contains(&previous) {
                entry.snapshot.related_root_run_ids.push(previous);
            }
        }
        goals.roots.retain(|_, thread| thread != thread_id);
        goals.roots.insert(run_id, thread_id.into());
        if let Some(entry) = goals.goals.get_mut(thread_id) {
            let changed =
                entry.snapshot.root_run_id != run_id.to_string() || entry.snapshot.work_stopped;
            entry.snapshot.root_run_id = run_id.to_string();
            entry.snapshot.work_stopped = false;
            if changed && entry.snapshot.phase != ThreadGoalPhase::Complete {
                invalidate(entry);
                self.publish_thread_goal(&entry.snapshot);
            }
        }
        Ok(())
    }

    pub fn thread_goal(&self, thread_id: &str) -> Option<ThreadGoalSnapshot> {
        self.goal_lock()
            .goals
            .get(thread_id)
            .map(|entry| entry.snapshot.clone())
    }

    /// Host creation seam. Agent calls additionally validate their trusted root identity.
    pub fn create_thread_goal(
        &self,
        thread_id: &str,
        run_id: RunId,
        objective: String,
        criteria: Vec<String>,
    ) -> Result<ThreadGoalSnapshot, String> {
        self.create_goal_inner(thread_id, run_id, objective, criteria, true)
    }

    pub(crate) fn create_agent_thread_goal(
        &self,
        thread_id: &str,
        run_id: RunId,
        objective: String,
        criteria: Vec<String>,
    ) -> Result<ThreadGoalSnapshot, String> {
        self.create_goal_inner(thread_id, run_id, objective, criteria, false)
    }

    fn create_goal_inner(
        &self,
        thread_id: &str,
        run_id: RunId,
        objective: String,
        criteria: Vec<String>,
        replace_blocked: bool,
    ) -> Result<ThreadGoalSnapshot, String> {
        validate_text(&objective)?;
        validate_criteria(&criteria)?;
        // Explicit replacement can reset an exhausted budget, but it cannot
        // make still-running work in this thread disappear from the new gate.
        let related_root_run_ids = self
            .thread_goal(thread_id)
            .into_iter()
            .flat_map(|goal| goal.related_root_run_ids)
            .filter(|related| {
                crate::meta::parse_run_id(related).is_ok_and(|run| {
                    !self.live_descendants(run).is_empty()
                        || self.inspect_agent(run).is_ok_and(|agent| {
                            matches!(
                                agent.phase,
                                AgentRunPhase::Pending
                                    | AgentRunPhase::Running
                                    | AgentRunPhase::Waiting
                            )
                        })
                })
            })
            .collect();
        let mut goals = self.goal_lock();
        if goals.roots.get(&run_id).map(String::as_str) != Some(thread_id) {
            return Err("run is not the registered thread root".into());
        }
        if goals.goals.get(thread_id).is_some_and(|entry| {
            entry.snapshot.phase != ThreadGoalPhase::Complete
                && !(replace_blocked && entry.snapshot.phase == ThreadGoalPhase::Blocked)
        }) {
            return Err(
                "this thread already has an unfinished goal; continue that objective".into(),
            );
        }
        let sequence = goals
            .goals
            .get(thread_id)
            .map_or(0, |entry| entry.snapshot.epoch.saturating_add(1));
        let snapshot = ThreadGoalSnapshot {
            goal_id: format!("thread-goal-{run_id}-{sequence}"),
            thread_id: thread_id.into(),
            root_run_id: run_id.to_string(),
            related_root_run_ids,
            original_request: objective.clone(),
            objective,
            criteria,
            checks: Vec::new(),
            phase: ThreadGoalPhase::Working,
            review_enabled: false,
            checks_paused: false,
            work_stopped: false,
            epoch: sequence,
            review_round: 0,
            findings: Vec::new(),
            reason: None,
            usage: ThreadGoalUsage::default(),
            max_review_rounds: 3,
            max_model_requests: 100,
            max_tokens: None,
        };
        let previous = goals.goals.insert(
            thread_id.into(),
            GoalEntry {
                snapshot: snapshot.clone(),
                reviewer: None,
                review_result: None,
            },
        );
        self.publish_thread_goal(&snapshot);
        drop(goals);
        self.cancel_goal_reviewer(previous.and_then(|entry| entry.reviewer));
        Ok(snapshot)
    }

    pub(crate) fn validate_new_thread_goal(
        &self,
        thread: &str,
        objective: &str,
        criteria: &[String],
    ) -> Result<(), String> {
        validate_text(objective)?;
        validate_criteria(criteria)?;
        if self.goal_lock().goals.get(thread).is_some_and(|entry| {
            !matches!(
                entry.snapshot.phase,
                ThreadGoalPhase::Complete | ThreadGoalPhase::Blocked
            )
        }) {
            return Err(
                "this thread already has an unfinished goal; continue that objective".into(),
            );
        }
        Ok(())
    }

    /// Rehydrate event-sourced state without starting work or granting new authority.
    pub fn restore_thread_goal(&self, mut snapshot: ThreadGoalSnapshot) -> Result<(), String> {
        validate_text(&snapshot.objective)?;
        validate_text(&snapshot.original_request)?;
        validate_criteria(&snapshot.criteria)?;
        validate_checks(&snapshot.criteria, &snapshot.checks, false)?;
        validate_findings(&snapshot.findings)?;
        let root = crate::meta::parse_run_id(&snapshot.root_run_id)?;
        if snapshot.related_root_run_ids.len() > 128 {
            return Err("too many goal root handoffs".into());
        }
        for related in &snapshot.related_root_run_ids {
            crate::meta::parse_run_id(related)?;
        }
        snapshot.epoch = snapshot.epoch.saturating_add(1);
        snapshot.work_stopped = snapshot.phase != ThreadGoalPhase::Complete;
        if matches!(
            snapshot.phase,
            ThreadGoalPhase::Checking | ThreadGoalPhase::Reviewing
        ) {
            snapshot.phase = ThreadGoalPhase::Working;
            snapshot.checks.clear();
        }
        let mut goals = self.goal_lock();
        goals.roots.insert(root, snapshot.thread_id.clone());
        self.publish_thread_goal(&snapshot);
        goals.goals.insert(
            snapshot.thread_id.clone(),
            GoalEntry {
                snapshot,
                reviewer: None,
                review_result: None,
            },
        );
        Ok(())
    }

    pub fn set_goal_review(
        &self,
        thread_id: &str,
        goal_id: &str,
        enabled: bool,
    ) -> Result<(), String> {
        let reviewer = {
            let mut goals = self.goal_lock();
            let entry = matching(&mut goals, thread_id, goal_id)?;
            if entry.snapshot.review_enabled == enabled {
                return Ok(());
            }
            if entry.snapshot.phase == ThreadGoalPhase::Complete {
                return Err("completed goal review settings cannot be changed".into());
            }
            entry.snapshot.review_enabled = enabled;
            let reviewer = entry.reviewer.take();
            if entry.snapshot.phase != ThreadGoalPhase::Complete {
                invalidate(entry);
            }
            self.publish_thread_goal(&entry.snapshot);
            reviewer
        };
        self.cancel_goal_reviewer(reviewer);
        self.wake_goal_checks(thread_id);
        Ok(())
    }

    pub fn set_goal_checks_paused(
        &self,
        thread_id: &str,
        goal_id: &str,
        paused: bool,
    ) -> Result<(), String> {
        let reviewer = {
            let mut goals = self.goal_lock();
            let entry = matching(&mut goals, thread_id, goal_id)?;
            if entry.snapshot.checks_paused == paused {
                return Ok(());
            }
            if entry.snapshot.phase == ThreadGoalPhase::Complete {
                return Err("completed goal checks cannot be paused or resumed".into());
            }
            entry.snapshot.checks_paused = paused;
            let reviewer = entry.reviewer.take();
            if entry.snapshot.phase != ThreadGoalPhase::Complete {
                invalidate(entry);
            }
            self.publish_thread_goal(&entry.snapshot);
            reviewer
        };
        self.cancel_goal_reviewer(reviewer);
        if !paused {
            self.wake_goal_checks(thread_id);
        }
        Ok(())
    }

    fn wake_goal_checks(&self, thread_id: &str) {
        let Some(snapshot) = self.thread_goal(thread_id) else {
            return;
        };
        if snapshot.checks_paused
            || snapshot.work_stopped
            || matches!(
                snapshot.phase,
                ThreadGoalPhase::Complete | ThreadGoalPhase::Blocked
            )
        {
            return;
        }
        let Ok(root) = crate::meta::parse_run_id(&snapshot.root_run_id) else {
            return;
        };
        if self.inspect_agent(root).is_ok_and(|run| {
            matches!(
                run.phase,
                AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting
            )
        }) {
            let _ = self.send_internal_message(root, CHECKS_WAKE.into());
        }
    }

    pub(crate) fn goal_for_root(&self, root: RunId) -> Option<ThreadGoalSnapshot> {
        let goals = self.goal_lock();
        goals
            .roots
            .get(&root)
            .and_then(|thread| goals.goals.get(thread))
            .map(|entry| entry.snapshot.clone())
    }

    pub(crate) fn goal_thread(&self, root: RunId) -> Option<String> {
        self.goal_lock().roots.get(&root).cloned()
    }

    pub(crate) fn transfer_thread_goal_root(
        &self,
        source: RunId,
        target: RunId,
    ) -> Result<(), String> {
        let Some(thread) = self.goal_thread(source) else {
            return Ok(());
        };
        let request = self.goal_lock().requests.get(&source).cloned();
        self.bind_thread_root(&thread, target)?;
        if let Some(request) = request {
            self.goal_lock().requests.insert(target, request);
        }
        Ok(())
    }

    pub(crate) fn goal_owner_for_run(&self, run: RunId) -> Option<RunId> {
        let roots = {
            let goals = self.goal_lock();
            let mut roots = HashMap::new();
            for (root, thread) in &goals.roots {
                roots.insert(*root, *root);
                if let Some(entry) = goals.goals.get(thread) {
                    for related in &entry.snapshot.related_root_run_ids {
                        if let Ok(previous) = crate::meta::parse_run_id(related) {
                            roots.insert(previous, *root);
                        }
                    }
                }
            }
            roots
        };
        let runs = self.list_agents();
        let mut current = run;
        loop {
            if let Some(owner) = roots.get(&current) {
                return Some(*owner);
            }
            current = runs
                .iter()
                .find(|candidate| candidate.run_id == current)?
                .parent_run_id?;
        }
    }

    pub(crate) fn active_goal_children(&self, root: RunId) -> Vec<RunId> {
        let mut descendants: std::collections::HashSet<_> =
            self.live_descendants(root).into_iter().collect();
        if let Some(goal) = self.goal_for_root(root) {
            for related in goal.related_root_run_ids {
                if let Ok(previous) = crate::meta::parse_run_id(&related) {
                    descendants.extend(self.live_descendants(previous));
                    if self.inspect_agent(previous).is_ok_and(|run| {
                        matches!(
                            run.phase,
                            AgentRunPhase::Pending
                                | AgentRunPhase::Running
                                | AgentRunPhase::Waiting
                        )
                    }) {
                        descendants.insert(previous);
                    }
                }
            }
        }
        descendants.remove(&root);
        descendants.into_iter().collect()
    }

    pub(crate) fn goal_user_input(&self, root: RunId, text: &str) {
        let reviewer = {
            let mut goals = self.goal_lock();
            let Some(thread) = goals.roots.get(&root).cloned() else {
                return;
            };
            goals.requests.insert(root, bounded(text, MAX_TEXT));
            let Some(entry) = goals.goals.get_mut(&thread) else {
                return;
            };
            if entry.snapshot.phase == ThreadGoalPhase::Complete {
                return;
            }
            let reviewer = entry.reviewer.take();
            invalidate(entry);
            entry.snapshot.work_stopped = false;
            // Preserve the original request and the newest correction within a bounded record.
            let original = &entry.snapshot.original_request;
            entry.snapshot.original_request = bounded(
                &format!(
                    "{}\nLatest user instruction: {}",
                    bounded(original, MAX_TEXT / 2),
                    bounded(text, MAX_TEXT / 2 - 32)
                ),
                MAX_TEXT,
            );
            self.publish_thread_goal(&entry.snapshot);
            reviewer
        };
        self.cancel_goal_reviewer(reviewer);
    }

    pub(crate) fn goal_work_stopped(&self, root: RunId) {
        let reviewer = {
            let mut goals = self.goal_lock();
            let Some(thread) = goals.roots.get(&root).cloned() else {
                return;
            };
            let Some(entry) = goals.goals.get_mut(&thread) else {
                return;
            };
            if entry.snapshot.phase == ThreadGoalPhase::Complete {
                return;
            }
            let reviewer = entry.reviewer.take();
            invalidate(entry);
            entry.snapshot.work_stopped = true;
            self.publish_thread_goal(&entry.snapshot);
            reviewer
        };
        self.cancel_goal_reviewer(reviewer);
    }

    fn cancel_goal_reviewer(&self, reviewer: Option<RunId>) {
        if let Some(reviewer) = reviewer {
            let _ = self.cancel(reviewer);
        }
    }

    fn publish_thread_goal(&self, snapshot: &ThreadGoalSnapshot) {
        self.shared
            .bus
            .emit(Event::new(OrchestratorEvent::ThreadGoalUpdated {
                snapshot: snapshot.clone(),
            }));
    }

    /// Count both worker and reviewer requests in one durable, non-resetting budget.
    pub(crate) fn goal_model_request(
        &self,
        run: RunId,
        purpose: crate::RunPurpose,
        token_limit: Option<u64>,
    ) -> bool {
        let root = match purpose {
            crate::RunPurpose::ThreadGoalReview { root_run_id, .. } => root_run_id,
            _ => self.goal_owner_for_run(run).unwrap_or(run),
        };
        let mut goals = self.goal_lock();
        let Some(thread) = goals.roots.get(&root).cloned() else {
            return true;
        };
        let Some(entry) = goals.goals.get_mut(&thread) else {
            return true;
        };
        if entry.snapshot.phase == ThreadGoalPhase::Complete {
            return true;
        }
        if run == root
            && purpose == crate::RunPurpose::General
            && let Some(limit) = token_limit
        {
            entry.snapshot.max_tokens = Some(
                entry
                    .snapshot
                    .max_tokens
                    .map_or(limit, |previous| previous.min(limit)),
            );
        }
        let used_tokens = entry
            .snapshot
            .usage
            .input_tokens
            .saturating_add(entry.snapshot.usage.output_tokens);
        if entry
            .snapshot
            .max_tokens
            .is_some_and(|limit| used_tokens >= limit)
        {
            entry.snapshot.phase = ThreadGoalPhase::Blocked;
            entry.snapshot.reason = Some(format!(
                "Goal cumulative token budget exhausted; automatic checking is stopped.{RECOVERY_HINT}"
            ));
            self.publish_thread_goal(&entry.snapshot);
            return false;
        }
        if entry.snapshot.usage.model_requests >= entry.snapshot.max_model_requests {
            entry.snapshot.phase = ThreadGoalPhase::Blocked;
            entry.snapshot.reason = Some(format!(
                "Goal model-request budget exhausted; automatic checking is stopped.{RECOVERY_HINT}"
            ));
            self.publish_thread_goal(&entry.snapshot);
            return false;
        }
        entry.snapshot.usage.model_requests += 1;
        self.publish_thread_goal(&entry.snapshot);
        true
    }

    pub(crate) fn goal_usage(
        &self,
        run: RunId,
        purpose: crate::RunPurpose,
        usage: providers::Usage,
    ) {
        let root = match purpose {
            crate::RunPurpose::ThreadGoalReview { root_run_id, .. } => root_run_id,
            _ => self.goal_owner_for_run(run).unwrap_or(run),
        };
        let mut goals = self.goal_lock();
        let Some(thread) = goals.roots.get(&root).cloned() else {
            return;
        };
        if let Some(entry) = goals.goals.get_mut(&thread) {
            if entry.snapshot.phase == ThreadGoalPhase::Complete {
                return;
            }
            entry.snapshot.usage.input_tokens = entry
                .snapshot
                .usage
                .input_tokens
                .saturating_add(usage.input_tokens);
            entry.snapshot.usage.output_tokens = entry
                .snapshot
                .usage
                .output_tokens
                .saturating_add(usage.output_tokens);
            let total = entry
                .snapshot
                .usage
                .input_tokens
                .saturating_add(entry.snapshot.usage.output_tokens);
            if entry.snapshot.max_tokens.is_some_and(|limit| total > limit) {
                entry.snapshot.phase = ThreadGoalPhase::Blocked;
                entry.snapshot.reason = Some(format!(
                    "Goal cumulative token budget exhausted; automatic checking is stopped.{RECOVERY_HINT}"
                ));
            }
            self.publish_thread_goal(&entry.snapshot);
        }
    }
}

fn matching<'a>(
    goals: &'a mut ThreadGoals,
    thread: &str,
    id: &str,
) -> Result<&'a mut GoalEntry, String> {
    goals
        .goals
        .get_mut(thread)
        .filter(|entry| entry.snapshot.goal_id == id)
        .ok_or_else(|| "goal is no longer current".into())
}
fn invalidate(entry: &mut GoalEntry) {
    entry.snapshot.epoch = entry.snapshot.epoch.saturating_add(1);
    if entry.snapshot.phase != ThreadGoalPhase::Blocked {
        entry.snapshot.phase = ThreadGoalPhase::Working;
    }
    entry.snapshot.checks.clear();
    entry.review_result = None;
}
fn bounded(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}
fn validate_text(text: &str) -> Result<(), String> {
    if text.trim().is_empty() || text.chars().count() > MAX_TEXT {
        Err(format!("text must contain 1..={MAX_TEXT} characters"))
    } else {
        Ok(())
    }
}
fn validate_criteria(criteria: &[String]) -> Result<(), String> {
    if criteria.is_empty() || criteria.len() > MAX_ITEMS {
        return Err("provide 1..=32 acceptance criteria".into());
    }
    for criterion in criteria {
        validate_text(criterion)?;
    }
    Ok(())
}
fn validate_findings(findings: &[String]) -> Result<(), String> {
    if findings.len() > MAX_ITEMS {
        return Err("at most 32 findings are accepted".into());
    }
    for finding in findings {
        validate_text(finding)?;
    }
    Ok(())
}
fn validate_checks(
    criteria: &[String],
    checks: &[ThreadGoalCheck],
    complete: bool,
) -> Result<(), String> {
    if checks.len() > criteria.len() || (complete && checks.len() != criteria.len()) {
        return Err("report exactly one check per acceptance criterion".into());
    }
    let mut seen = std::collections::HashSet::new();
    for check in checks {
        if check.criterion >= criteria.len() || !seen.insert(check.criterion) {
            return Err("invalid or duplicate criterion index".into());
        }
        validate_text(&check.evidence)?;
    }
    Ok(())
}
