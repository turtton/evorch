//! Requests: individual provider attempts, newest first, linked to their thread.

use egui::Ui;
use egui_extras::{Column, TableBuilder};
use workspace_ui::SidebarState;

use super::charts;
use super::{compact_tokens, key_label};
use crate::model::telemetry::pricing::UsagePricing;
use crate::model::usage_stats::{
    CostCalculator, CostMode, UsageDataset, UsageDimension, UsageFact, UsageFilter,
};
use crate::theme::text::muted;
use crate::theme::tokens::palette;

/// Rows shown before "Show more".
pub(super) const PAGE: usize = 200;

/// Status choices for the Requests view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum StatusFilter {
    #[default]
    All,
    Failed,
}

/// Controls owned by the Requests view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RequestControls {
    pub status: StatusFilter,
    pub purpose: Option<String>,
    pub limit: usize,
}

impl Default for RequestControls {
    fn default() -> Self {
        Self {
            status: StatusFilter::All,
            purpose: None,
            limit: PAGE,
        }
    }
}

/// One rendered table row.
pub(super) struct RequestRow {
    when: String,
    model: String,
    provider: String,
    role: String,
    purpose: String,
    status: String,
    failed: bool,
    input: u64,
    output: u64,
    cache_hit: Option<f64>,
    reasoning: Option<u64>,
    ttft_ms: Option<u64>,
    duration_ms: u64,
    tokens_per_second: Option<f64>,
    cost: f64,
    unpriced: bool,
    thread_id: Option<String>,
    run_id: Option<String>,
    request_id: String,
}

pub(super) struct RequestsData {
    rows: Vec<RequestRow>,
    /// Matching requests beyond the shown page.
    total: usize,
    purposes: Vec<String>,
}

impl RequestsData {
    pub(super) fn compute(
        dataset: &UsageDataset,
        filter: &UsageFilter,
        pricing: &UsagePricing,
        mode: CostMode,
        controls: &RequestControls,
    ) -> Self {
        let mut costs = CostCalculator::new(pricing, mode);
        let requests: Vec<&UsageFact> = dataset
            .facts
            .iter()
            .filter(|fact| fact.request.is_some() && filter.matches(fact))
            .collect();
        let purposes = requests
            .iter()
            .map(|fact| UsageDimension::Purpose.key(fact))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let matching: Vec<&UsageFact> = requests
            .into_iter()
            .rev()
            .filter(|fact| controls.status == StatusFilter::All || fact.failed > 0)
            .filter(|fact| {
                controls
                    .purpose
                    .as_ref()
                    .is_none_or(|purpose| *purpose == UsageDimension::Purpose.key(fact))
            })
            .collect();
        let total = matching.len();
        let rows = matching
            .into_iter()
            .take(controls.limit)
            .map(|fact| row(fact, &mut costs))
            .collect();
        Self {
            rows,
            total,
            purposes,
        }
    }

    /// Renders the view; returns the thread a row asked to open.
    pub(super) fn render(
        &self,
        ui: &mut Ui,
        sidebar: &SidebarState,
        controls: &mut RequestControls,
    ) -> Option<String> {
        let mut open = None;
        ui.horizontal_wrapped(|ui| {
            for (status, label) in [(StatusFilter::All, "All"), (StatusFilter::Failed, "Failed")] {
                if charts::segment(ui, controls.status == status, label) {
                    controls.status = status;
                    controls.limit = PAGE;
                }
            }
            let text = controls.purpose.as_deref().map_or_else(
                || "Purpose: all".to_owned(),
                |purpose| {
                    format!(
                        "Purpose: {}",
                        key_label(UsageDimension::Purpose, purpose, sidebar)
                    )
                },
            );
            egui::ComboBox::from_id_salt("usage-request-purpose")
                .selected_text(text)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut controls.purpose, None, "All");
                    for purpose in &self.purposes {
                        let label = key_label(UsageDimension::Purpose, purpose, sidebar);
                        ui.selectable_value(&mut controls.purpose, Some(purpose.clone()), label);
                    }
                });
            ui.label(muted(format!(
                "Showing {} of {} requests",
                self.rows.len(),
                self.total
            )));
            if ui
                .button("Copy CSV")
                .on_hover_text("Copy the shown rows as CSV")
                .clicked()
            {
                ui.ctx().copy_text(self.csv());
            }
        });
        if self.rows.is_empty() {
            ui.label(muted(
                "No individual requests match. Rolled-up days are not listed.",
            ));
            return None;
        }
        let row_height = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
        // Thread and cost lead so they stay visible when the tab is narrow.
        let headers = [
            "Time",
            "Thread",
            "Model",
            "Cost",
            "Status",
            "Role",
            "Purpose",
            "Input",
            "Output",
            "Cache",
            "Reasoning",
            "TTFT",
            "Duration",
            "tok/s",
        ];
        egui::ScrollArea::horizontal()
            .id_salt("usage-requests-scroll")
            .show(ui, |ui| {
                // One line per row: long cells truncate instead of growing the row.
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                TableBuilder::new(ui)
                    .id_salt("usage-requests")
                    .striped(true)
                    .resizable(true)
                    .column(Column::auto().at_least(150.0))
                    .column(Column::initial(150.0).at_least(80.0).clip(true))
                    .column(Column::initial(130.0).at_least(80.0).clip(true))
                    .columns(Column::auto().at_least(48.0), headers.len() - 3)
                    .header(row_height, |mut header| {
                        for title in headers {
                            header.col(|ui| {
                                ui.strong(title);
                            });
                        }
                    })
                    .body(|body| {
                        body.rows(row_height, self.rows.len(), |mut table_row| {
                            let row = &self.rows[table_row.index()];
                            table_row.col(|ui| {
                                ui.label(&row.when).on_hover_text(format!(
                                    "{}\nrun {}",
                                    row.request_id,
                                    row.run_id.as_deref().unwrap_or("—")
                                ));
                            });
                            table_row.col(|ui| match &row.thread_id {
                                Some(thread) => {
                                    let title = key_label(UsageDimension::Thread, thread, sidebar);
                                    if ui
                                        .link(title)
                                        .on_hover_text("Open this conversation")
                                        .clicked()
                                    {
                                        open = Some(thread.clone());
                                    }
                                }
                                None => {
                                    ui.label(muted("—"));
                                }
                            });
                            table_row.col(|ui| {
                                ui.label(&row.model).on_hover_text(&row.provider);
                            });
                            table_row.col(|ui| {
                                ui.label(format!(
                                    "${:.4}{}",
                                    row.cost,
                                    if row.unpriced { "*" } else { "" }
                                ));
                            });
                            table_row.col(|ui| {
                                if row.failed {
                                    ui.colored_label(palette().ERROR_FG, &row.status);
                                } else {
                                    ui.label(&row.status);
                                }
                            });
                            for text in [
                                row.role.clone(),
                                row.purpose.clone(),
                                compact_tokens(row.input),
                                compact_tokens(row.output),
                                row.cache_hit
                                    .map_or_else(|| "—".into(), |rate| format!("{rate:.0}%")),
                                row.reasoning.map_or_else(|| "—".into(), compact_tokens),
                                row.ttft_ms
                                    .map_or_else(|| "—".into(), |ms| format!("{ms} ms")),
                                format!("{:.1} s", row.duration_ms as f64 / 1_000.0),
                                row.tokens_per_second
                                    .map_or_else(|| "—".into(), |speed| format!("{speed:.0}")),
                            ] {
                                table_row.col(|ui| {
                                    ui.label(text);
                                });
                            }
                        });
                    });
            });
        if self.total > self.rows.len() && ui.button("Show more").clicked() {
            controls.limit += PAGE;
        }
        open
    }

    fn csv(&self) -> String {
        let mut csv = String::from(
            "time,request_id,run_id,thread_id,provider,model,role,purpose,status,input_tokens,\
             output_tokens,cache_hit_percent,reasoning_tokens,ttft_ms,duration_ms,cost_usd\n",
        );
        let field = |value: &str| {
            if value.contains([',', '"', '\n']) {
                format!("\"{}\"", value.replace('"', "\"\""))
            } else {
                value.to_owned()
            }
        };
        for row in &self.rows {
            let optional = |value: Option<String>| value.unwrap_or_default();
            csv.push_str(
                &[
                    field(&row.when),
                    field(&row.request_id),
                    field(row.run_id.as_deref().unwrap_or("")),
                    field(row.thread_id.as_deref().unwrap_or("")),
                    field(&row.provider),
                    field(&row.model),
                    field(&row.role),
                    field(&row.purpose),
                    field(&row.status),
                    row.input.to_string(),
                    row.output.to_string(),
                    optional(row.cache_hit.map(|rate| format!("{rate:.1}"))),
                    optional(row.reasoning.map(|tokens| tokens.to_string())),
                    optional(row.ttft_ms.map(|ms| ms.to_string())),
                    row.duration_ms.to_string(),
                    format!("{:.6}", row.cost),
                ]
                .join(","),
            );
            csv.push('\n');
        }
        csv
    }
}

fn row(fact: &UsageFact, costs: &mut CostCalculator<'_>) -> RequestRow {
    let detail = fact.request.as_ref();
    let (cost, unpriced) = costs.price(fact);
    let failed = fact.failed > 0;
    let unknown = |value: &Option<String>| value.clone().unwrap_or_else(|| "—".into());
    RequestRow {
        when: format!(
            "{} {}",
            fact.day,
            detail.map_or("", |detail| detail.time.as_str())
        ),
        model: fact.model.clone(),
        provider: fact.provider_label(),
        role: unknown(&fact.role),
        purpose: unknown(&fact.purpose),
        status: if failed {
            detail
                .and_then(|detail| detail.failure.clone())
                .unwrap_or_else(|| "failed".into())
        } else {
            detail
                .and_then(|detail| detail.finish_reason.clone())
                .unwrap_or_else(|| "ok".into())
        },
        failed,
        input: fact.tokens.input,
        output: fact.tokens.output,
        cache_hit: (fact.tokens.input > 0).then(|| fact.tokens.cache_hit_rate()),
        reasoning: detail.and_then(|detail| detail.reasoning),
        ttft_ms: detail.and_then(|detail| detail.ttft_ms),
        duration_ms: fact.duration_sum_ms,
        tokens_per_second: (fact.duration_sum_ms > 0 && fact.tokens.output > 0)
            .then(|| fact.tokens.output as f64 / (fact.duration_sum_ms as f64 / 1_000.0)),
        cost,
        unpriced,
        thread_id: fact.thread_id.clone(),
        run_id: detail.and_then(|detail| detail.run_id.clone()),
        request_id: detail.map_or_else(String::new, |detail| detail.request_id.clone()),
    }
}
