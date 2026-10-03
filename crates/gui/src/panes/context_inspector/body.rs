//! Preview, Run and Compare bodies of the Context tab.

use egui_extras::{Column, TableBuilder};
use runtime::base_context::BaseContextReport;

use super::{ContextInspectorPane, RunActuals, Selection, budget_bar, list_row, usage_line};
use crate::model::context_inspector::{
    MessageRow, compact_tokens, compare_rows, message_rows, preview_segments, run_segments,
    run_summary, source_label, tool_difference,
};
use crate::panes::usage::charts::segment;
use crate::theme::text::{h3, muted, section};
use crate::theme::tokens::palette;

/// Below this width the list and detail stack instead of sitting side by side.
const SPLIT_MIN_WIDTH: f32 = 520.0;

impl ContextInspectorPane {
    pub(super) fn preview_body(&mut self, ui: &mut egui::Ui) {
        let loading = self.preview.is_loading();
        let Some(result) = self.preview.report() else {
            ui.spinner();
            return;
        };
        let report = match result {
            Ok(report) => report.clone(),
            Err(error) => {
                ui.colored_label(palette().ERROR_FG, error);
                return;
            }
        };
        if loading {
            ui.spinner();
        }
        report_header(ui, &report);
        split(ui, |ui, column| match column {
            Column_::List => self.preview_list(ui, &report),
            Column_::Detail => self.preview_detail(ui, &report),
        });
    }

    fn preview_list(&mut self, ui: &mut egui::Ui, report: &BaseContextReport) {
        ui.label(section("System message"));
        for (index, item) in report.sections.iter().enumerate() {
            let label = format!(
                "{}  ·  {}",
                item.kind.label(),
                compact_tokens(item.estimated_tokens)
            );
            if list_row(
                ui,
                self.selection == Selection::Section(index),
                label,
                Some(source_label(&item.source)),
            ) {
                self.selection = Selection::Section(index);
            }
        }
        ui.add_space(6.0);
        ui.label(section(format!(
            "Tools ({}) · {}",
            report.tools.len(),
            compact_tokens(report.tool_tokens())
        )));
        let mut tools: Vec<_> = report.tools.iter().enumerate().collect();
        tools.sort_by_key(|(_, tool)| std::cmp::Reverse(tool.estimated_tokens));
        for (index, tool) in tools {
            let label = format!(
                "{}  ·  {}",
                tool.name,
                compact_tokens(tool.estimated_tokens)
            );
            if list_row(ui, self.selection == Selection::Tool(index), label, None) {
                self.selection = Selection::Tool(index);
            }
        }
        ui.add_space(6.0);
        egui::CollapsingHeader::new(format!("Not included ({})", report.exclusions.len()))
            .id_salt("context-exclusions")
            .default_open(true)
            .show(ui, |ui| {
                for item in &report.exclusions {
                    ui.label(egui::RichText::new(&item.item).strong());
                    ui.label(muted(&item.reason));
                }
            });
        egui::CollapsingHeader::new(format!("Hidden tools ({})", report.excluded_tools.len()))
            .id_salt("context-hidden-tools")
            .show(ui, |ui| {
                for item in &report.excluded_tools {
                    ui.label(format!("{} — {}", item.item, item.reason));
                }
            });
    }

    fn preview_detail(&mut self, ui: &mut egui::Ui, report: &BaseContextReport) {
        match self.selection {
            Selection::Section(index) if index < report.sections.len() => {
                let item = &report.sections[index];
                ui.label(h3(item.kind.label()));
                ui.label(muted(format!(
                    "{} · {} tokens (estimate)",
                    source_label(&item.source),
                    item.estimated_tokens
                )));
                if !item.kind.in_system_message() {
                    ui.label(muted("Appended to the first user message."));
                }
                self.text_view(ui, &item.text, &format!("context-section-{index}"));
            }
            Selection::Tool(index) if index < report.tools.len() => {
                let tool = &report.tools[index];
                ui.label(h3(&tool.name));
                ui.label(muted(format!(
                    "{} tokens (estimate)",
                    tool.estimated_tokens
                )));
                ui.label(&tool.description);
                ui.add_space(4.0);
                let schema = serde_json::to_string_pretty(&tool.input_schema).unwrap_or_default();
                read_only_text(ui, &schema);
            }
            _ => model_facts(ui, report),
        }
    }

    pub(super) fn run_body(&mut self, ui: &mut egui::Ui, actuals: &RunActuals) {
        let view = match &self.run_view {
            Some((_, Ok(Some(view)))) => view.clone(),
            Some((_, Ok(None))) => {
                ui.label("No saved checkpoint for this run yet.");
                return;
            }
            Some((_, Err(error))) => {
                ui.colored_label(palette().ERROR_FG, error);
                return;
            }
            None if self.run.is_empty() => return,
            None => {
                ui.spinner();
                return;
            }
        };
        ui.label(muted(run_summary(&view)));
        let rows = message_rows(&view.messages);
        let estimated: u64 = rows.iter().map(|row| row.tokens).sum();
        let window = actuals
            .estimate
            .as_ref()
            .map_or(0, |estimate| estimate.window_tokens);
        ui.horizontal_wrapped(|ui| {
            ui.label(h3(format!(
                "Saved history {}",
                if window > 0 {
                    usage_line(estimated, window)
                } else {
                    compact_tokens(estimated)
                }
            )));
            ui.label(muted("estimate: serialized bytes / 4"));
        });
        budget_bar(ui, &run_segments(&rows), window.max(estimated));
        actual_line(ui, actuals);
        split(ui, |ui, column| match column {
            Column_::List => {
                ui.label(section(format!("Messages ({})", rows.len())));
                for (index, row) in rows.iter().enumerate() {
                    let label = format!(
                        "{} · {} · {}",
                        row.role,
                        compact_tokens(row.tokens),
                        row.summary
                    );
                    if list_row(ui, self.selection == Selection::Message(index), label, None) {
                        self.selection = Selection::Message(index);
                    }
                }
                ui.add_space(6.0);
                egui::CollapsingHeader::new(format!("Tools ({})", view.tool_names.len()))
                    .id_salt("context-run-tools")
                    .show(ui, |ui| {
                        if view.tool_names.is_empty() {
                            ui.label(muted("Not recorded for this snapshot."));
                        }
                        for name in &view.tool_names {
                            ui.label(name);
                        }
                    });
            }
            Column_::Detail => match self.selection {
                Selection::Message(index) if index < rows.len() => {
                    let row: &MessageRow = &rows[index];
                    ui.label(h3(format!("{} message", row.role)));
                    ui.label(muted(format!("{} tokens (estimate)", row.tokens)));
                    self.text_view(ui, &row.text, &format!("context-message-{index}"));
                }
                _ => {
                    ui.label(muted(
                        "Select a message. History is secret-redacted and ends at the last complete tool round.",
                    ));
                }
            },
        });
    }

    pub(super) fn compare_body(&mut self, ui: &mut egui::Ui) {
        let (Some(left), Some(right)) = (self.compare_left.report(), self.compare_right.report())
        else {
            ui.spinner();
            return;
        };
        let (left, right) = match (left, right) {
            (Ok(left), Ok(right)) => (left, right),
            (Err(error), _) | (_, Err(error)) => {
                ui.colored_label(palette().ERROR_FG, error);
                return;
            }
        };
        let rows = compare_rows(left, right);
        let cell = |tokens: Option<u64>| tokens.map_or_else(|| "—".to_owned(), compact_tokens);
        TableBuilder::new(ui)
            .id_salt("context-compare")
            .striped(true)
            .column(Column::remainder().at_least(140.0))
            .columns(Column::auto().at_least(70.0), 4)
            .header(20.0, |mut header| {
                for title in ["Section", left.role.name(), right.role.name(), "Δ", "Text"] {
                    header.col(|ui| {
                        ui.strong(title);
                    });
                }
            })
            .body(|mut body| {
                for row in &rows {
                    body.row(20.0, |mut table| {
                        table.col(|ui| {
                            ui.label(&row.label);
                        });
                        table.col(|ui| {
                            ui.label(cell(row.left));
                        });
                        table.col(|ui| {
                            ui.label(cell(row.right));
                        });
                        table.col(|ui| {
                            let delta = i128::from(row.right.unwrap_or(0))
                                - i128::from(row.left.unwrap_or(0));
                            ui.label(format!("{delta:+}"));
                        });
                        table.col(|ui| {
                            ui.label(match row.identical {
                                Some(true) => "same",
                                Some(false) => "differs",
                                None => "",
                            });
                        });
                    });
                }
            });
        let (left_only, right_only) = tool_difference(left, right);
        ui.add_space(6.0);
        for (role, names) in [(left.role, left_only), (right.role, right_only)] {
            ui.label(section(format!("Tools only {} sees", role.name())));
            ui.label(if names.is_empty() {
                "none".to_owned()
            } else {
                names.join(", ")
            });
        }
    }

    fn text_view(&mut self, ui: &mut egui::Ui, text: &str, id: &str) {
        ui.horizontal(|ui| {
            if segment(ui, !self.raw, "Rendered") {
                self.raw = false;
            }
            if segment(ui, self.raw, "Raw") {
                self.raw = true;
            }
        });
        egui::ScrollArea::vertical()
            .id_salt(id)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                if self.raw {
                    read_only_text(ui, text);
                } else {
                    crate::panes::markdown_render::render_markdown(ui, text, id);
                }
            });
    }
}

fn report_header(ui: &mut egui::Ui, report: &BaseContextReport) {
    ui.horizontal_wrapped(|ui| {
        ui.label(h3(format!(
            "{} base context {}",
            report.role.name(),
            usage_line(report.total_tokens(), report.context_window)
        )));
        ui.label(muted(format!(
            "{} · estimate: bytes / 4, before the task prompt",
            report.selected_model
        )));
    });
    budget_bar(ui, &preview_segments(report), report.context_window);
}

fn model_facts(ui: &mut egui::Ui, report: &BaseContextReport) {
    ui.label(h3("Model and request settings"));
    let window_source = match report.window_source {
        event_bus::WindowSource::Override => "config override",
        event_bus::WindowSource::Catalog => "model catalog",
        event_bus::WindowSource::Default => "default",
    };
    let mut facts = vec![
        ("Model", report.selected_model.clone()),
        (
            "Context window",
            format!("{} ({window_source})", report.context_window),
        ),
    ];
    if let Some(binding) = &report.binding {
        let generation = &binding.generation;
        let unset = || "provider default".to_owned();
        facts.extend([
            ("Logical model", binding.logical_model.clone()),
            (
                "Preset",
                binding.preset.clone().unwrap_or_else(|| "none".into()),
            ),
            (
                "Reasoning effort",
                generation.reasoning_effort.clone().unwrap_or_else(unset),
            ),
            (
                "Temperature",
                generation
                    .temperature
                    .map_or_else(unset, |value| value.to_string()),
            ),
            (
                "Max output tokens",
                generation
                    .max_tokens
                    .map_or_else(unset, |value| value.to_string()),
            ),
        ]);
    }
    egui::Grid::new("context-model-facts")
        .num_columns(2)
        .spacing([12.0, 4.0])
        .show(ui, |ui| {
            for (label, value) in facts {
                ui.label(muted(label));
                ui.label(value);
                ui.end_row();
            }
        });
    ui.add_space(6.0);
    ui.label(muted(
        "Select a section or tool to read exactly what the model receives.",
    ));
    if !report.available_skills.is_empty() {
        ui.add_space(6.0);
        ui.label(section(format!(
            "Discovered skills ({})",
            report.available_skills.len()
        )));
        for skill in &report.available_skills {
            ui.label(format!("{} — {}", skill.name, skill.description));
        }
    }
}

fn actual_line(ui: &mut egui::Ui, actuals: &RunActuals) {
    let mut parts = Vec::new();
    if let Some(input) = actuals.latest_input {
        let cached = actuals
            .latest_cache_read
            .map(|read| format!(", {} from cache", compact_tokens(read)))
            .unwrap_or_default();
        parts.push(format!(
            "Latest request input (provider): {}{cached}",
            compact_tokens(input)
        ));
    }
    if let Some(estimate) = &actuals.estimate {
        parts.push(format!(
            "runtime estimate {} · instructions {} · tools {} · conversation {} · tool outputs {}",
            usage_line(estimate.projected_tokens, estimate.window_tokens),
            compact_tokens(estimate.instructions),
            compact_tokens(estimate.tool_definitions),
            compact_tokens(estimate.conversation),
            compact_tokens(estimate.tool_outputs),
        ));
    }
    if parts.is_empty() {
        ui.label(muted(
            "No provider usage observed for this run in this session.",
        ));
    } else {
        ui.label(muted(parts.join(" · ")));
    }
}

fn read_only_text(ui: &mut egui::Ui, text: &str) {
    let mut text = text;
    ui.add(
        egui::TextEdit::multiline(&mut text)
            .code_editor()
            .desired_width(f32::INFINITY),
    );
}

#[derive(Clone, Copy)]
enum Column_ {
    List,
    Detail,
}

/// List and detail side by side, or stacked when the tab is narrow.
fn split(ui: &mut egui::Ui, mut add: impl FnMut(&mut egui::Ui, Column_)) {
    if ui.available_width() < SPLIT_MIN_WIDTH {
        egui::ScrollArea::vertical()
            .id_salt("context-stacked")
            .show(ui, |ui| {
                add(ui, Column_::List);
                ui.separator();
                add(ui, Column_::Detail);
            });
        return;
    }
    ui.columns(2, |columns| {
        egui::ScrollArea::vertical()
            .id_salt("context-list")
            .show(&mut columns[0], |ui| add(ui, Column_::List));
        add(&mut columns[1], Column_::Detail);
    });
}
