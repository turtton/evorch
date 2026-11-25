//! The Context view of the Usage tab: a run of the selected thread and what its saved
//! context holds, drawn as the legacy Context tab draws a run.
use workbench_protocol::{
    ids::{RunId, ThreadId},
    namespaces::diagnostics::{ContextMessage, ContextMessageRole, RunContextOverview},
    views::{RunRole, RunSummary},
};

use crate::core_client::cache::Cache;
use crate::format::compact_count as compact_tokens;
use crate::panes::usage_widgets::{message_heading, segments_bar};
use crate::theme::text::{h3, muted, section};

pub(super) const NO_THREAD: &str = "Select a thread to inspect its runs.";
pub(super) const NO_RUNS: &str = "No runs in this thread yet.";
pub(super) const NO_CHECKPOINT: &str = "No saved checkpoint for this run yet.";
pub(super) const SHOW_MORE: &str = "Show more";

const SEGMENT_ORDER: [&str; 4] = ["system", "user", "assistant", "tool result"];

fn role_name(role: RunRole) -> &'static str {
    match role {
        RunRole::Orchestrator => "orchestrator",
        RunRole::Worker => "worker",
        RunRole::Planner => "planner",
        RunRole::Reviewer => "reviewer",
        RunRole::Advisor => "advisor",
        RunRole::Other => "other",
    }
}

fn message_role(role: ContextMessageRole) -> &'static str {
    match role {
        ContextMessageRole::System => "system",
        ContextMessageRole::User => "user",
        ContextMessageRole::Assistant => "assistant",
        ContextMessageRole::ToolResult => "tool result",
    }
}

pub(super) struct RunChoice {
    pub run: RunId,
    pub label: String,
}

/// The runs of `thread`: roots first, then the runs they started, each newest first.
pub(super) fn run_choices(cache: &Cache, thread: &ThreadId) -> Vec<RunChoice> {
    let mut runs: Vec<&RunSummary> = cache
        .runs
        .values()
        .filter(|run| run.thread == *thread)
        .collect();
    runs.sort_by(|left, right| {
        left.parent_run
            .is_some()
            .cmp(&right.parent_run.is_some())
            .then_with(|| run_age_key(right).cmp(&run_age_key(left)))
    });
    runs.into_iter()
        .map(|run| RunChoice {
            run: run.id.clone(),
            label: format!("{} · {}", run.id.as_str(), role_name(run.role)),
        })
        .collect()
}

/// A time-ordered id has a 26 digit body and a legacy sequential `run-<n>` a shorter
/// one, so comparing length before text orders both by age.
fn run_age_key(run: &RunSummary) -> (usize, &str) {
    (run.id.as_str().len(), run.id.as_str())
}

pub(super) fn run_picker(
    ui: &mut egui::Ui,
    choices: &[RunChoice],
    current: Option<&RunId>,
) -> Option<RunId> {
    let selected = choices
        .iter()
        .find(|choice| Some(&choice.run) == current)
        .map_or_else(
            || current.map_or("", RunId::as_str).to_owned(),
            |choice| choice.label.clone(),
        );
    let mut picked = None;
    ui.horizontal_wrapped(|ui| {
        ui.label(muted("Run"));
        egui::ComboBox::from_id_salt("context-run")
            .selected_text(selected)
            .show_ui(ui, |ui| {
                for choice in choices {
                    if ui
                        .selectable_label(Some(&choice.run) == current, &choice.label)
                        .clicked()
                    {
                        picked = Some(choice.run.clone());
                    }
                }
            });
    });
    picked
}

fn summary_line(overview: &RunContextOverview) -> String {
    let category = overview
        .category
        .as_deref()
        .map(|category| format!(" · {category}"))
        .unwrap_or_default();
    let model = overview
        .selected_model
        .as_deref()
        .unwrap_or("model not recorded");
    format!(
        "{}{category} · {model} · {} messages · {} compaction checkpoints · saved at {}",
        overview.role, overview.message_count, overview.compaction_checkpoints, overview.phase
    )
}

/// `messages` are the pages read so far; the heading counts what they list against
/// every saved message, as the legacy run view does once all are read.
pub(super) fn overview_body(
    ui: &mut egui::Ui,
    overview: &RunContextOverview,
    messages: &[ContextMessage],
) {
    ui.label(muted(summary_line(overview)));
    let segments: Vec<(&str, u64)> = SEGMENT_ORDER
        .iter()
        .filter_map(|label| {
            overview
                .segments
                .iter()
                .find(|segment| segment.label == *label)
                .map(|segment| (*label, segment.tokens))
        })
        .chain(
            overview
                .segments
                .iter()
                .filter(|segment| !SEGMENT_ORDER.contains(&segment.label.as_str()))
                .map(|segment| (segment.label.as_str(), segment.tokens)),
        )
        .filter(|(_, tokens)| *tokens > 0)
        .collect();
    let estimated: u64 = segments.iter().map(|(_, tokens)| tokens).sum();
    ui.horizontal_wrapped(|ui| {
        ui.label(h3(format!("Saved history {}", compact_tokens(estimated))));
        ui.label(muted("estimate: serialized bytes / 4"));
    });
    segments_bar(ui, &segments, None);
    let listed = messages.iter().filter(|message| message.listed).count();
    let (heading, hidden) = message_heading(
        listed,
        messages.len() - listed,
        overview.message_count as usize,
    );
    ui.label(section(heading));
    if let Some(hidden) = hidden {
        ui.label(muted(hidden));
    }
}

pub(super) fn message_row(ui: &mut egui::Ui, message: &ContextMessage) {
    let text = format!(
        "{} · {} · {}",
        message_role(message.role),
        compact_tokens(message.tokens),
        message.summary
    );
    crate::theme::widgets::compact_row(ui, false, |ui| {
        crate::theme::widgets::fill_label(
            ui,
            text,
            crate::theme::tokens::ROW_DENSE,
            egui::Sense::hover(),
        );
    });
}

pub(super) fn tools(ui: &mut egui::Ui, overview: &RunContextOverview) {
    let count = overview.tools.len() + overview.tools_omitted as usize;
    egui::CollapsingHeader::new(format!("Tools ({count})"))
        .id_salt("core-context-run-tools")
        .show(ui, |ui| {
            if overview.tools.is_empty() && overview.tools_omitted == 0 {
                ui.label(muted("Not recorded for this snapshot."));
            }
            for name in &overview.tools {
                ui.label(name);
            }
            if overview.tools_omitted > 0 {
                ui.label(muted(format!("{} more not listed", overview.tools_omitted)));
            }
        });
}
