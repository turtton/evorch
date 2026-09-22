// allow: SIZE_OK - T5 confines dock lifecycle changes to this existing action module; broader action extraction is separate work.
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use workspace_ui::{
    Panel, PanelId, PanelKind, ProjectError, ProjectId, ThreadError, ThreadId, TrustState,
};

use super::{ConversationFocus, WorkbenchError, WorkbenchState};
use crate::diff::{DiffMode, DiffRequest};
use crate::model::commands::{LoopEvent, MergeDecision, WorkbenchCommand};
use crate::model::tasks::AgentRunSource;

impl<S: AgentRunSource> WorkbenchState<S> {
    pub(super) fn sync_shell_cwd(&mut self) -> bool {
        let cwd = self
            .sidebar
            .resolved_primary_project()
            .map(|project| project.repo_root.clone());
        match self.sink.set_default_cwd(cwd) {
            Ok(()) => true,
            Err(error) => {
                self.push_notice(format!("Failed to configure project shell: {error}"));
                false
            }
        }
    }

    pub fn set_primary_project(
        &mut self,
        project_id: Option<ProjectId>,
    ) -> Result<(), WorkbenchError> {
        self.sidebar.set_primary_project(project_id)?;
        self.save_sidebar();
        Ok(())
    }

    pub fn select_project(&mut self, project_id: ProjectId) -> Result<(), WorkbenchError> {
        self.sidebar.select_project(&project_id)?;
        Ok(())
    }

    pub fn add_project(&mut self, path: impl AsRef<Path>) -> Result<ProjectId, WorkbenchError> {
        let path =
            crate::model::project_path::expand_tilde(path.as_ref(), self.home_dir.as_deref())?;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("project");
        let id = ProjectId::new(name);
        self.sidebar.add_project(id.clone(), name, &path)?;
        if self.sidebar.selected_project.is_none() {
            self.sidebar.select_project(&id)?;
        }
        Ok(id)
    }

    pub fn add_allowed_directory(&mut self, path: impl AsRef<Path>) -> Result<(), WorkbenchError> {
        let project_id = self
            .sidebar
            .selected_project
            .clone()
            .ok_or(ProjectError::UnknownProject)?;
        self.sidebar
            .add_allowed_directory(&project_id, path.as_ref(), TrustState::Approved)?;
        Ok(())
    }

    pub fn set_allowed_trust(
        &mut self,
        path: impl AsRef<Path>,
        trust: TrustState,
    ) -> Result<(), WorkbenchError> {
        let project_id = self
            .sidebar
            .selected_project
            .clone()
            .ok_or(ProjectError::UnknownProject)?;
        self.sidebar
            .set_allowed_trust(&project_id, path.as_ref(), trust)?;
        Ok(())
    }

    pub fn create_thread(&mut self, title: impl Into<String>) -> Result<ThreadId, WorkbenchError> {
        let project_id = self
            .sidebar
            .selected_project
            .clone()
            .ok_or(ProjectError::UnknownProject)?;
        let id = ThreadId::new(format!("thread-{}", self.sidebar.threads.len() + 1));
        self.sidebar.create_thread(id.clone(), project_id, title)?;
        self.switch_thread(id.clone())?;
        Ok(id)
    }

    pub fn fork_thread(&mut self, source: ThreadId) -> Result<ThreadId, WorkbenchError> {
        let original = self
            .sidebar
            .threads
            .iter()
            .find(|thread| thread.id == source)
            .cloned()
            .ok_or(ThreadError::UnknownThread)?;
        let id = ThreadId::new(format!(
            "{}-fork-{}",
            source,
            self.sidebar.threads.len() + 1
        ));
        let mut fork = workspace_ui::ThreadRecord {
            pinned: original.pinned,
            paused: original.paused,
            model_preference: original.model_preference,
            ..workspace_ui::ThreadRecord::new(id.clone(), original.project_id, original.title)
        };
        fork.id = id.clone();
        fork.title = format!("{} (fork)", fork.title);
        fork.parent_thread_id = Some(source);
        fork.fork_event_id = None;
        fork.run_ids.clear();
        fork.branch = None;
        fork.worktree_path = None;
        self.sidebar.threads.push(fork);
        self.switch_thread(id.clone())?;
        self.save_sidebar();
        Ok(id)
    }

    pub fn switch_thread(&mut self, thread_id: ThreadId) -> Result<(), WorkbenchError> {
        self.sidebar.switch_thread(&thread_id)?;
        self.transcripts.select_thread(Some(thread_id.to_string()));
        self.focus = ConversationFocus::Thread;
        self.save_sidebar();
        Ok(())
    }

    pub fn toggle_pin(&mut self, thread_id: ThreadId) -> Result<(), WorkbenchError> {
        let pinned = self
            .sidebar
            .threads
            .iter()
            .find(|thread| thread.id == thread_id)
            .ok_or(ThreadError::UnknownThread)?
            .pinned;
        self.sidebar.set_pinned(&thread_id, !pinned)?;
        Ok(())
    }

    pub fn toggle_pause(&mut self, thread_id: ThreadId) -> Result<(), WorkbenchError> {
        let paused = self
            .sidebar
            .threads
            .iter()
            .find(|thread| thread.id == thread_id)
            .ok_or(ThreadError::UnknownThread)?
            .paused;
        self.sidebar.set_paused(&thread_id, !paused)?;
        Ok(())
    }

    pub fn drill_down(&mut self, run_id: &str) {
        if self
            .dock
            .find_tab(&PanelId::new(format!("agent-{run_id}")))
            .is_some()
        {
            self.focus_panel(&format!("agent-{run_id}"));
            return;
        }
        self.focus = ConversationFocus::Agent(run_id.to_owned());
    }
    pub fn return_to_thread(&mut self) {
        self.focus = ConversationFocus::Thread;
    }

    pub fn open_agent_pane(&mut self, run_id: &str) {
        let panel_id = PanelId::new(format!("agent-{run_id}"));
        if self.panels.contains_key(&panel_id) {
            self.focus_panel(panel_id.as_str());
            return;
        }
        let title = self
            .tasks
            .rows()
            .iter()
            .find(|row| row.run_id.to_string() == run_id)
            .map_or_else(
                || run_id.to_owned(),
                |row| format!("{} ({run_id})", row.name),
            );
        self.panels.insert(
            panel_id.clone(),
            Panel {
                id: panel_id.clone(),
                kind: PanelKind::AgentTranscript,
                title,
                target: Some(run_id.to_owned()),
            },
        );
        let target = self
            .dock
            .find_tab(&PanelId::new("agent-main"))
            .or_else(|| self.dock.find_tab(&PanelId::new("agents-main")))
            .or_else(|| self.dock.find_tab(&PanelId::new("notifications-main")))
            .or_else(|| self.dock.iter_all_tabs().next().map(|(path, _)| path));
        if let Some(path) = target
            && let Ok(leaf) = self.dock.leaf_mut(path.node_path())
        {
            let index = leaf
                .tabs
                .iter()
                .position(|id| id.as_str() == "agent-main")
                .map_or(leaf.tabs.len(), |index| index + 1);
            if leaf.active.0 >= index {
                leaf.active.0 += 1;
            }
            leaf.tabs.insert(index, panel_id);
            let _ = leaf.set_active_tab(index);
        }
    }

    pub fn open_default_agent_panes(&mut self) {
        let roles = [
            ("orchestrator", false),
            ("worker", true),
            ("reviewer", true),
        ];
        let mut selected = BTreeSet::new();
        for (role, latest) in roles {
            let rows = self
                .tasks
                .rows()
                .iter()
                .filter(|row| row.role.eq_ignore_ascii_case(role));
            let row = if latest {
                rows.max_by_key(|row| row.run_id.get())
            } else {
                rows.min_by_key(|row| row.run_id.get())
            };
            if let Some(row) = row {
                selected.insert(row.run_id.to_string());
            }
        }
        for run_id in selected {
            self.open_agent_pane(&run_id);
        }
    }

    pub(super) fn is_conversation_run(&self, run_id: &str) -> bool {
        self.transcripts
            .route(&event_bus::Event::new(
                event_bus::MessageEvent::MessageDelta {
                    delta: String::new(),
                    run_id: Some(run_id.to_owned()),
                },
            ))
            .contains(&crate::model::transcript_registry::TranscriptKey::Thread)
    }

    pub fn request_diff(&mut self, mode: DiffMode) {
        let Some(repo_root) = self.active_repo_root() else {
            return;
        };
        self.diff.request(
            Arc::clone(&self.diff_source),
            DiffRequest { repo_root, mode },
        );
    }

    pub fn submit_goal(&mut self) {
        let (Some(project_id), Some(thread_id)) = (
            self.sidebar.selected_project.as_ref(),
            self.sidebar.active_thread.as_ref(),
        ) else {
            return;
        };
        let command = self
            .goal_form
            .build_command(&project_id.to_string(), &thread_id.to_string());
        self.submit_command(command);
    }

    pub fn decide_merge(&mut self, decision: MergeDecision) {
        let Some(thread_id) = self.sidebar.active_thread.as_ref() else {
            return;
        };
        let Some(mut command) = self.merge.decide(decision) else {
            return;
        };
        if let WorkbenchCommand::DecideMerge(merge) = &mut command {
            merge.thread_id = thread_id.to_string();
        }
        self.submit_command(command);
    }

    pub(super) fn decide_tool_approval(&mut self, call_id: String, approved: bool) {
        self.submit_command(WorkbenchCommand::DecideToolApproval { call_id, approved });
    }

    pub fn save_sidebar(&self) {
        let Some(path) = self.sidebar_path.as_ref() else {
            return;
        };
        if let Err(error) = self.write_history_sidebar(path) {
            tracing::warn!(path = %path.display(), %error, "failed to save sidebar");
        }
    }

    pub fn pause_goal(&mut self) {
        self.submit_goal_control(|goal_id| WorkbenchCommand::PauseGoal { goal_id });
    }

    pub fn resume_goal(&mut self) {
        self.submit_goal_control(|goal_id| WorkbenchCommand::ResumeGoal { goal_id });
    }

    pub fn cancel_goal(&mut self) {
        self.submit_goal_control(|goal_id| WorkbenchCommand::CancelGoal { goal_id });
    }

    fn submit_goal_control(&mut self, build: impl FnOnce(String) -> WorkbenchCommand) {
        let Some(goal_id) = self.loop_status.goal_id.clone() else {
            return;
        };
        self.submit_command(build(goal_id));
    }

    pub fn apply_loop_event(&mut self, event: LoopEvent) {
        match event {
            LoopEvent::UserAnswerSaved { question_id } => {
                self.user_questions.remove(&question_id);
                self.question_drafts.remove(&question_id);
            }
            LoopEvent::SnapshotRestored { thread_id, diff } => {
                if self
                    .sidebar
                    .active_thread
                    .as_ref()
                    .is_some_and(|id| id.to_string() == thread_id)
                {
                    match diff {
                        Some(diff) => {
                            self.diff.show_snapshot(diff);
                            self.focus_panel("diff-main");
                        }
                        None => self.push_notice("No snapshot to restore"),
                    }
                }
            }
            LoopEvent::ChatAccepted { thread_id, run_id } => {
                if self.bind_thread_run(&thread_id, &run_id) {
                    self.save_sidebar();
                }
            }
            LoopEvent::ChatRejected { reason, .. } => {
                tracing::warn!(%reason, "chat command rejected");
                self.push_notice(format!("chat failed: {reason}"));
            }
            LoopEvent::GoalAccepted { goal_id, .. } => {
                self.push_notice(format!("accepted: {goal_id}"));
                self.goal_form.last_accepted = Some(goal_id);
            }
            LoopEvent::MergeStateUpdated(view) => {
                if let Some(pr) = &view.pr
                    && view.resolution.is_none()
                {
                    self.push_notice(format!(
                        "Merge approval requested for PR #{} (approval UI is moving to the Diff view)",
                        pr.number
                    ));
                }
                self.merge.view = *view;
            }
            LoopEvent::MergeResolved { decision, .. } => {
                self.merge.view.resolution = Some(decision)
            }
            LoopEvent::LoopStatusUpdated(status) => self.loop_status = status,
            LoopEvent::CommandRejected { reason } => {
                tracing::warn!(%reason, "goal loop command rejected");
                self.push_notice(reason);
            }
        }
    }

    pub(super) fn submit_command(&mut self, command: WorkbenchCommand) {
        if !self.thread_writable() {
            self.push_notice("Read-only attach: explicitly Start or Claim before mutating.");
            return;
        }
        if !self.sync_shell_cwd() {
            return;
        }
        self.issued.push(command.clone());
        for event in self.sink.submit(command) {
            self.apply_loop_event(event);
        }
    }

    pub(super) fn active_repo_root(&self) -> Option<PathBuf> {
        let project_id = self.sidebar.selected_project.as_ref()?;
        self.sidebar
            .projects
            .iter()
            .find(|project| &project.id == project_id)
            .map(|project| project.repo_root.clone())
    }

    pub(super) fn focus_panel(&mut self, id: &str) {
        if let Some(tab_path) = self.dock.find_tab(&PanelId::new(id)) {
            let _ = self.dock.set_active_tab(tab_path);
        }
    }
}
