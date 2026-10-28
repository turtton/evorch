// allow: SIZE_OK - T5 confines dock lifecycle changes to this existing action module; broader action extraction is separate work.
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use workspace_ui::{
    Panel, PanelId, PanelKind, ProjectError, ProjectId, ThreadError, ThreadId, TrustState,
};

use super::{ConversationFocus, WorkbenchError, WorkbenchState};
use crate::diff::{DiffMode, DiffRequest};
use crate::model::commands::{LoopEvent, WorkbenchCommand};
use crate::model::tasks::AgentRunSource;

impl<S: AgentRunSource> WorkbenchState<S> {
    /// Tells the runtime which projects' own instructions it may load, before any run starts.
    pub(super) fn sync_project_trust(&mut self) {
        for project in &self.sidebar.projects {
            if self.declared_trust.get(&project.repo_root) != Some(&project.trust) {
                self.sink.set_project_trust(
                    project.repo_root.clone(),
                    project.trust == TrustState::Approved,
                );
                self.declared_trust
                    .insert(project.repo_root.clone(), project.trust);
            }
        }
        self.sync_project_allowed_directories();
        self.sync_thread_read_only_directories();
    }

    /// Declares each project's approved allowed directories that still exist.
    /// Missing ones keep their saved record and are left out until they return.
    pub(super) fn sync_project_allowed_directories(&mut self) {
        for project in &self.sidebar.projects {
            let (present, missing): (Vec<_>, Vec<_>) = project
                .allowed_directories
                .iter()
                .filter(|directory| directory.trust == TrustState::Approved)
                .map(|directory| directory.path.clone())
                .partition(|path| path.is_dir());
            if self.declared_allowed.get(&project.repo_root) == Some(&present) {
                continue;
            }
            for path in &missing {
                tracing::warn!(
                    project = %project.repo_root.display(),
                    directory = %path.display(),
                    "allowed directory no longer exists; not declared"
                );
            }
            self.sink
                .set_project_allowed_directories(project.repo_root.clone(), present.clone());
            self.declared_allowed
                .insert(project.repo_root.clone(), present);
        }
    }

    /// Declares each thread's read-only directories that still exist (ADR 0037),
    /// re-sending only changed lists. A declared thread whose list became empty
    /// is sent an empty list once so the runtime drops it; threads that never had
    /// any are not declared at all.
    pub(super) fn sync_thread_read_only_directories(&mut self) {
        let mut current = std::collections::BTreeMap::new();
        for thread in &self.sidebar.threads {
            let (present, missing): (Vec<_>, Vec<_>) =
                thread.extra_read_dirs.iter().cloned().partition(|path| {
                    let root = self
                        .sidebar
                        .projects
                        .iter()
                        .find(|project| project.id == thread.project_id)
                        .map(|project| project.repo_root.as_path());
                    crate::panes::self_improvement::read_directory_error(
                        path,
                        root,
                        self.memory
                            .config
                            .as_ref()
                            .map(|config| config.db_path.as_path()),
                    )
                    .is_none()
                });
            let id = thread.id.to_string();
            if self
                .declared_thread_read_dirs
                .get(&id)
                .map_or(&[][..], Vec::as_slice)
                != present.as_slice()
            {
                for path in &missing {
                    tracing::warn!(
                        thread = %id,
                        directory = %path.display(),
                        "thread read-only directory is missing or protected; not declared"
                    );
                }
            }
            if !present.is_empty() {
                current.insert(id, present);
            }
        }
        let cleared: Vec<String> = self
            .declared_thread_read_dirs
            .keys()
            .filter(|id| !current.contains_key(*id))
            .cloned()
            .collect();
        for id in cleared {
            self.declared_thread_read_dirs.remove(&id);
            self.sink.set_thread_read_only_directories(id, Vec::new());
        }
        for (id, present) in current {
            if self.declared_thread_read_dirs.get(&id) == Some(&present) {
                continue;
            }
            self.sink
                .set_thread_read_only_directories(id.clone(), present.clone());
            self.declared_thread_read_dirs.insert(id, present);
        }
    }

    pub fn set_project_trust(
        &mut self,
        project_id: &ProjectId,
        trust: TrustState,
    ) -> Result<(), WorkbenchError> {
        self.sidebar.set_project_trust(project_id, trust)?;
        self.save_sidebar();
        self.sync_project_trust();
        Ok(())
    }

    pub(super) fn sync_shell_cwd(&mut self) -> bool {
        self.sync_project_trust();
        // The active thread's project hosts the shell and the runtime's active project.
        let cwd = self
            .sidebar
            .active_thread
            .as_ref()
            .and_then(|thread| self.thread_project(thread))
            .map(|project| project.repo_root.clone())
            .or_else(|| self.active_repo_root());
        match self.sink.set_default_cwd(cwd) {
            Ok(()) => true,
            Err(error) => {
                self.push_notice(format!("Failed to configure project shell: {error}"));
                false
            }
        }
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

    /// Operator-added directories are trusted: picking one is the approval.
    pub fn add_allowed_directory(
        &mut self,
        project_id: &ProjectId,
        path: impl AsRef<Path>,
    ) -> Result<(), WorkbenchError> {
        let path =
            crate::model::project_path::expand_tilde(path.as_ref(), self.home_dir.as_deref())?;
        self.sidebar
            .add_allowed_directory(project_id, &path, TrustState::Approved)?;
        self.save_sidebar();
        self.sync_project_allowed_directories();
        Ok(())
    }

    pub fn rename_project(
        &mut self,
        project_id: &ProjectId,
        name: &str,
    ) -> Result<(), WorkbenchError> {
        self.sidebar.rename_project(project_id, name)?;
        self.save_sidebar();
        Ok(())
    }

    pub fn set_allowed_trust(
        &mut self,
        project_id: &ProjectId,
        path: impl AsRef<Path>,
        trust: TrustState,
    ) -> Result<(), WorkbenchError> {
        self.sidebar
            .set_allowed_trust(project_id, path.as_ref(), trust)?;
        self.save_sidebar();
        self.sync_project_allowed_directories();
        Ok(())
    }

    pub fn remove_allowed_directory(
        &mut self,
        project_id: &ProjectId,
        path: impl AsRef<Path>,
    ) -> Result<(), WorkbenchError> {
        self.sidebar
            .remove_allowed_directory(project_id, path.as_ref())?;
        self.save_sidebar();
        self.sync_project_allowed_directories();
        Ok(())
    }

    pub fn create_thread(&mut self, title: impl Into<String>) -> Result<ThreadId, WorkbenchError> {
        let id = self.insert_thread(title)?;
        self.switch_thread(id.clone())?;
        Ok(id)
    }

    /// Opens a thread whose runs may read `dirs`, never write them (ADR 0037),
    /// with `draft_input` waiting in the composer. Nothing is sent. Rejected
    /// directories leave no thread behind.
    pub fn create_thread_with_read_dirs(
        &mut self,
        title: impl Into<String>,
        dirs: Vec<PathBuf>,
        draft_input: String,
    ) -> Result<ThreadId, WorkbenchError> {
        let root = self
            .sidebar
            .selected_project
            .as_ref()
            .and_then(|id| {
                self.sidebar
                    .projects
                    .iter()
                    .find(|project| &project.id == id)
            })
            .map(|project| project.repo_root.as_path());
        for dir in &dirs {
            if let Some(reason) = crate::panes::self_improvement::read_directory_error(
                dir,
                root,
                self.memory
                    .config
                    .as_ref()
                    .map(|config| config.db_path.as_path()),
            ) {
                return Err(ThreadError::InvalidReadDirectory(reason).into());
            }
        }
        let id = self.insert_thread(title)?;
        if let Err(error) = self.sidebar.set_thread_extra_read_dirs(&id, &dirs) {
            self.sidebar.threads.retain(|thread| thread.id != id);
            return Err(error.into());
        }
        self.switch_thread(id.clone())?;
        self.composer.input.clone_from(&draft_input);
        if let Some(thread) = self
            .sidebar
            .threads
            .iter_mut()
            .find(|thread| thread.id == id)
        {
            thread.draft_input = draft_input;
        }
        self.save_sidebar();
        self.sync_thread_read_only_directories();
        Ok(id)
    }

    /// Adds a thread to the selected project without opening it.
    fn insert_thread(&mut self, title: impl Into<String>) -> Result<ThreadId, WorkbenchError> {
        let project_id = self
            .sidebar
            .selected_project
            .clone()
            .ok_or(ProjectError::UnknownProject)?;
        let id = ThreadId::new(format!("thread-{}", self.sidebar.threads.len() + 1));
        self.sidebar.create_thread(id.clone(), project_id, title)?;
        Ok(id)
    }

    /// Fork from the latest completed turn, or an empty child before any turn.
    pub fn fork_thread(&mut self, source: ThreadId) -> Result<ThreadId, WorkbenchError> {
        let point = self
            .thread_transcript(&source)
            .and_then(crate::model::transcript::TranscriptModel::last_fork_point);
        self.branch_thread(&source, point, workspace_ui::LineageKind::Fork)
    }

    pub fn switch_thread(&mut self, thread_id: ThreadId) -> Result<(), WorkbenchError> {
        // A hidden version is reached through its group's visible version.
        let thread_id = if self
            .sidebar
            .threads
            .iter()
            .any(|thread| thread.id == thread_id && thread.superseded)
        {
            workspace_ui::visible_version(&self.sidebar.threads, &thread_id).unwrap_or(thread_id)
        } else {
            thread_id
        };
        let previous = self.sidebar.active_thread.clone();
        self.sidebar.switch_thread(&thread_id)?;
        self.finish_thread_switch(previous);
        Ok(())
    }

    /// All selection paths, including archive -> None, preserve the old editor
    /// before restoring the new thread and committing the selection atomically.
    pub(super) fn finish_thread_switch(&mut self, previous: Option<ThreadId>) {
        if let Some(previous) = previous
            && let Some(thread) = self
                .sidebar
                .threads
                .iter_mut()
                .find(|thread| thread.id == previous)
        {
            if thread.draft_input != self.composer.input {
                // This boundary may see input before the next UI/logic tick.
                // Keep it retryable if the immediate atomic save fails, even
                // when archive leaves no active composer to observe later.
                self.draft_dirty_at.set(Some(std::time::Instant::now()));
            }
            thread.draft_input = std::mem::take(&mut self.composer.input);
            self.composer_attachments
                .insert(previous, std::mem::take(&mut self.composer.attachments));
        }
        self.transcripts
            .select_thread(self.sidebar.active_thread.as_ref().map(ToString::to_string));
        if let Some(thread) = self
            .sidebar
            .threads
            .iter()
            .find(|thread| Some(&thread.id) == self.sidebar.active_thread.as_ref())
        {
            self.composer.restore_thread_role(thread);
            self.composer.input.clone_from(&thread.draft_input);
            self.composer.attachments = self
                .composer_attachments
                .remove(&thread.id)
                .unwrap_or_default();
        } else {
            self.composer.input.clear();
            self.composer.attachments.clear();
        }
        self.composer.completions_dismissed_for = None;
        self.refresh_draft_banner();
        self.focus = ConversationFocus::Thread;
        self.sync_subagent_thread_panes();
        // Previews, skills and the shell follow the thread now in view.
        self.sync_shell_cwd();
        self.save_sidebar();
    }

    pub fn toggle_pin(&mut self, thread_id: ThreadId) -> Result<(), WorkbenchError> {
        // A child hangs under its parent, so a pin could never take effect.
        // Ignore stale child actions the same way archiving does.
        if workspace_ui::display_parent(&self.sidebar.threads, &thread_id).is_some() {
            return Ok(());
        }
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
            if self.dock.find_tab(&panel_id).is_none()
                && let Some(thread_id) = self.thread_for_run(run_id)
            {
                let _ = self.switch_thread(ThreadId::new(thread_id));
            }
            if self.dock.find_tab(&panel_id).is_some() {
                self.focus_panel(panel_id.as_str());
                return;
            }
            // A completed run may have been registered while its owner was
            // unknown. Explicit navigation must still open a visible pane.
            self.panels.remove(&panel_id);
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
                rows.max_by_key(|row| row.run_id)
            } else {
                rows.min_by_key(|row| row.run_id)
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

    fn thread_of_run(&self, run_id: &str) -> Option<ThreadId> {
        self.sidebar
            .threads
            .iter()
            .find(|thread| thread.run_ids.iter().any(|id| id == run_id))
            .map(|thread| thread.id.clone())
    }

    /// The runtime holds subagent reports for this finished root: resume it
    /// under the thread's current authority so it receives them. The user did
    /// not ask for this, so routine notices about its state stay quiet.
    pub(super) fn wake_conversation_root(&mut self, run_id: &str) {
        let Some(thread_id) = self.thread_of_run(run_id) else {
            return;
        };
        if self.readonly_threads.contains(&thread_id.to_string()) {
            return;
        }
        let command = WorkbenchCommand::ContinueChat(self.chat_continuation(&thread_id, true));
        self.issued.push(command.clone());
        for event in self.sink.submit(command) {
            if !matches!(event, LoopEvent::ChatNotice { .. }) {
                self.apply_loop_event(event);
            }
        }
    }

    pub(super) fn announce_auto_continuation_pause(&mut self, run_id: &str, limit: u32) {
        let Some(thread_id) = self.thread_of_run(run_id) else {
            return;
        };
        self.apply_loop_event(LoopEvent::ChatNotice {
            thread_id: thread_id.to_string(),
            text: format!(
                "Paused automatic continuation after {limit} subagent reports without your input. Send a message to deliver the remaining reports and continue."
            ),
        });
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
        let Some(thread_id) = self.sidebar.active_thread.clone() else {
            return;
        };
        let Some(project) = self.thread_project(&thread_id) else {
            return;
        };
        let command = self.goal_form.build_command(
            &project.id.to_string(),
            &project.repo_root,
            &thread_id.to_string(),
        );
        self.submit_command(command);
    }

    pub(super) fn decide_tool_approval(&mut self, call_id: String, approved: bool) {
        self.submit_command(WorkbenchCommand::DecideToolApproval { call_id, approved });
    }

    pub fn save_sidebar(&self) {
        let Some(path) = self.sidebar_path.as_ref() else {
            return;
        };
        if let Err(error) = self.persist_history() {
            tracing::warn!(path = %path.display(), %error, "failed to save sidebar");
        }
    }

    pub fn apply_loop_event(&mut self, event: LoopEvent) {
        match event {
            LoopEvent::UserAnswerSaved { question_id } => {
                self.user_questions.remove(&question_id);
                self.question_drafts.remove(&question_id);
            }
            LoopEvent::ChatAccepted { thread_id, run_id } => {
                // Acceptance indexes the run; lifecycle events identify the current
                // conversation root. Follow-up receipts must not reroute its transcript.
                if self.bind_thread_run(&thread_id, &run_id) {
                    self.save_sidebar();
                }
            }
            LoopEvent::ChatStopped {
                thread_id,
                run_id,
                running_children,
            } => {
                self.phases
                    .insert(run_id, workspace_ui::ThreadRunPhase::Stopped);
                self.running_children
                    .insert(thread_id.clone(), running_children);
            }
            LoopEvent::ChatNotice { thread_id, text } => {
                if self
                    .sidebar
                    .active_thread
                    .as_ref()
                    .is_some_and(|id| id.to_string() == thread_id)
                {
                    self.push_notice(text);
                }
            }
            LoopEvent::ChatRejected { reason, .. } => {
                tracing::warn!(%reason, "chat command rejected");
                self.push_reason_notice("chat failed: ", &reason);
            }
            LoopEvent::GoalAccepted { thread_id, goal_id } => {
                if self
                    .sidebar
                    .active_thread
                    .as_ref()
                    .is_some_and(|id| id.to_string() == thread_id)
                {
                    self.push_notice(format!("accepted: {goal_id}"));
                }
                self.goal_form.last_accepted = Some(goal_id);
            }
            LoopEvent::CommandRejected { reason } => {
                tracing::warn!(%reason, "goal loop command rejected");
                self.push_reason_notice("", &reason);
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

    /// The project a thread was started in; its runs work there.
    pub(super) fn thread_project(
        &self,
        thread_id: &ThreadId,
    ) -> Option<&workspace_ui::ProjectRecord> {
        let thread = self
            .sidebar
            .threads
            .iter()
            .find(|thread| &thread.id == thread_id)?;
        self.sidebar
            .projects
            .iter()
            .find(|project| project.id == thread.project_id)
    }

    /// The selected project, the one composer mentions resolve in.
    pub(super) fn active_project(&self) -> Option<&workspace_ui::ProjectRecord> {
        let project_id = self.sidebar.selected_project.as_ref()?;
        self.sidebar
            .projects
            .iter()
            .find(|project| &project.id == project_id)
    }

    pub(super) fn active_repo_root(&self) -> Option<PathBuf> {
        self.active_project()
            .map(|project| project.repo_root.clone())
    }

    pub(super) fn focus_panel(&mut self, id: &str) {
        if let Some(tab_path) = self.dock.find_tab(&PanelId::new(id)) {
            let _ = self.dock.set_active_tab(tab_path);
        }
    }
}
