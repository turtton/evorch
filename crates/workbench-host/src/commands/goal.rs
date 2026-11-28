//! Generic thread goals.

use runtime::{AgentRuntime, RunConfig};

use super::{
    ChatInput, CommandService, ComposerRole, GoalInput, HostEvent, ReferenceKind, RejectKind,
};

impl CommandService {
    pub(super) fn submit_goal(&mut self, submission: GoalInput) -> Vec<HostEvent> {
        let thread_id = submission.thread_id.clone();
        let objective = submission.goal.clone();
        let criteria = vec![objective.clone()];
        let prompt = render_entry_prompt(&submission);
        let mut initial = RunConfig::default();
        if let Some(value) = submission.delegation_value {
            if value.trim().is_empty() {
                return vec![HostEvent::CommandRejected {
                    kind: RejectKind::Invalid,
                    reason: "team mode requires explicit delegation value".into(),
                }];
            }
            let (Some(config), Some(writer)) = (&self.memory_config, &self.team_writer) else {
                return vec![HostEvent::CommandRejected {
                    kind: RejectKind::Unsupported,
                    reason: "team storage is unavailable".into(),
                }];
            };
            initial.team_store = Some(runtime::team_context::TeamStore {
                config: config.clone(),
                writer: writer.clone(),
                id: format!("{}:{}", submission.project_id, thread_id),
            });
            initial.topology = runtime::CoordinationTopology::DynamicTeam { max_workers: 3 };
            initial.delegation_value = Some(value);
            initial.finding_store = Some(config.db_path.clone());
        }
        initial.initial_thread_goal = Some((objective, criteria));
        let previous_project = self
            .bindings
            .goal_projects
            .insert(thread_id.clone(), submission.project_id.clone());
        let mut events = self.submit_chat_config(
            ChatInput {
                submission_id: String::new(),
                project_root: submission.project_root.clone(),
                thread_id: thread_id.clone(),
                text: prompt,
                composer_role: ComposerRole::Orchestrator,
                attachments: Vec::new(),
                model_preference: runtime::ModelPreferenceUpdate::Keep,
                fork_seed: None,
                skills: Vec::new(),
            },
            false,
            initial,
        );
        let accepted = events
            .iter()
            .any(|event| matches!(event, HostEvent::ChatAccepted { .. }));
        if !accepted {
            match previous_project {
                Some(project) => {
                    self.bindings.goal_projects.insert(thread_id, project);
                }
                None => {
                    self.bindings.goal_projects.remove(&thread_id);
                }
            }
            return events;
        }
        if let Some(goal) = self.runtime.thread_goal(&thread_id) {
            events.push(HostEvent::GoalAccepted {
                thread_id,
                goal_id: goal.goal_id,
            });
        }
        events
    }

    pub(super) fn goal_control(
        &self,
        control: impl FnOnce(&AgentRuntime) -> Result<(), String>,
    ) -> Vec<HostEvent> {
        let _runtime = self.handle.enter();
        match control(&self.runtime) {
            Ok(()) => Vec::new(),
            Err(reason) => vec![HostEvent::CommandRejected {
                kind: RejectKind::Failed,
                reason,
            }],
        }
    }
}

/// GoalInput を background run へ渡す entry prompt として整形する。
///
/// goal 本文を先頭に置き、references / constraints は空でない場合のみ
/// `References:` / `Constraints:` セクションとして 1 行 1 項目で続ける。
/// 参照と制約は通常会話へ渡す文脈であり、PR や固定フローの起動条件ではない。
pub fn render_entry_prompt(submission: &GoalInput) -> String {
    let mut prompt = submission.goal.clone();
    if !submission.references.is_empty() {
        prompt.push_str("\n\nReferences:\n");
        let lines: Vec<String> = submission
            .references
            .iter()
            .map(|reference| {
                let kind_label = reference_kind_label(&reference.kind);
                format!("- {kind_label}: {}", reference.value)
            })
            .collect();
        prompt.push_str(&lines.join("\n"));
    }
    if !submission.constraints.is_empty() {
        prompt.push_str("\n\nConstraints:\n");
        let lines: Vec<String> = submission
            .constraints
            .iter()
            .map(|constraint| format!("- {constraint}"))
            .collect();
        prompt.push_str(&lines.join("\n"));
    }
    prompt
}

/// 参照元種別のラベル。
fn reference_kind_label(kind: &ReferenceKind) -> &'static str {
    match kind {
        ReferenceKind::Packet => "packet",
        ReferenceKind::Issue => "issue",
    }
}
