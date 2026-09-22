use super::WorkbenchState;
use crate::model::tasks::AgentRunSource;
use event_bus::{Event, EventKind, OrchestratorEvent};
impl<S: AgentRunSource> WorkbenchState<S> {
    pub(super) fn bind_goal_event(&mut self, event: &Event) {
        if let EventKind::Orchestrator(OrchestratorEvent::GoalCreated {
            thread_id,
            project_id,
            root_run_id,
            ..
        }) = &event.kind
        {
            // Persisted events identify a goal; only the current sidebar may
            // authorize its project/thread binding. A deleted or moved thread
            // must not recreate authority to the previous project's team store.
            let Some(thread) = self.sidebar.threads.iter().find(|thread| {
                thread.id.to_string() == *thread_id && thread.project_id.to_string() == *project_id
            }) else {
                return;
            };
            if !self
                .sidebar
                .projects
                .iter()
                .any(|project| project.id == thread.project_id)
            {
                return;
            }
            self.sink.bind_goal_context(
                &thread.id.to_string(),
                &thread.project_id.to_string(),
                root_run_id,
            );
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
        egui::Window::new("実行の診断")
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
                if ui.button("更新").clicked() {
                    self.restore_status = None;
                }
                if self.restore_status.is_none() && !self.diagnostics_run.is_empty() {
                    self.restore_status =
                        Some(self.sink.restore_diagnostics(&self.diagnostics_run));
                }
                if let Some(row) = self.telemetry.row(&self.diagnostics_run) {
                    ui.label(format!("現在: {}", row.activity_label()));
                    ui.label(row.diagnostics_label());
                }
                match &self.restore_status {
                    Some(Ok(Some(status))) => {
                        ui.separator();
                        let saved = u64::try_from(status.last_successful_checkpoint_at_ns)
                            .ok()
                            .and_then(|ns| {
                                std::time::UNIX_EPOCH
                                    .checked_add(std::time::Duration::from_nanos(ns))
                            });
                        let age = saved.and_then(|at| at.elapsed().ok());
                        ui.label(match age {
                            Some(age) => format!("最終保存: {} 秒前", age.as_secs()),
                            None => "最終保存: 時刻を確認できません".into(),
                        });
                        ui.label(format!(
                            "{} messages / {} compacted checkpoints",
                            status.message_count, status.compaction_checkpoint_count
                        ));
                        ui.label(if status.disk_restorable {
                            "保存済み履歴から復元できます"
                        } else if status.history_available_with_current_authority {
                            "現在の権限・設定を再発行すれば履歴を利用できます"
                        } else {
                            "復元には状態の確認が必要です"
                        });
                        if let Some(reason) = &status.refusal_reason {
                            ui.label(reason);
                        }
                        for call in &status.interrupted_tool_calls {
                            ui.label(format!(
                                "未確認の実行: {} ({})",
                                call.tool_name, call.call_id
                            ));
                        }
                        if let Some(task) = &status.durable_task_id {
                            ui.label(format!("Durable task: {task}"));
                        }
                    }
                    Some(Ok(None)) => {
                        ui.label("保存済みチェックポイントがありません");
                    }
                    Some(Err(error)) => {
                        ui.label(error);
                    }
                    None => {
                        ui.label("実行を選択してください");
                    }
                }
            });
        self.diagnostics_open = open;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::DemoSource;

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
