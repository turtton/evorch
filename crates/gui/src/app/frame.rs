use event_bus::{AgentRunPhase, Event, EventKind, LifecycleEvent};
use runtime::RunId;
use workspace_ui::{KeyAction, PanelId, ThreadRunPhase, Workspace};

use super::WorkbenchState;
use crate::dock::{from_dock_state, to_dock_state};
use crate::model::tasks::AgentRunSource;

impl<S: AgentRunSource> WorkbenchState<S> {
    pub fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.drain_pump();
        self.diff.poll();
        self.drain_pty(&ctx);
        self.handle_input(&ctx);
        self.render(ui);
    }

    fn drain_pump(&mut self) {
        let events = self
            .pump
            .as_mut()
            .map_or_else(Vec::new, crate::events::EventPump::drain);
        for event in events {
            self.transcripts.apply(&event);
            self.tasks.apply_event(&event);
            self.telemetry.apply_event(&event);
            self.apply_runtime_event(&event);
        }
        self.refresh_active_thread_workspace();
    }

    fn apply_runtime_event(&mut self, event: &Event) {
        match &event.kind {
            EventKind::Lifecycle(LifecycleEvent::AgentRunStarted {
                run_id,
                parent_run_id,
                ..
            }) => {
                self.attach_run(run_id, parent_run_id.as_deref());
            }
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to, .. }) => {
                self.phases.insert(run_id.clone(), phase(*to));
            }
            EventKind::Lifecycle(_)
            | EventKind::Message(_)
            | EventKind::Tool(_)
            | EventKind::Usage(_)
            | EventKind::Provider(_)
            | EventKind::Fault(_)
            | EventKind::AgentMessage(_)
            | EventKind::Compaction(_) => {}
        }
    }

    fn attach_run(&mut self, run_id: &str, parent_run_id: Option<&str>) {
        let parent = parent_run_id.and_then(|parent| {
            self.sidebar
                .threads
                .iter()
                .position(|thread| thread.run_ids.iter().any(|run| run == parent))
        });
        let active = self.sidebar.active_thread.as_ref().and_then(|active| {
            self.sidebar.threads.iter().position(|thread| {
                &thread.id == active
                    && (parent_run_id.is_some()
                        || self.sidebar.selected_project.as_ref() == Some(&thread.project_id))
            })
        });
        if let Some(index) = parent.or(active)
            && !self.sidebar.threads[index]
                .run_ids
                .iter()
                .any(|existing| existing == run_id)
        {
            self.sidebar.threads[index].run_ids.push(run_id.to_owned());
        }
    }

    fn refresh_active_thread_workspace(&mut self) {
        let Some(active_id) = self.sidebar.active_thread.clone() else {
            return;
        };
        let Some(index) = self
            .sidebar
            .threads
            .iter()
            .position(|thread| thread.id == active_id)
        else {
            return;
        };
        for raw_id in self.sidebar.threads[index].run_ids.clone() {
            let Some(run_id) = parse_run_id(&raw_id) else {
                continue;
            };
            let Some(workspace) = self.tasks.inspect(run_id).and_then(|run| run.workspace) else {
                continue;
            };
            if workspace.worktree_path.is_some() {
                self.sidebar.threads[index].branch = workspace.branch;
                self.sidebar.threads[index].worktree_path = workspace.worktree_path;
                break;
            }
        }
    }

    fn drain_pty(&mut self, ctx: &egui::Context) {
        if let Some(pty) = &mut self.pty {
            let output = pty.drain_output();
            if !output.is_empty() {
                self.terminal.feed(&output);
                ctx.request_repaint();
            }
        }
    }

    fn handle_input(&mut self, ctx: &egui::Context) {
        if let Some(action) = ctx.input(|input| self.keymap.action_for_input(input)) {
            self.dispatch(action, ctx);
        }
    }

    fn dispatch(&mut self, action: KeyAction, ctx: &egui::Context) {
        match action {
            KeyAction::FocusAgentPane => self.focus_panel("agent-main"),
            KeyAction::FocusTerminalPane => self.focus_panel("terminal-main"),
            KeyAction::FocusTasksPane => {
                let id = if self.dock.find_tab(&PanelId::new("tasks-main")).is_some() {
                    "tasks-main"
                } else {
                    "agents-main"
                };
                self.focus_panel(id);
            }
            KeyAction::SaveLayout => self.save_layout(),
            KeyAction::ResetLayout => self.reset_layout(ctx),
        }
    }

    fn save_layout(&self) {
        let Some(path) = self.save_path.as_ref() else {
            return;
        };
        match from_dock_state(&self.dock, &self.panels) {
            Ok(workspace) => {
                if let Err(error) = workspace_ui::save_to(&workspace, path) {
                    tracing::warn!(path = %path.display(), %error, "failed to save layout");
                }
            }
            Err(error) => tracing::warn!(%error, "failed to extract workspace"),
        }
    }

    fn reset_layout(&mut self, ctx: &egui::Context) {
        let workspace = Workspace::default_v02();
        match to_dock_state(&workspace) {
            Ok(dock) => {
                self.dock = dock;
                self.panels = workspace.panels;
                ctx.request_repaint();
            }
            Err(error) => tracing::warn!(%error, "failed to reset layout"),
        }
    }
}

fn phase(phase: AgentRunPhase) -> ThreadRunPhase {
    match phase {
        AgentRunPhase::Pending => ThreadRunPhase::Pending,
        AgentRunPhase::Running => ThreadRunPhase::Running,
        AgentRunPhase::Waiting => ThreadRunPhase::Waiting,
        AgentRunPhase::Done => ThreadRunPhase::Done,
        AgentRunPhase::Error => ThreadRunPhase::Error,
    }
}

fn parse_run_id(run_id: &str) -> Option<RunId> {
    run_id.strip_prefix("run-")?.parse().ok().map(RunId::new)
}
