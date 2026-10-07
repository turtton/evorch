use crate::model::durable_tasks::DurableTasksModel;
use crate::model::tasks::{AgentRunSource, TaskRow, TasksModel};
use crate::model::telemetry::{TelemetryOverlay, TelemetryRow, ThreadMetrics};
use crate::panes::agents_columns::fit_columns;
use crate::theme::icons;
use crate::theme::text::{muted, semibold};
use crate::theme::tokens::{
    CELL_PAD_X, DOT_SIZE, ROW_DENSE, SP_1, SP_3, agent_phase_color, palette,
};
use crate::theme::widgets::{
    empty_state, ghost, halo_dot, metric, metric_detailed, pane_root, soft_frame, status_dot,
};
use egui::{Align, Button, Label, Layout};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentsAction {
    DrillDown(String),
    ReturnToThread,
    OpenPane(String),
    OpenDefaultPanes,
    OpenTask(String),
    StopRun(String),
}

const HEADERS: [&str; 9] = [
    "run",
    "",
    "name",
    "role",
    "phase",
    "model",
    "provider",
    "activity",
    "tokens (in/out)",
];

pub fn agents_pane<S: AgentRunSource>(
    ui: &mut egui::Ui,
    tasks: &TasksModel<S>,
    telemetry: &TelemetryOverlay,
    durable_tasks: &DurableTasksModel,
) -> Option<AgentsAction> {
    pane_root(ui, "Agents", |ui| {
        let mut action = ui
            .button("Open default panes")
            .clicked()
            .then_some(AgentsAction::OpenDefaultPanes);
        ui.add_space(SP_1);

        if tasks.rows().is_empty() {
            empty_state(
                ui,
                "No agent runs yet",
                "Send a message or /goal in Conversation to start an agent run.",
                None,
            );
            return action;
        }

        let teams = tasks.teams();
        egui::ScrollArea::both().show(ui, |ui| {
            let widths = column_widths(ui, tasks, telemetry);

            render_header_row(ui, &widths);
            for row in tasks.rows() {
                let run_id = row.run_id.to_string();
                let row_telemetry = telemetry.row(&run_id);
                render_data_row(ui, &widths, row, row_telemetry, &mut action);
                ui.push_id(&run_id, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        for task in durable_tasks.tasks_for_run(&run_id) {
                            if ui
                                .link(format!("Task: {}", task.id))
                                .on_hover_text(&task.title)
                                .clicked()
                            {
                                action = Some(AgentsAction::OpenTask(task.id.clone()));
                            }
                        }
                        for (coordinator, team_tasks) in &teams {
                            for task in team_tasks {
                                if matches!(&task.state, runtime::team::ClaimState::Claimed(lease)
                                    if lease.owner_id == run_id)
                                    && ui.link(format!("Team task: {}", task.spec.id)).clicked()
                                {
                                    action = Some(AgentsAction::OpenTask(format!(
                                        "team:{coordinator}:{}",
                                        task.spec.id
                                    )));
                                }
                            }
                        }
                    });
                });
                if let Some(value) = row_telemetry {
                    let now = std::time::Instant::now();
                    ui.horizontal_wrapped(|ui| {
                        for (index, segment) in value
                            .compact_segments_at(now, telemetry.cost(&run_id))
                            .iter()
                            .enumerate()
                        {
                            if index > 0 {
                                ui.label(muted("·"));
                            }
                            ui.label(muted(segment));
                        }
                        if let Some(rate) = value.cache_reuse.average_retention() {
                            ui.label(muted("·"));
                            let mut text = muted(format!("cache {rate:.1}%"));
                            if value.cache_reuse.average_is_low() {
                                text = text.color(palette().WARNING_FG);
                            }
                            ui.label(text).on_hover_text(value.cache_tooltip());
                        }
                        if let Some(ttft) = value.average_ttft_ms() {
                            ui.label(muted(format!(
                                "TTFT {:.1}s",
                                std::time::Duration::from_millis(ttft).as_secs_f64()
                            )));
                        }
                        if let Some(elapsed) = value.elapsed_at(now) {
                            ui.label(muted(format!("Δ {:.1}s", elapsed.as_secs_f64())));
                        }
                    });
                    if value.request_started_at.is_some() && value.request_duration.is_none() {
                        ui.ctx()
                            .request_repaint_after(std::time::Duration::from_millis(100));
                    }
                }
            }
        });
        action
    })
}

fn column_widths<S: AgentRunSource>(
    ui: &egui::Ui,
    tasks: &TasksModel<S>,
    telemetry: &TelemetryOverlay,
) -> Vec<f32> {
    let font_id = egui::TextStyle::Body.resolve(ui.style());
    let text_width = |text: &str| {
        ui.painter()
            .layout_no_wrap(text.into(), font_id.clone(), palette().TEXT)
            .size()
            .x
    };
    let button_pad_x = ui.spacing().button_padding.x;
    let mut natural = [0.0_f32; 9];

    for (index, heading) in HEADERS.iter().enumerate() {
        natural[index] = text_width(heading) + CELL_PAD_X;
    }
    natural[1] = text_width("Open pane") + 2.0 * button_pad_x;

    for row in tasks.rows() {
        let run_id = row.run_id.to_string();
        natural[0] = natural[0].max(text_width(&run_id) + 2.0 * button_pad_x);
        natural[2] = natural[2].max(text_width(&row.name) + CELL_PAD_X);
        natural[3] = natural[3].max(text_width(&row.role) + CELL_PAD_X);
        let status = format!("{:?}", row.status);
        natural[4] = natural[4].max(text_width(&status) + DOT_SIZE + SP_1);

        if let Some(value) = telemetry.row(&run_id) {
            natural[5] = natural[5]
                .max(text_width(value.model.as_deref().unwrap_or("unknown")) + CELL_PAD_X);
            natural[6] = natural[6]
                .max(text_width(value.provider.as_deref().unwrap_or("unknown")) + CELL_PAD_X);
            natural[7] = natural[7].max(text_width(&value.activity_label()) + CELL_PAD_X);
            let usage = value.tokens_label();
            natural[8] = natural[8].max(text_width(&usage) + CELL_PAD_X);
        } else {
            natural[5] = natural[5].max(text_width("unknown") + CELL_PAD_X);
            natural[6] = natural[6].max(text_width("unknown") + CELL_PAD_X);
            natural[7] = natural[7].max(text_width("unknown") + CELL_PAD_X);
            natural[8] = natural[8].max(text_width("0 / 0") + CELL_PAD_X);
        }
    }

    fit_columns(&natural, SP_1, f32::INFINITY)
}

fn render_header_row(ui: &mut egui::Ui, widths: &[f32]) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = SP_1;
        for index in [2, 4, 0, 1, 3, 5, 6, 7, 8] {
            ui.add_sized(
                [widths[index], ROW_DENSE],
                Label::new(muted(HEADERS[index]).strong()).truncate(),
            );
        }
    });
}

fn render_data_row(
    ui: &mut egui::Ui,
    widths: &[f32],
    row: &TaskRow,
    row_telemetry: Option<&TelemetryRow>,
    action: &mut Option<AgentsAction>,
) {
    let run_id = row.run_id.to_string();
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = SP_1;

        ui.add_sized(
            [widths[2], ROW_DENSE],
            Label::new(egui::RichText::new(&row.name).color(palette().TEXT)).truncate(),
        );
        ui.allocate_ui_with_layout(
            egui::vec2(widths[4], ROW_DENSE),
            Layout::left_to_right(Align::Center),
            |ui| {
                status_dot(ui, agent_phase_color(row.status));
                ui.add(Label::new(format!("{:?}", row.status)).truncate());
            },
        );
        if ui
            .add_sized([widths[0], ROW_DENSE], Button::new(&run_id).truncate())
            .clicked()
        {
            *action = Some(AgentsAction::DrillDown(run_id.clone()));
        }
        if ui
            .add_sized([widths[1], ROW_DENSE], Button::new("Open pane").truncate())
            .clicked()
        {
            *action = Some(AgentsAction::OpenPane(run_id.clone()));
        }
        ui.add_sized(
            [widths[3], ROW_DENSE],
            Label::new(egui::RichText::new(&row.role).color(palette().TEXT)).truncate(),
        );
        let model = row_telemetry
            .and_then(|value| value.model.as_deref())
            .unwrap_or("unknown");
        ui.add_sized([widths[5], ROW_DENSE], Label::new(model).truncate());
        let provider = row_telemetry
            .and_then(|value| value.provider.as_deref())
            .unwrap_or("unknown");
        ui.add_sized([widths[6], ROW_DENSE], Label::new(provider).truncate());
        let current_tool = row_telemetry
            .map(|value| value.activity_label())
            .unwrap_or_else(|| "unknown".into());
        ui.add_sized([widths[7], ROW_DENSE], Label::new(current_tool).truncate());
        let usage = row_telemetry.map_or_else(|| "0 / 0".into(), TelemetryRow::tokens_label);
        let response = ui.add_sized([widths[8], ROW_DENSE], Label::new(usage).truncate());
        if let Some(value) = row_telemetry {
            response.on_hover_text(value.diagnostics_label());
        }
    });
}

/// Runs belonging to the selected conversation, including its delegated work.
pub fn subagents_pane<S: AgentRunSource>(
    ui: &mut egui::Ui,
    tasks: &TasksModel<S>,
    telemetry: &TelemetryOverlay,
    durable_tasks: &DurableTasksModel,
    run_ids: &[String],
) -> Option<AgentsAction> {
    pane_root(ui, "Subagents", |ui| {
        let mut action = None;
        let rows: Vec<_> = tasks
            .rows()
            .iter()
            .filter(|row| run_ids.contains(&row.run_id.to_string()))
            .collect();
        if rows.is_empty() {
            empty_state(
                ui,
                "No runs in this thread",
                "Start a conversation to see its agent runs.",
                None,
            );
            return None;
        }
        let teams = tasks.teams();
        egui::ScrollArea::vertical()
            .id_salt("thread-runs")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for row in rows {
                    let run_id = row.run_id.to_string();
                    ui.push_id(&run_id, |ui| {
                        soft_frame(palette().SURFACE).show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal_wrapped(|ui| {
                                halo_dot(
                                    ui,
                                    agent_phase_color(row.status),
                                    row.status == event_bus::AgentRunPhase::Running,
                                );
                                ui.label(semibold(&row.name).color(palette().TEXT));
                                ui.label(muted(format!("{} · {:?}", role_label(row), row.status)));
                                let value = telemetry.row(&run_id);
                                let model = value
                                    .and_then(|value| value.model.as_deref())
                                    .unwrap_or_else(|| if row.model.is_empty() { "unknown" } else { &row.model });
                                let provider = value
                                    .and_then(|value| value.provider.as_deref())
                                    .or_else(|| row.model.split_once('/').map(|(provider, _)| provider))
                                    .unwrap_or("unknown");
                                ui.label(muted(format!("{model} · {provider}")));
                            });
                            ui.horizontal_wrapped(|ui| {
                                if ui
                                    .link(&run_id)
                                    .on_hover_text("Show this run in Conversation")
                                    .clicked()
                                {
                                    action = Some(AgentsAction::DrillDown(run_id.clone()));
                                }
                                let open = ui.add(ghost(muted(icons::with_icon(
                                    icons::ARROW_SQUARE_OUT,
                                    "Open pane",
                                ))));
                                open.widget_info(|| {
                                    egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Open pane")
                                });
                                if open.clicked() {
                                    action = Some(AgentsAction::OpenPane(run_id.clone()));
                                }
                                for task in durable_tasks.tasks_for_run(&run_id) {
                                    if ui
                                        .link(format!("Task: {}", task.id))
                                        .on_hover_text(&task.title)
                                        .clicked()
                                    {
                                        action = Some(AgentsAction::OpenTask(task.id.clone()));
                                    }
                                }
                                for (coordinator, team_tasks) in &teams {
                                    for task in team_tasks {
                                        if matches!(&task.state, runtime::team::ClaimState::Claimed(lease)
                                            if lease.owner_id == run_id)
                                            && ui.link(format!("Team task: {}", task.spec.id)).clicked()
                                        {
                                            action = Some(AgentsAction::OpenTask(format!(
                                                "team:{coordinator}:{}", task.spec.id
                                            )));
                                        }
                                    }
                                }
                                if stop_run_button(ui, &run_id, row.status) {
                                    action = Some(AgentsAction::StopRun(run_id.clone()));
                                }
                            });
                            if let Some(value) = telemetry.row(&run_id) {
                                // Keep useful live activity (model/tool/compaction),
                                // but let the lifecycle label speak for waiting/done.
                                let activity = value.activity_label();
                                if !matches!(activity.as_str(), "agents" | "idle" | "user input") {
                                    ui.label(muted(activity));
                                }
                                render_token_metrics(ui, value);
                            }
                            render_run_metrics(
                                ui,
                                telemetry.thread_metrics(std::slice::from_ref(&run_id)),
                            );
                        });
                    });
                }
            });
        action
    })
}

/// Each run has its own two-click confirmation, using egui's frame time.
fn stop_run_button(ui: &mut egui::Ui, run_id: &str, phase: event_bus::AgentRunPhase) -> bool {
    let id = ui.make_persistent_id(("stop-run-confirmation", run_id));
    if !matches!(
        phase,
        event_bus::AgentRunPhase::Pending
            | event_bus::AgentRunPhase::Running
            | event_bus::AgentRunPhase::Waiting
    ) {
        ui.ctx().data_mut(|data| data.remove::<f64>(id));
        return false;
    }
    let now = ui.input(|input| input.time);
    let deadline = ui.ctx().data_mut(|data| {
        let deadline = data.get_temp::<f64>(id);
        if deadline.is_some_and(|deadline| now >= deadline) {
            data.remove::<f64>(id);
            None
        } else {
            deadline
        }
    });
    let text = if deadline.is_some() {
        egui::RichText::new("Confirm?").color(palette().WARNING_FG)
    } else {
        muted("Stop")
    };
    let response = ui.add(ghost(text)).on_hover_text("Stop this agent run");
    if response.clicked() {
        if deadline.is_some() {
            ui.ctx().data_mut(|data| data.remove::<f64>(id));
            return true;
        }
        ui.ctx().data_mut(|data| data.insert_temp(id, now + 2.0));
        ui.ctx().request_repaint();
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_secs(2));
    } else if let Some(deadline) = deadline {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_secs_f64(deadline - now));
    }
    false
}

fn role_label(row: &TaskRow) -> String {
    match &row.category {
        Some(category) if row.role.eq_ignore_ascii_case("worker") => {
            format!("Worker({category})")
        }
        _ => row.role.clone(),
    }
}

fn render_run_metrics(ui: &mut egui::Ui, metrics: ThreadMetrics) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = SP_3;
        metric(
            ui,
            &metrics
                .cost
                .map_or_else(|| "$—".into(), |cost| format!("${cost:.3}")),
        );
        metric_detailed(
            ui,
            &metrics.cache_reuse.average_label(),
            &metrics.cache_tooltip(),
            metrics.cache_reuse.average_is_low(),
        );
        for label in [
            metrics.average_tok_s.map_or_else(
                || "avg — tok/s".into(),
                |rate| format!("avg {rate:.1} tok/s"),
            ),
            metrics.average_ttft.map_or_else(
                || "avg TTFT —".into(),
                |ttft| {
                    format!(
                        "avg TTFT {}",
                        crate::model::telemetry::latency_label(ttft.as_secs_f64() * 1_000.0)
                    )
                },
            ),
        ]
        .iter()
        {
            metric(ui, label);
        }
    });
}

/// Cumulative input/output tokens and the latest context pressure, each
/// behind its own icon so the numbers are not a bare `in / out (ctx)` triple.
fn render_token_metrics(ui: &mut egui::Ui, row: &TelemetryRow) {
    use crate::panes::usage::compact_tokens;
    let diagnostics = row.diagnostics_label();
    let pressure = row.context_pressure_label();
    let metrics = [
        Some((
            icons::ARROW_UP,
            compact_tokens(row.usage.input),
            "Input tokens sent (cumulative)",
            format!("input {}", row.usage.input),
        )),
        Some((
            icons::ARROW_DOWN,
            compact_tokens(row.usage.output),
            "Output tokens received (cumulative)",
            format!("output {}", row.usage.output),
        )),
        pressure.map(|pressure| {
            (
                icons::STACK,
                pressure.clone(),
                "Context window usage of the latest request",
                format!("context {pressure}"),
            )
        }),
    ];
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = SP_3;
        for (icon, value, hint, label) in metrics.into_iter().flatten() {
            let response = ui.label(muted(icons::with_icon(icon, value)));
            response
                .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &label));
            response.on_hover_text(format!("{hint}\n\n{diagnostics}"));
        }
    });
}

#[cfg(test)]
mod stop_tests {
    use super::*;
    use egui_kittest::{Harness, kittest::Queryable};
    use event_bus::{AgentRunPhase, Event, LifecycleEvent};
    use runtime::{AgentSummary, RunId};

    struct Source(Vec<AgentSummary>);
    impl AgentRunSource for Source {
        fn list(&self) -> Vec<AgentSummary> {
            self.0.clone()
        }
    }

    type State = (TasksModel<Source>, Vec<AgentsAction>);

    fn harness(phases: &[AgentRunPhase]) -> Harness<'static, State> {
        let mut tasks = TasksModel::new(Source(
            phases
                .iter()
                .enumerate()
                .map(|(index, phase)| AgentSummary {
                    run_id: RunId::new(index as u64 + 2),
                    parent_run_id: Some(RunId::new(1)),
                    name: format!("Agent {}", index + 2),
                    role_name: "worker".into(),
                    phase: *phase,
                    model: "test".into(),
                    category: None,
                })
                .collect(),
        ));
        tasks.refresh();
        let run_ids: Vec<_> = tasks
            .rows()
            .iter()
            .map(|row| row.run_id.to_string())
            .collect();
        Harness::builder()
            .with_size(egui::vec2(1000.0, 1400.0))
            .build_ui_state(
                move |ui, (tasks, actions)| {
                    if let Some(action) = subagents_pane(
                        ui,
                        tasks,
                        &Default::default(),
                        &Default::default(),
                        &run_ids,
                    ) {
                        actions.push(action);
                    }
                },
                (tasks, Vec::new()),
            )
    }

    fn frame_at(harness: &mut Harness<'_, State>, time: f64) {
        harness.input_mut().time = Some(time);
        harness.step();
    }

    #[test]
    fn subagents_stop_requires_two_clicks_on_the_same_live_run() {
        let mut h = harness(&[
            AgentRunPhase::Pending,
            AgentRunPhase::Running,
            AgentRunPhase::Waiting,
            AgentRunPhase::Stopped,
            AgentRunPhase::Done,
            AgentRunPhase::Error,
        ]);
        frame_at(&mut h, 0.0);
        assert_eq!(h.query_all_by_label("Stop").count(), 3);
        h.query_all_by_label("Stop").next().unwrap().click();
        frame_at(&mut h, 0.0);
        frame_at(&mut h, 0.0);
        h.query_all_by_label("Stop").next().unwrap().click();
        frame_at(&mut h, 0.5);
        frame_at(&mut h, 0.5);
        assert_eq!(h.query_all_by_label("Confirm?").count(), 2);
        assert!(h.state().1.is_empty());
        h.query_all_by_label("Confirm?").next().unwrap().click();
        frame_at(&mut h, 1.0);
        frame_at(&mut h, 1.0);
        // Rows list the newest run first, so the first live row is run-4.
        assert_eq!(h.state().1, [AgentsAction::StopRun("run-4".into())]);
        assert_eq!(h.query_all_by_label("Confirm?").count(), 1);
    }

    #[test]
    fn subagents_stop_confirmation_expires_and_clears_on_terminal_state() {
        let mut h = harness(&[AgentRunPhase::Running]);
        frame_at(&mut h, 0.0);
        h.get_by_label("Stop").click();
        frame_at(&mut h, 0.0);
        frame_at(&mut h, 1.99);
        h.get_by_label("Confirm?");
        frame_at(&mut h, 2.0);
        h.get_by_label("Stop");
        assert!(h.query_by_label("Confirm?").is_none());
        h.get_by_label("Stop").click();
        frame_at(&mut h, 2.0);
        frame_at(&mut h, 2.0);
        h.get_by_label("Confirm?");
        assert!(h.state().1.is_empty());
        h.state_mut()
            .0
            .apply_event(&Event::new(LifecycleEvent::AgentRunStateChanged {
                run_id: "run-2".into(),
                from: AgentRunPhase::Running,
                to: AgentRunPhase::Stopped,
                reason: None,
            }));
        frame_at(&mut h, 2.5);
        assert!(h.query_by_label("Stop").is_none());
        assert!(h.query_by_label("Confirm?").is_none());
        h.state_mut()
            .0
            .apply_event(&Event::new(LifecycleEvent::AgentRunStateChanged {
                run_id: "run-2".into(),
                from: AgentRunPhase::Stopped,
                to: AgentRunPhase::Running,
                reason: None,
            }));
        frame_at(&mut h, 2.5);
        h.get_by_label("Stop");
        assert!(h.query_by_label("Confirm?").is_none());
    }
}
