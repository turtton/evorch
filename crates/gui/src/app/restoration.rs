use super::WorkbenchState;
use crate::model::tasks::AgentRunSource;
use event_bus::{Event, EventKind, OrchestratorEvent};
impl<S: AgentRunSource> WorkbenchState<S> {
    pub(super) fn bind_goal_event(&mut self, event: &Event) {
        if let EventKind::Orchestrator(OrchestratorEvent::ThreadGoalUpdated { snapshot }) =
            &event.kind
            && let Some(thread) = self.sidebar.threads.iter().find(|thread| {
                thread.id.to_string() == snapshot.thread_id
                    && self
                        .sidebar
                        .projects
                        .iter()
                        .any(|project| project.id == thread.project_id)
            })
        {
            self.sink
                .bind_thread_goal(snapshot, &thread.project_id.to_string());
        }
        if let EventKind::Orchestrator(OrchestratorEvent::ThreadTodoUpdated { snapshot }) =
            &event.kind
            && self.thread_todos.get(&snapshot.thread_id) == Some(snapshot)
            && self.sidebar.threads.iter().any(|thread| {
                thread.id.to_string() == snapshot.thread_id
                    && self
                        .sidebar
                        .projects
                        .iter()
                        .any(|project| project.id == thread.project_id)
            })
        {
            self.sink.bind_thread_todo(snapshot);
        }
        if let EventKind::Lifecycle(event_bus::LifecycleEvent::EscalationRequested {
            new_run_id,
            ..
        }) = &event.kind
            && let Some(thread) = self.sidebar.threads.iter().find(|thread| {
                thread.escalation_source_run_id.is_some()
                    && thread.run_ids.contains(new_run_id)
                    && self
                        .sidebar
                        .projects
                        .iter()
                        .any(|project| project.id == thread.project_id)
            })
        {
            self.sink.bind_goal_context(
                &thread.id.to_string(),
                &thread.project_id.to_string(),
                new_run_id,
            );
            // A procedure update can precede creation of the sidebar child during replay.
            if let Some(snapshot) = self.thread_todos.get(&thread.id.to_string()) {
                self.sink.bind_thread_todo(snapshot);
            }
            // A goal update can precede creation of the sidebar child during replay.
            if let Some(snapshot) = self.thread_goals.get(&thread.id.to_string()) {
                self.sink
                    .bind_thread_goal(snapshot, &thread.project_id.to_string());
            }
        }
    }
    pub(super) fn invalidate_restore_diagnostics(&mut self, event: &Event) {
        let run = match &event.kind {
            EventKind::Tool(
                event_bus::ToolEvent::ToolStarted { run_id, .. }
                | event_bus::ToolEvent::ToolCompleted { run_id, .. },
            ) => run_id.as_deref(),
            EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStateChanged {
                run_id,
                ..
            })
            | EventKind::Compaction(event_bus::CompactionEvent::Compacted { run_id, .. }) => {
                Some(run_id.as_str())
            }
            EventKind::Diagnostic(d)
                if matches!(
                    d.code.as_str(),
                    "ContextCheckpointSaved" | "ContextSnapshotFailed"
                ) =>
            {
                d.run_id.as_deref()
            }
            _ => None,
        };
        if run == Some(self.diagnostics_run.as_str()) {
            self.restore_status = None;
        }
    }

    pub(super) fn render_restore_diagnostics(&mut self, ctx: &egui::Context) {
        if !self.diagnostics_open {
            return;
        }
        let runs = self
            .sidebar
            .threads
            .iter()
            .find(|t| Some(&t.id) == self.sidebar.active_thread.as_ref())
            .map(|t| t.run_ids.clone())
            .unwrap_or_default();
        if !runs.contains(&self.diagnostics_run) {
            self.diagnostics_run = runs.last().cloned().unwrap_or_default();
            self.restore_status = None;
        }
        let mut open = true;
        egui::Window::new("Run diagnostics")
            .id(egui::Id::new("restore-diagnostics"))
            .open(&mut open)
            .show(ctx, |ui| {
                egui::ComboBox::from_id_salt("diagnostic-run")
                    .selected_text(&self.diagnostics_run)
                    .show_ui(ui, |ui| {
                        for run in &runs {
                            if ui
                                .selectable_value(&mut self.diagnostics_run, run.clone(), run)
                                .changed()
                            {
                                self.restore_status = None;
                            }
                        }
                    });
                if ui.button("Refresh").clicked() {
                    self.restore_status = None;
                }
                if self.restore_status.is_none() && !self.diagnostics_run.is_empty() {
                    self.restore_status =
                        Some(self.sink.restore_diagnostics(&self.diagnostics_run));
                }
                if let Some(row) = self.telemetry.row(&self.diagnostics_run) {
                    ui.label(format!("Current: {}", row.activity_label()));
                    ui.label(row.diagnostics_label());
                }
                match &self.restore_status {
                    Some(Ok(Some(status))) => {
                        ui.separator();
                        let (saved, absolute) = last_saved(
                            status.last_successful_checkpoint_at_ns,
                            std::time::SystemTime::now(),
                        );
                        let response = ui.label(saved);
                        if let Some(absolute) = absolute {
                            response.on_hover_text(absolute);
                        }
                        ui.label(format!(
                            "{} messages / {} compacted checkpoints",
                            status.message_count, status.compaction_checkpoint_count
                        ));
                        ui.label(if status.disk_restorable {
                            "Restorable from saved history"
                        } else if status.history_available_with_current_authority {
                            "History is usable once current authority and settings are reissued"
                        } else {
                            "Restoring requires checking the run state"
                        });
                        if let Some(reason) = &status.refusal_reason {
                            ui.label(reason);
                        }
                        if status.history_available_with_current_authority
                            && !status.interrupted_tool_calls.is_empty()
                        {
                            ui.label("Missing tool results carry over as errors so the conversation can continue. Earlier calls are not re-run automatically.");
                        }
                        for call in &status.interrupted_tool_calls {
                            ui.label(format!(
                                "Unconfirmed tool call: {} ({})",
                                call.tool_name, call.call_id
                            ));
                        }
                        if let Some(task) = &status.durable_task_id {
                            ui.label(format!("Durable task: {task}"));
                        }
                    }
                    Some(Ok(None)) => {
                        ui.label("No saved checkpoint");
                    }
                    Some(Err(error)) => {
                        ui.label(error);
                    }
                    None => {
                        ui.label("Select a run");
                    }
                }
            });
        self.diagnostics_open = open;
    }
}

fn last_saved(at_ns: i64, now: std::time::SystemTime) -> (String, Option<String>) {
    let Some(saved) = u64::try_from(at_ns)
        .ok()
        .and_then(|ns| std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_nanos(ns)))
    else {
        return ("Last saved: time unavailable".into(), None);
    };
    (
        format!("Last saved {}", crate::format::time_since(saved, now)),
        Some(crate::format::utc_timestamp(saved.into())),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::DemoSource;

    #[test]
    fn last_saved_checkpoint_reads_as_relative_time_with_absolute_tooltip() {
        let saved_ns = 1_760_000_000_000_000_000;
        let now = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_760_000_000 + 312);
        assert_eq!(
            last_saved(saved_ns, now),
            (
                "Last saved 5m 12s ago".into(),
                Some("2025-10-09 08:53 UTC".into())
            )
        );
        assert_eq!(last_saved(-1, now).0, "Last saved: time unavailable");
    }

    #[test]
    fn relevant_events_expire_cached_restore_status_without_polling() {
        let mut state =
            WorkbenchState::new(DemoSource(vec![]), &workspace_ui::UiSettings::default()).unwrap();
        state.diagnostics_run = "run-1".into();
        state.restore_status = Some(Ok(None));
        let tool = |run: &str| {
            Event::new(event_bus::ToolEvent::ToolStarted {
                run_id: Some(run.into()),
                call_id: "call-1".into(),
                tool_name: "shell".into(),
                input: None,
                parent_call_id: None,
            })
        };
        state.apply_events([tool("run-2")]);
        assert!(state.restore_status.is_some());
        state.apply_events([tool("run-1")]);
        assert!(state.restore_status.is_none());
        state.restore_status = Some(Ok(None));
        state.apply_events([Event::new(
            event_bus::LifecycleEvent::AgentRunStateChanged {
                run_id: "run-1".into(),
                from: event_bus::AgentRunPhase::Running,
                to: event_bus::AgentRunPhase::Error,
                reason: Some("cancelled".into()),
            },
        )]);
        assert!(state.restore_status.is_none());
    }
}
