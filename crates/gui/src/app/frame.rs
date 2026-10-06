// allow: SIZE_OK - #110 の相関 join は既存 fold 順序に依存するため、フレーム全体の分割は別変更とする。
use event_bus::{AgentRunPhase, Event, EventKind, LifecycleEvent};
use runtime::RunId;
use std::time::{Duration, Instant};
use workspace_ui::{KeyAction, PanelId, ThreadRunPhase, Workspace};

use super::WorkbenchState;
use crate::dock::{from_dock_state, to_dock_state};
use crate::model::commands::apply_orchestrator_event;
use crate::model::tasks::AgentRunSource;
use crate::model::transcript::TranscriptEntry;

impl<S: AgentRunSource> WorkbenchState<S> {
    pub fn with_quota_backend(
        mut self,
        backend: Box<dyn crate::model::telemetry::quota::QuotaBackend>,
    ) -> Self {
        self.telemetry.quota = crate::model::telemetry::quota::QuotaState::with_backend(backend);
        self
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let frame_started = Instant::now();
        let ctx = ui.ctx().clone();
        self.handle_input(&ctx);
        if !self.theme_installed {
            crate::theme::style::install_preset(&ctx, self.theme_preset);
            self.theme_installed = true;
        }
        let (event_count, drain_time, fold_time) = self.drain_pump();
        self.poll_external();
        self.poll_auto_titles();
        if self.external_command_running() && ui.button("Cancel external command").clicked() {
            self.cancel_external_command();
        }
        self.diff.set_repo_root(self.active_repo_root());
        let sink_started = Instant::now();
        for event in self.sink.poll() {
            self.apply_loop_event(event);
        }
        let sink_time = sink_started.elapsed();
        ctx.request_repaint_after(std::time::Duration::from_millis(200));
        self.diff.poll();
        self.drain_pty(&ctx);
        self.poll_provider_save();
        self.poll_routing_save();
        self.poll_project_role_profile();
        self.poll_self_improvement_save();
        self.poll_storage_settings();
        if self.provider_settings.catalog.poll() {
            ctx.request_repaint();
        }
        if let Some(result) = self.folder_picker.poll() {
            self.apply_picked_folder(result);
            ctx.request_repaint();
        }
        if self.provider_settings.poll_models() {
            ctx.request_repaint();
        }
        self.prepare_codex_editor();
        if self.codex_auth_mut().poll() {
            ctx.request_repaint();
        }
        if let Some(url) = self.codex_auth_mut().take_url_to_open() {
            ctx.open_url(egui::OpenUrl::new_tab(url));
        }
        self.telemetry.refresh_costs(&self.provider_settings);
        self.sync_usage_ledger(Instant::now());
        self.usage
            .update(&ctx, Instant::now(), self.memory.config.as_ref(), || {
                self.provider_settings.usage_pricing()
            });
        let project = self
            .sidebar
            .selected_project
            .as_ref()
            .map(ToString::to_string);
        self.context_inspector
            .update(&ctx, self.sink.as_ref(), project.as_deref());
        self.telemetry
            .quota
            .configure_profiles(&self.provider_settings, self.credential_store.clone());
        self.telemetry.quota.poll(std::time::Instant::now());
        self.telemetry
            .kimi_quota
            .configure(&self.provider_settings, self.credential_store.clone());
        self.telemetry
            .kimi_quota
            .state
            .poll(std::time::Instant::now());
        let ownership_started = Instant::now();
        self.ownership_ui(ui);
        let ownership_time = ownership_started.elapsed();
        let render_started = Instant::now();
        self.render(ui);
        let render_time = render_started.elapsed();
        let draft_started = Instant::now();
        self.persist_composer_draft();
        let draft_time = draft_started.elapsed();
        self.render_restore_diagnostics(&ctx);
        if self.provider_settings.openai_mut().is_some_and(|editor| {
            matches!(
                editor.models_fetch_state,
                crate::model::provider_settings::ModelsFetchState::Loading
            )
        }) || self.codex_auth().is_authenticating()
            || self.provider_save_rx.is_some()
            || self.folder_picker.is_busy()
            || self.provider_settings.catalog.is_busy()
        {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }
        let frame_time = frame_started.elapsed();
        // A slow frame can be diagnosed after it returns without recording user content or
        // emitting an event back into the queue being measured. Bound repeated warnings.
        if frame_time >= Duration::from_millis(200)
            && self.last_slow_frame_log.is_none_or(|last| {
                frame_started.saturating_duration_since(last) >= Duration::from_secs(30)
            })
        {
            self.last_slow_frame_log = Some(frame_started);
            let measured =
                drain_time + fold_time + sink_time + ownership_time + render_time + draft_time;
            tracing::warn!(
                target: "gui::frame",
                frame_ms = frame_time.as_secs_f64() * 1000.0,
                events = event_count,
                drain_ms = drain_time.as_secs_f64() * 1000.0,
                fold_ms = fold_time.as_secs_f64() * 1000.0,
                sink_ms = sink_time.as_secs_f64() * 1000.0,
                ownership_ms = ownership_time.as_secs_f64() * 1000.0,
                render_ms = render_time.as_secs_f64() * 1000.0,
                draft_ms = draft_time.as_secs_f64() * 1000.0,
                other_ms = frame_time.saturating_sub(measured).as_secs_f64() * 1000.0,
                "slow GUI frame"
            );
        }
    }

    fn drain_pump(&mut self) -> (usize, Duration, Duration) {
        let drain_started = Instant::now();
        let events = self
            .pump
            .as_mut()
            .map_or_else(Vec::new, crate::events::EventPump::drain);
        let drain_time = drain_started.elapsed();
        let event_count = events.len();
        let fold_started = Instant::now();
        self.apply_events(events);
        (event_count, drain_time, fold_started.elapsed())
    }

    /// Synchronous fold for fixtures/tests/headless capture; production uses
    /// `EventPump`.
    pub fn apply_events(&mut self, events: impl IntoIterator<Item = Event>) {
        for event in events {
            self.fold_event(&event);
        }
        self.refresh_active_thread_workspace();
    }

    fn fold_event(&mut self, event: &Event) {
        self.fold_user_question(event);
        self.invalidate_restore_diagnostics(event);
        if self.apply_conversation_event(event) {
            self.save_sidebar();
        }
        self.bind_goal_event(event);
        self.sink.observe_lifecycle(event);
        self.apply_runtime_event(event);
        self.pending_approvals.apply_event(
            event,
            |call_id| self.transcripts.run_for_call(call_id).map(str::to_owned),
            |run_id, original_call_id| {
                self.transcripts
                    .run(run_id)?
                    .entries()
                    .iter()
                    .rev()
                    .find_map(|entry| match entry {
                        TranscriptEntry::Tool { call_id, input, .. }
                            if call_id == original_call_id =>
                        {
                            Some(input.clone())
                        }
                        TranscriptEntry::Tool { .. }
                        | TranscriptEntry::Error { .. }
                        | TranscriptEntry::UserMessage { .. }
                        | TranscriptEntry::Notice { .. }
                        | TranscriptEntry::SandboxReview { .. }
                        | TranscriptEntry::Compaction { .. }
                        | TranscriptEntry::Message { .. }
                        | TranscriptEntry::Reasoning { .. }
                        | TranscriptEntry::AgentMessage { .. }
                        | TranscriptEntry::TurnEnd { .. }
                        | TranscriptEntry::Branch { .. } => None,
                    })
                    .flatten()
            },
        );
        self.notifications.apply_event(event, |call_id| {
            self.transcripts.run_for_call(call_id).map(str::to_owned)
        });
        self.ledger.apply(event);
        self.tasks.apply_event(event);
        self.durable_tasks.apply_event(event);
        self.telemetry.apply_event(event);
    }

    fn apply_runtime_event(&mut self, event: &Event) {
        match &event.kind {
            EventKind::Lifecycle(LifecycleEvent::EscalationRequested {
                source_run_id,
                new_run_id,
                ..
            }) => {
                if self.is_conversation_run(new_run_id) {
                    self.prepare_escalation_thread(source_run_id, new_run_id);
                }
            }
            EventKind::Lifecycle(LifecycleEvent::AgentRunStarted { run_id, .. }) => {
                self.open_subagent_pane(run_id);
            }
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to, .. }) => {
                self.phases.insert(run_id.clone(), phase(*to));
                match to {
                    AgentRunPhase::Stopped | AgentRunPhase::Done | AgentRunPhase::Error => {
                        let has_pane = self
                            .dock
                            .find_tab(&PanelId::new(format!("agent-{run_id}")))
                            .is_some();
                        let known_conversation = self.is_conversation_run(run_id);
                        if has_pane || !known_conversation {
                            self.park_completed_subagent(run_id);
                        }
                    }
                    AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting => {}
                }
            }
            EventKind::Lifecycle(_)
            | EventKind::Ledger(_)
            | EventKind::Message(_)
            | EventKind::Tool(_)
            | EventKind::Usage(_)
            | EventKind::Provider(_)
            | EventKind::Fault(_)
            | EventKind::AgentMessage(_)
            | EventKind::Compaction(_)
            | EventKind::Diagnostic(_)
            | EventKind::Ownership(_)
            | EventKind::Snapshot(_) => {}
            // goal ループ状態の UI 反映は T1.5 の reducer で接続する。
            EventKind::Orchestrator(ev) => {
                apply_orchestrator_event(&mut self.merge.view, &mut self.loop_status, ev);
            }
        }
    }

    pub(super) fn refresh_active_thread_workspace(&mut self) {
        let mut changed = false;
        // Reconcile every thread, including inactive ones. Do not cache run IDs/phases:
        // the same RunId may reattach a retained workspace after Stopped.
        for thread in &mut self.sidebar.threads {
            let mut isolated = None;
            let mut shared = None;
            for raw_id in &thread.run_ids {
                let Some(run_id) = parse_run_id(raw_id) else {
                    continue;
                };
                let Some(workspace) = self.tasks.inspect(run_id).and_then(|run| run.workspace)
                else {
                    continue;
                };
                match workspace.mode {
                    runtime::WorkspaceMode::Isolated if workspace.worktree_path.is_some() => {
                        isolated.get_or_insert(workspace);
                    }
                    runtime::WorkspaceMode::Shared if workspace.active_root.is_some() => {
                        shared.get_or_insert(workspace);
                    }
                    _ => {}
                }
            }
            let (branch, worktree_path, active_root) =
                isolated.or(shared).map_or((None, None, None), |workspace| {
                    (
                        workspace.branch,
                        workspace.worktree_path,
                        workspace.active_root,
                    )
                });
            if thread.branch != branch
                || thread.worktree_path != worktree_path
                || thread.active_root != active_root
            {
                thread.branch = branch;
                thread.worktree_path = worktree_path;
                thread.active_root = active_root;
                changed = true;
            }
        }
        // fold_event persists before this inspection pass; persist the refreshed roots too.
        if changed {
            self.save_sidebar();
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
            if action == KeyAction::CycleAgentRole && self.settings_owns_input() {
                return;
            }
            self.dispatch(action, ctx);
        }
    }

    fn dispatch(&mut self, action: KeyAction, ctx: &egui::Context) {
        match action {
            KeyAction::FocusAgentPane => self.focus_panel("agent-main"),
            KeyAction::FocusTerminalPane => self.focus_panel("terminal-main"),
            KeyAction::FocusTasksPane => self.focus_panel("tasks-main"),
            KeyAction::SaveLayout => self.save_layout(),
            KeyAction::ResetLayout => self.reset_layout(ctx),
            KeyAction::CycleAgentRole => self.composer.toggle_role(),
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
                self.register_work_panels();
                ctx.request_repaint();
            }
            Err(error) => tracing::warn!(%error, "failed to reset layout"),
        }
    }
}

pub(super) fn phase(phase: AgentRunPhase) -> ThreadRunPhase {
    match phase {
        AgentRunPhase::Pending => ThreadRunPhase::Pending,
        AgentRunPhase::Running => ThreadRunPhase::Running,
        AgentRunPhase::Waiting => ThreadRunPhase::Waiting,
        AgentRunPhase::Done => ThreadRunPhase::Done,
        AgentRunPhase::Error => ThreadRunPhase::Error,
        AgentRunPhase::Stopped => ThreadRunPhase::Stopped,
    }
}

fn parse_run_id(run_id: &str) -> Option<RunId> {
    run_id.parse().ok()
}
