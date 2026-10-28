// allow: SIZE_OK - #110 の相関 join は既存 fold 順序に依存するため、フレーム全体の分割は別変更とする。
use event_bus::{AgentRunPhase, Event, EventKind, LifecycleEvent};
use runtime::RunId;
use std::time::{Duration, Instant};
use workspace_ui::{KeyAction, PanelId, ThreadRunPhase, Workspace};

use super::WorkbenchState;
use crate::dock::{from_dock_state, to_dock_state};
use crate::model::tasks::AgentRunSource;
use crate::model::transcript::TranscriptEntry;

impl<S: AgentRunSource> WorkbenchState<S> {
    /// Process validated runtime events even when eframe skips painting a hidden window.
    pub fn logic(&mut self, ctx: &egui::Context) {
        self.drain_pump(ctx, true);
        self.persist_composer_draft();
        self.dispatch_system_notifications(ctx);
        // Folded events add and bind threads while the window is hidden, too.
        self.sync_thread_mirror(Instant::now());
        ctx.request_repaint_after(Duration::from_millis(200));
    }

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
        self.dispatch_system_notifications(&ctx);
        self.handle_input(&ctx);
        if !self.theme_installed {
            crate::theme::style::install_preset(&ctx, self.theme_preset);
            self.theme_installed = true;
        }
        let (event_count, drain_time, fold_time) = self.drain_pump(&ctx, false);
        self.dispatch_system_notifications(&ctx);
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
            self.warn_stale_auto_candidates();
            ctx.request_repaint();
        }
        if let Some(result) = self.folder_picker.poll() {
            self.apply_picked_folder(result);
            ctx.request_repaint();
        }
        if self.provider_settings.poll_models() {
            ctx.request_repaint();
        }
        if let Some(editor) = self.provider_settings.subscription_mut() {
            editor.load_auth(self.credential_store.clone());
            if editor.poll_login() {
                if matches!(
                    editor.auth,
                    crate::model::subscription_provider::SubscriptionAuthState::SignedIn { .. }
                ) {
                    self.telemetry
                        .subscription_quota
                        .invalidate_account(&editor.account);
                    editor.models.available_models = None;
                    editor.models.models_rx = None;
                    editor.models.models_fetch_state =
                        crate::model::provider_settings::ModelsFetchState::Idle;
                }
                ctx.request_repaint();
            }
            if let Some(url) = editor.take_authorize_url() {
                ctx.open_url(egui::OpenUrl::new_tab(url));
            }
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
        self.sync_thread_mirror(Instant::now());
        self.usage
            .update(&ctx, Instant::now(), self.memory.config.as_ref(), || {
                self.provider_settings.usage_pricing()
            });
        self.context_inspector
            .set_thread(self.sidebar.active_thread.as_ref().map(ToString::to_string));
        self.context_inspector.update(&ctx, self.sink.as_ref());
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
        self.telemetry
            .subscription_quota
            .configure(&self.provider_settings, self.credential_store.clone());
        self.telemetry
            .subscription_quota
            .claude
            .poll(Instant::now());
        self.telemetry
            .subscription_quota
            .cursor
            .poll(Instant::now());
        let ownership_started = Instant::now();
        self.ownership_ui(ui);
        let ownership_time = ownership_started.elapsed();
        let render_started = Instant::now();
        self.render(ui);
        crate::panes::image_viewer::show(&ctx);
        self.open_requested_artifacts(&ctx);
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
        if super::frame_trace::enabled() {
            self.frame_trace
                .get_or_insert_with(|| super::frame_trace::FrameTrace::new(frame_started))
                .record_pass(
                    &ctx,
                    frame_time,
                    super::frame_trace::PassSections {
                        drain: drain_time,
                        fold: fold_time,
                        sink: sink_time,
                        ownership: ownership_time,
                        render: render_time,
                        draft: draft_time,
                    },
                );
        }
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

    fn drain_pump(&mut self, ctx: &egui::Context, logic: bool) -> (usize, Duration, Duration) {
        let frame = ctx.cumulative_frame_nr();
        // eframe calls logic and ui in the same frame; egui can also rerun a
        // discarded UI pass. Neither may read the receipts a second time.
        let same_frame = self.last_pump_frame == Some(frame);
        let hidden_tick = logic && self.last_pump_logic_only;
        if !logic {
            self.last_pump_logic_only = false;
        }
        if same_frame && !hidden_tick {
            return (0, Duration::ZERO, Duration::ZERO);
        }
        // run_logic does not increment egui's frame counter while hidden. Each
        // consecutive logic-only tick is still a new opportunity for repair.
        self.last_pump_logic_only = logic;
        self.last_pump_frame = Some(frame);
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
        let events: Vec<_> = events.into_iter().collect();
        for event in &events {
            self.note_receipt_event(event);
        }
        // A queued input may be consumed without a pump (headless fixtures or
        // lagged notifications). Poll only unsettled work at this frame boundary.
        if self
            .follow_up_queues
            .values()
            .any(|rows| rows.iter().any(|row| !row.undelivered))
        {
            self.receipt_reconciliation.dirty = true;
        }
        let mut receipts = self.read_receipts_if_due(Instant::now());
        // Snapshots precede provider events in storage. Project delivered prompts
        // before folding their replies, even if the receipt notification was lost.
        let mut changed = receipts
            .as_ref()
            .is_some_and(|receipts| self.apply_receipts(receipts, false));
        for event in events {
            self.fold_event(&event, receipts.as_deref_mut().unwrap_or_default());
        }
        // A terminal phase in this batch may settle an Accepted receipt. Reuse
        // the same lookup; admission-pending live runs must stay Queued.
        if let Some(receipts) = receipts {
            changed |= self.apply_receipts(&receipts, false);
        }
        self.finish_receipt_reconciliation(changed);
        self.refresh_active_thread_workspace();
    }

    fn fold_event(&mut self, event: &Event, receipts: &mut [storage::HostInputReceipt]) {
        self.fold_user_question(event);
        self.invalidate_restore_diagnostics(event);
        if self.apply_conversation_event(event) {
            self.save_sidebar();
        }
        self.bind_goal_event(event);
        self.fold_agent_browser_event(event);
        self.sink.observe_lifecycle(event);
        self.apply_runtime_event(event, receipts);
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
                        | TranscriptEntry::Artifacts { .. }
                        | TranscriptEntry::Branch { .. } => None,
                    })
                    .flatten()
            },
        );
        self.notifications.apply_event_named(
            event,
            |call_id| self.transcripts.run_for_call(call_id).map(str::to_owned),
            |run_id| {
                crate::model::tasks::run_display_name(
                    self.tasks.rows(),
                    &self.sidebar.threads,
                    run_id,
                )
                .map(str::to_owned)
            },
        );
        self.ledger.apply(event);
        self.tasks.apply_event(event);
        self.durable_tasks.apply_event(event);
        self.telemetry.apply_event(event);
        self.fold_system_notification(event);
    }

    fn apply_runtime_event(&mut self, event: &Event, receipts: &mut [storage::HostInputReceipt]) {
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
            EventKind::Lifecycle(LifecycleEvent::AgentRunStarted {
                run_id, agent_name, ..
            }) => {
                self.open_subagent_pane(run_id);
                self.unpark_subagent(run_id);
                if let Some(panel) = self
                    .panels
                    .get_mut(&PanelId::new(format!("agent-{run_id}")))
                    && !agent_name.trim().is_empty()
                {
                    panel.title = format!("Transcript: {agent_name}");
                }
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
                    AgentRunPhase::Pending | AgentRunPhase::Running => {
                        self.unpark_subagent(run_id);
                    }
                    AgentRunPhase::Waiting => {}
                }
            }
            EventKind::Ownership(event_bus::OwnershipEvent {
                thread_id,
                owner_id,
                action: event_bus::OwnershipAction::Claimed | event_bus::OwnershipAction::Handoff,
                ..
            }) if self
                .ownership
                .as_ref()
                .is_some_and(|host| host.owner_id() == owner_id) =>
            {
                self.settle_claimed_receipts(thread_id, receipts);
            }
            EventKind::Lifecycle(LifecycleEvent::RootWakeRequested { run_id }) => {
                self.wake_conversation_root(run_id);
            }
            EventKind::Lifecycle(LifecycleEvent::AutoContinuationPaused { run_id, limit }) => {
                self.announce_auto_continuation_pause(run_id, *limit);
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
            | EventKind::Orchestrator(_) => {}
        }
    }

    pub(super) fn refresh_active_thread_workspace(&mut self) {
        let mut changed = false;
        // Reconcile every thread, including inactive ones. Do not cache run IDs/phases:
        // the same RunId may start again after Stopped.
        for thread in &mut self.sidebar.threads {
            let active_root = thread.run_ids.iter().find_map(|raw_id| {
                let run_id = parse_run_id(raw_id)?;
                self.tasks.inspect(run_id)?.workspace?.active_root
            });
            if thread.active_root != active_root {
                thread.active_root = active_root;
                changed = true;
            }
        }
        // fold_event persists before this inspection pass; persist the refreshed roots too.
        if changed {
            self.save_sidebar();
        }
    }

    /// Hand artifacts the user chose to open to the desktop, after panes render.
    fn open_requested_artifacts(&self, ctx: &egui::Context) {
        for path in crate::panes::artifact_card::take_external_open_requests(ctx) {
            crate::model::artifact_opener::ArtifactOpener::open(
                &crate::model::artifact_opener::PortalArtifactOpener,
                &path,
            );
        }
        for path in crate::panes::artifact_card::take_open_requests(ctx) {
            match &self.artifact_opener {
                Some(opener) => opener.open(&path),
                None => tracing::warn!(path = %path.display(), "no artifact opener configured"),
            }
        }
    }

    fn drain_pty(&mut self, ctx: &egui::Context) {
        // The terminal pane re-asserts focus while it renders this frame.
        self.terminals.focused = false;
        let projects = &self.sidebar.projects;
        self.terminals
            .retain_projects(|id| projects.iter().any(|project| &project.id == id));
        if self.terminals.poll() {
            ctx.request_repaint();
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
