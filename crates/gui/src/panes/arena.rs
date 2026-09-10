use arena::{ArenaReport, Confirmation};
use std::collections::BTreeMap;

#[derive(Default)]
pub struct ArenaPane {
    project: String,
    reports: BTreeMap<String, ArenaReport>,
    pending: Option<(String, String)>,
    candidate: Option<String>,
    error: Option<String>,
}

impl ArenaPane {
    pub fn render(&mut self, ui: &mut egui::Ui, source: Option<(&storage::StorageConfig, &str)>) {
        let Some((config, project)) = source else {
            ui.label("Select a project with connected memory storage.");
            return;
        };
        let refresh = ui
            .horizontal_wrapped(|ui| {
                ui.heading("Role evaluation arena");
                ui.button("Refresh").clicked()
            })
            .inner;
        if self.project != project || refresh {
            self.project = project.into();
            self.pending = None;
            self.candidate = None;
            self.reports.clear();
            self.error = None;
            match storage::Database::open(config).and_then(|db| db.eval_traces(project)) {
                Ok(traces) => {
                    let mut groups: BTreeMap<String, Vec<_>> = BTreeMap::new();
                    for trace in traces {
                        groups
                            .entry(trace.arena_id.clone())
                            .or_default()
                            .push(trace);
                    }
                    for (id, traces) in groups {
                        match ArenaReport::from_traces(traces) {
                            Ok(report) => {
                                self.reports.insert(id, report);
                            }
                            Err(error) => self.error = Some(error.to_string()),
                        }
                    }
                }
                Err(error) => self.error = Some(error.to_string()),
            }
        }
        ui.label(crate::theme::text::muted(
            "Hard gate > Pareto frontier > token / latency / ID tiebreak",
        ));
        ui.label("Active routing is unchanged.");
        if let Some(error) = &self.error {
            ui.colored_label(crate::theme::tokens::ERROR_FG, error);
        }
        if self.reports.is_empty() {
            ui.label("No evaluation traces for this project.");
        }
        egui::ScrollArea::both()
            .id_salt("arena_comparisons")
            .show(ui, |ui| {
                for (id, report) in &self.reports {
                    ui.push_id(id, |ui| {
                        ui.separator();
                        ui.monospace(id);
                        let selected = report.selected();
                        egui::Grid::new("comparison").striped(true).show(ui, |ui| {
                            for title in [
                                "Config / role",
                                "Model",
                                "Gate",
                                "Tokens",
                                "Time (ms)",
                                "Routing candidate",
                            ] {
                                ui.strong(title);
                            }
                            ui.end_row();
                            for trace in report.traces() {
                                ui.label(format!("{} / {:?}", trace.config_id, trace.attribution));
                                ui.monospace(&trace.model);
                                ui.label(
                                    trace
                                        .failure
                                        .map_or_else(|| "Pass".into(), |f| format!("{f:?}")),
                                );
                                ui.label(format!(
                                    "{} + {}",
                                    trace.input_tokens, trace.output_tokens
                                ));
                                ui.label(trace.elapsed_ms.to_string());
                                if selected.contains(&trace.config_id) {
                                    if ui.button(format!("Propose {}", trace.config_id)).clicked() {
                                        self.pending = Some((id.clone(), trace.config_id.clone()));
                                        self.candidate = None;
                                    }
                                } else {
                                    ui.label("Not selected");
                                }
                                ui.end_row();
                            }
                        });
                        ui.collapsing("Task and output evidence", |ui| {
                            if let Some(trace) = report.traces().first() {
                                ui.label(&trace.task_spec);
                            }
                            for trace in report.traces() {
                                ui.monospace(&trace.config_id);
                                ui.label(&trace.output);
                            }
                        });
                    });
                }
            });
        if let Some((arena_id, config_id)) = self.pending.clone() {
            ui.separator();
            ui.label(format!(
                "Promote {config_id} from {arena_id} to a routing candidate?"
            ));
            ui.horizontal_wrapped(|ui| {
                if ui.button("Confirm candidate").clicked() {
                    if let Some(report) = self.reports.get(&arena_id) {
                        match report
                            .promote(&config_id, Confirmation::Approved)
                            .map_err(|e| e.to_string())
                            .and_then(|c| {
                                serde_json::to_string_pretty(&c).map_err(|e| e.to_string())
                            }) {
                            Ok(candidate) => self.candidate = Some(candidate),
                            Err(error) => self.error = Some(error),
                        }
                    }
                    self.pending = None;
                }
                if ui.button("Cancel").clicked() {
                    self.pending = None;
                }
            });
        }
        if let Some(candidate) = &self.candidate {
            ui.code(candidate);
            if ui.button("Copy routing candidate").clicked() {
                ui.ctx().copy_text(candidate.clone());
            }
        }
    }
}
