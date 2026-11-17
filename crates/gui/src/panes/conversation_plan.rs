//! The conversation plan as drawn: the progress band above the composer and
//! the ConversationPlan panel (task tree, the selected task, change history).
//!
//! Everything here only reads. States are an icon and a color, every icon
//! carries its name as tooltip and accessible label, and every figure is the
//! server's [`PlanProgress`] as read; nothing is recomputed.

use egui::{Color32, CornerRadius, RichText, Sense, Stroke, Ui};
use workbench_protocol::namespaces::execution::plan::{
    PlanChange, PlanChangeCause, PlanHistoryEntry, PlanNodeView, PlanProgress, PlanStatusValues,
    PlanSummary, PlanTaskStatus,
};

use crate::model::conversation_plan::{PlanState, completed_percent, is_planned};
use crate::theme::icons;
use crate::theme::tokens::{
    FONT_SMALL, R_MD, ROW_DENSE, SP_1, SP_2, SP_3, palette, text_style_badge,
};
use crate::theme::widgets::{accessible, icon_button};

/// What the band above the composer shows. `summary` is `None` until the
/// first read answered, which looks the same as a conversation without a plan.
#[derive(Debug, Clone, Copy)]
pub struct PlanBand<'a> {
    pub summary: Option<&'a PlanSummary>,
}

/// What the panel asks of its owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanPanelAction {
    Select(String),
    HistoryPage(u32),
}

/// The ConversationPlan panel's id; the band's open button asks to focus it.
pub const PANEL_ID: &str = "conversation-plan-main";
pub const OPEN_LABEL: &str = "Open plan";
const NOT_PLANNED: &str = "Not planned";
const BAR_WIDTH: f32 = 120.0;
const MINI_BAR_WIDTH: f32 = 44.0;
const BAR_HEIGHT: f32 = 6.0;

/// A task state's glyph, name and color.
pub fn status_style(status: PlanTaskStatus) -> (&'static str, &'static str, Color32) {
    match status {
        PlanTaskStatus::Completed => (icons::CHECK_CIRCLE, "Completed", palette().SUCCESS),
        PlanTaskStatus::InProgress => (icons::CIRCLE_HALF, "In progress", palette().RUNNING),
        PlanTaskStatus::Interrupted => (icons::WARNING_CIRCLE, "Interrupted", palette().WARNING),
        PlanTaskStatus::Pending => (icons::CIRCLE, "Pending", palette().TEXT_MUTED),
        PlanTaskStatus::Blocked => (icons::PROHIBIT, "Blocked", palette().ERROR_FG),
    }
}

/// Order the bar paints states in, left to right.
const BAR_ORDER: [PlanTaskStatus; 4] = [
    PlanTaskStatus::Completed,
    PlanTaskStatus::InProgress,
    PlanTaskStatus::Interrupted,
    PlanTaskStatus::Blocked,
];

const fn value(values: &PlanStatusValues, status: PlanTaskStatus) -> u32 {
    match status {
        PlanTaskStatus::Pending => values.pending,
        PlanTaskStatus::InProgress => values.in_progress,
        PlanTaskStatus::Completed => values.completed,
        PlanTaskStatus::Blocked => values.blocked,
        PlanTaskStatus::Interrupted => values.interrupted,
    }
}

/// The state that best describes a whole plan, for the band's leading icon.
pub fn overall_status(progress: &PlanProgress) -> PlanTaskStatus {
    let leaves = &progress.leaf_counts;
    if progress.leaf_count > 0 && leaves.completed == progress.leaf_count {
        PlanTaskStatus::Completed
    } else if leaves.in_progress > 0 {
        PlanTaskStatus::InProgress
    } else if leaves.interrupted > 0 {
        PlanTaskStatus::Interrupted
    } else if leaves.blocked > 0 {
        PlanTaskStatus::Blocked
    } else {
        PlanTaskStatus::Pending
    }
}

/// The band's sentence for screen readers and its hover text.
pub fn band_label(summary: Option<&PlanSummary>) -> String {
    let Some(progress) = summary.map(|s| &s.progress).filter(|p| is_planned(p)) else {
        return NOT_PLANNED.to_owned();
    };
    let leaves = &progress.leaf_counts;
    let mut text = format!(
        "Plan {}% complete, {} of {} tasks completed",
        completed_percent(progress),
        leaves.completed,
        progress.leaf_count
    );
    if leaves.blocked > 0 {
        text.push_str(&format!(", {} blocked", leaves.blocked));
    }
    if leaves.interrupted > 0 {
        text.push_str(&format!(", {} interrupted", leaves.interrupted));
    }
    text
}

fn status_icon(ui: &mut Ui, status: PlanTaskStatus) -> egui::Response {
    let (icon, name, color) = status_style(status);
    labeled_icon(ui, icon, color, name)
}

fn labeled_icon(ui: &mut Ui, icon: &str, color: Color32, name: &str) -> egui::Response {
    let response = ui.label(RichText::new(icon).color(color));
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, name));
    response.on_hover_text(name)
}

fn small(text: impl Into<String>, color: Color32) -> RichText {
    RichText::new(text).size(FONT_SMALL).color(color)
}

/// A bar split by the states' shares, or a dashed empty outline without a plan.
fn progress_bar(ui: &mut Ui, width: f32, shares: Option<&PlanStatusValues>) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, BAR_HEIGHT), Sense::hover());
    let painter = ui.painter_at(rect);
    let radius = CornerRadius::same((BAR_HEIGHT / 2.0) as u8);
    let Some(shares) = shares else {
        let stroke = Stroke::new(1.0, palette().TEXT_MUTED);
        let r = rect.shrink(0.5);
        let corners = [
            r.left_top(),
            r.right_top(),
            r.right_bottom(),
            r.left_bottom(),
            r.left_top(),
        ];
        painter.extend(egui::Shape::dashed_line(&corners, stroke, 3.0, 2.0));
        return response;
    };
    painter.rect_filled(rect, radius, palette().TEXT_MUTED.gamma_multiply(0.3));
    let mut x = rect.left();
    for status in BAR_ORDER {
        let share = f64::from(value(shares, status).min(1_000_000)) / 1_000_000.0;
        let w = (f64::from(rect.width()) * share) as f32;
        if w <= 0.0 {
            continue;
        }
        let segment = egui::Rect::from_min_max(
            egui::pos2(x, rect.top()),
            egui::pos2((x + w).min(rect.right()), rect.bottom()),
        );
        painter.rect_filled(segment, radius, status_style(status).2);
        x += w;
    }
    response
}

/// One row of the composer band. Returns whether the open button was pressed.
pub fn band_row(ui: &mut Ui, band: PlanBand<'_>) -> bool {
    let progress = band.summary.map(|s| &s.progress).filter(|p| is_planned(p));
    let label = band_label(band.summary);
    let mut open = false;
    let row = ui.horizontal(|ui| {
        ui.set_min_height(ROW_DENSE - SP_1);
        let Some(progress) = progress else {
            labeled_icon(ui, icons::CIRCLE, palette().TEXT_MUTED, NOT_PLANNED);
            progress_bar(ui, BAR_WIDTH, None);
            ui.label(small(NOT_PLANNED, palette().TEXT_MUTED));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_enabled_ui(false, |ui| {
                    icon_button(ui, icons::ARROW_SQUARE_OUT, OPEN_LABEL)
                });
            });
            return;
        };
        status_icon(ui, overall_status(progress));
        progress_bar(ui, BAR_WIDTH, Some(&progress.weighted_ppm)).on_hover_text(&label);
        ui.label(small(
            format!("{}%", completed_percent(progress)),
            palette().TEXT,
        ));
        let leaves = &progress.leaf_counts;
        let (icon, _, color) = status_style(PlanTaskStatus::Completed);
        counted(
            ui,
            icon,
            color,
            &format!("{}/{}", leaves.completed, progress.leaf_count),
            "Completed tasks",
        );
        for status in [PlanTaskStatus::Blocked, PlanTaskStatus::Interrupted] {
            let count = value(leaves, status);
            if count > 0 {
                let (icon, name, color) = status_style(status);
                counted(ui, icon, color, &count.to_string(), name);
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            open = icon_button(ui, icons::ARROW_SQUARE_OUT, OPEN_LABEL).clicked();
        });
    });
    row.response
        .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &label));
    open
}

fn counted(ui: &mut Ui, icon: &str, color: Color32, count: &str, name: &str) {
    ui.label(small("·", palette().TEXT_MUTED));
    let response = ui
        .horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            ui.label(small(icon, color));
            ui.label(small(count, palette().TEXT));
        })
        .response;
    let text = format!("{name}: {count}");
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &text));
    let _ = response.on_hover_text(text);
}

/// The ConversationPlan panel. Returns what the user asked for this frame.
pub fn plan_panel(ui: &mut Ui, state: &PlanState) -> Option<PlanPanelAction> {
    let mut action = None;
    let progress = state
        .tree()
        .map(|tree| &tree.progress)
        .or_else(|| state.summary().map(|s| &s.progress));
    egui::Frame::new()
        .inner_margin(egui::Margin::same(SP_2 as i8))
        .show(ui, |ui| {
            legend_row(ui, state, progress);
            if let Some(error) = state.error() {
                ui.label(small(error, palette().ERROR_FG));
            }
            ui.separator();
            let Some(tree) = state.tree().filter(|tree| !tree.nodes.is_empty()) else {
                ui.label(small(
                    if progress.is_some_and(is_planned)
                        || state.tree().is_none() && state.summary().is_none()
                    {
                        ""
                    } else {
                        NOT_PLANNED
                    },
                    palette().TEXT_MUTED,
                ));
                return;
            };
            let tree_height = (ui.available_height() * 0.45).max(ROW_DENSE * 3.0);
            egui::ScrollArea::vertical()
                .id_salt("plan-tree")
                .max_height(tree_height)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for node in &tree.nodes {
                        if node_row(ui, node, state.selected() == Some(node.task_id.as_str())) {
                            action = Some(PlanPanelAction::Select(node.task_id.clone()));
                        }
                    }
                });
            ui.separator();
            egui::ScrollArea::vertical()
                .id_salt("plan-detail")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if let Some(task) = state.task() {
                        detail(ui, &task.node);
                    }
                    if let Some(page) = history(ui, state, &tree.nodes) {
                        action = Some(PlanPanelAction::HistoryPage(page));
                    }
                });
        });
    action
}

fn legend_row(ui: &mut Ui, state: &PlanState, progress: Option<&PlanProgress>) {
    ui.horizontal(|ui| {
        let planned = progress.filter(|p| is_planned(p));
        progress_bar(ui, BAR_WIDTH, planned.map(|p| &p.weighted_ppm));
        if let Some(progress) = planned {
            ui.label(small(
                format!("{}%", completed_percent(progress)),
                palette().TEXT,
            ));
            for status in [
                PlanTaskStatus::Completed,
                PlanTaskStatus::InProgress,
                PlanTaskStatus::Interrupted,
                PlanTaskStatus::Blocked,
                PlanTaskStatus::Pending,
            ] {
                let (icon, name, color) = status_style(status);
                let count = value(&progress.leaf_counts, status);
                counted(ui, icon, color, &count.to_string(), name);
            }
        } else {
            ui.label(small(NOT_PLANNED, palette().TEXT_MUTED));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let revision = state
                .tree()
                .map(|t| t.revision)
                .or_else(|| state.summary().map(|s| s.revision));
            let run = state
                .history()
                .and_then(|h| h.entries.first())
                .and_then(|entry| entry.change.as_ref())
                .and_then(|change| change.source_run_id.as_deref());
            let mut meta = Vec::new();
            if let Some(revision) = revision.filter(|r| r.0 > 0) {
                meta.push(format!("r{}", revision.0));
            }
            if let Some(time) = state.summary().and_then(|s| clock(s.updated_at_unix_ms)) {
                meta.push(time);
            }
            if let Some(run) = run {
                meta.push(run.to_owned());
            }
            if !meta.is_empty() {
                let text = meta.join(" · ");
                let response = ui.label(
                    RichText::new(&text)
                        .text_style(text_style_badge())
                        .color(palette().TEXT_MUTED),
                );
                let hover = format!("Revision and last run: {text}");
                response.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &hover)
                });
            }
        });
    });
}

/// A tree row; returns whether it was clicked.
fn node_row(ui: &mut Ui, node: &PlanNodeView, selected: bool) -> bool {
    let label = node_label(node);
    let fill = if selected {
        palette().SELECTED_ROW
    } else {
        Color32::TRANSPARENT
    };
    let inner = egui::Frame::new()
        .fill(fill)
        .corner_radius(CornerRadius::same(R_MD))
        .inner_margin(egui::Margin::symmetric(SP_1 as i8, 2))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.add_space(SP_3 * node.depth as f32);
                match node.status {
                    Some(status) => {
                        status_icon(ui, status);
                    }
                    None => {
                        progress_bar(ui, MINI_BAR_WIDTH, Some(&node.progress_ppm));
                        ui.label(small(
                            format!("{}%", node.progress_ppm.completed.min(1_000_000) / 10_000),
                            palette().TEXT_MUTED,
                        ));
                    }
                }
                ui.add(egui::Label::new(&node.title).truncate());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let response = ui.label(small(
                        format!("{} {}", icons::SCALES, node.weight),
                        palette().TEXT_MUTED,
                    ));
                    let weight = format!("Weight {}", node.weight);
                    response.widget_info(|| {
                        egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &weight)
                    });
                    let _ = response.on_hover_text(weight);
                });
            });
        });
    let response = ui.interact(
        inner.response.rect,
        ui.id().with(("plan-node", &node.task_id)),
        Sense::click(),
    );
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, &label)
    });
    response.clicked()
}

/// A tree row's accessible name: its title and state (or completion share).
pub fn node_label(node: &PlanNodeView) -> String {
    let state = match node.status {
        Some(status) => status_style(status).1.to_owned(),
        None => format!(
            "{}% complete",
            node.progress_ppm.completed.min(1_000_000) / 10_000
        ),
    };
    format!("{}, {state}, weight {}", node.title, node.weight)
}

fn section(ui: &mut Ui, icon: &str, name: &str, color: Color32, body: &str) {
    ui.horizontal_top(|ui| {
        labeled_icon(ui, icon, color, name);
        ui.add(egui::Label::new(body).wrap());
    });
}

fn detail(ui: &mut Ui, node: &workbench_protocol::namespaces::execution::plan::PlanNodeDetail) {
    ui.horizontal(|ui| {
        match node.status {
            Some(status) => {
                status_icon(ui, status);
            }
            None => {
                progress_bar(ui, MINI_BAR_WIDTH, Some(&node.progress_ppm));
                ui.label(small(
                    format!("{}%", node.progress_ppm.completed.min(1_000_000) / 10_000),
                    palette().TEXT_MUTED,
                ));
            }
        }
        ui.add(egui::Label::new(RichText::new(&node.title).strong()).wrap());
    });
    if !node.description.is_empty() {
        section(
            ui,
            icons::TEXT_ALIGN_LEFT,
            "Description",
            palette().TEXT_MUTED,
            &node.description,
        );
    }
    if !node.result.is_empty() {
        section(ui, icons::FLAG, "Result", palette().SUCCESS, &node.result);
    }
    if !node.blocker.is_empty() {
        egui::Frame::new()
            .stroke(Stroke::new(1.0, palette().ERROR_FG))
            .fill(palette().ERROR_SURFACE)
            .corner_radius(CornerRadius::same(R_MD))
            .inner_margin(egui::Margin::same(SP_1 as i8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                section(
                    ui,
                    icons::PROHIBIT,
                    "Blocker",
                    palette().ERROR_FG,
                    &node.blocker,
                );
            });
    }
    ui.add_space(SP_2);
}

fn change_style(change: &PlanChange) -> (&'static str, &'static str, Option<&str>) {
    match change {
        PlanChange::Added { task_id, .. } => (icons::PLUS, "Added", Some(task_id)),
        PlanChange::Updated { task_id, .. } => (icons::PENCIL_SIMPLE, "Updated", Some(task_id)),
        PlanChange::Moved { task_id, .. } => (icons::ARROWS_DOWN_UP, "Moved", Some(task_id)),
        PlanChange::SubtreeRemoved { task_id, .. } => (icons::TRASH, "Removed", Some(task_id)),
        PlanChange::Cleared { .. } => (icons::BROOM, "Cleared", None),
    }
}

const fn new_status(change: &PlanChange) -> Option<PlanTaskStatus> {
    match change {
        PlanChange::Added { status, .. } => Some(*status),
        PlanChange::Updated { status, .. } => *status,
        PlanChange::Moved { .. }
        | PlanChange::SubtreeRemoved { .. }
        | PlanChange::Cleared { .. } => None,
    }
}

/// The state `task` had before `index` on this page, if an older entry shows it.
fn previous_status(
    entries: &[PlanHistoryEntry],
    index: usize,
    task: &str,
) -> Option<PlanTaskStatus> {
    entries[index + 1..].iter().find_map(|entry| {
        entry
            .change
            .as_ref()?
            .changes
            .iter()
            .rev()
            .find_map(|change| {
                let id = match change {
                    PlanChange::Added { task_id, .. } | PlanChange::Updated { task_id, .. } => {
                        task_id
                    }
                    _ => return None,
                };
                (id == task).then(|| new_status(change)).flatten()
            })
    })
}

/// Local `HH:MM` of a saved change; `None` when the time is unknown.
pub fn clock(unix_ms: Option<i64>) -> Option<String> {
    let at = chrono::DateTime::from_timestamp_millis(unix_ms?)?;
    Some(at.with_timezone(&chrono::Local).format("%H:%M").to_string())
}

/// The change history with its pager; returns a page to show.
fn history(ui: &mut Ui, state: &PlanState, nodes: &[PlanNodeView]) -> Option<u32> {
    let mut page = None;
    let pages = state.history_pages();
    let current = state.history_page();
    ui.horizontal(|ui| {
        labeled_icon(
            ui,
            icons::CLOCK_COUNTER_CLOCKWISE,
            palette().TEXT_MUTED,
            "History",
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let older = ui.add_enabled_ui(current + 1 < pages, |ui| {
                icon_button(ui, icons::CARET_RIGHT, "Older changes")
            });
            if older.inner.clicked() {
                page = Some(current + 1);
            }
            let position = format!("{}/{}", current + 1, pages);
            let response = ui.label(small(&position, palette().TEXT_MUTED));
            let hover = format!("History page {position}");
            accessible(&response, &hover);
            let newer = ui.add_enabled_ui(current > 0, |ui| {
                icon_button(ui, icons::CARET_LEFT, "Newer changes")
            });
            if newer.inner.clicked() {
                page = Some(current.saturating_sub(1));
            }
        });
    });
    let Some(history) = state.history() else {
        return page;
    };
    for (index, entry) in history.entries.iter().enumerate() {
        history_row(ui, &history.entries, index, entry, nodes);
    }
    page
}

fn history_row(
    ui: &mut Ui,
    entries: &[PlanHistoryEntry],
    index: usize,
    entry: &PlanHistoryEntry,
    nodes: &[PlanNodeView],
) {
    let Some(change) = entry.change.as_ref() else {
        ui.label(small(
            format!("r{} · —", entry.revision.0),
            palette().TEXT_MUTED,
        ));
        return;
    };
    for item in &change.changes {
        let (icon, kind, task) = change_style(item);
        let title = task.and_then(|task| {
            nodes
                .iter()
                .find(|node| node.task_id == task)
                .map(|node| node.title.as_str())
        });
        let time = clock(entry.updated_at_unix_ms);
        let mut label = format!("Revision {}, {kind}", entry.revision.0);
        if let Some(time) = &time {
            label.push_str(&format!(" at {time}"));
        }
        ui.horizontal(|ui| {
            ui.label(small(
                format!("r{}", entry.revision.0),
                palette().TEXT_MUTED,
            ));
            if let Some(time) = &time {
                ui.label(small("·", palette().TEXT_MUTED));
                ui.label(small(time, palette().TEXT_MUTED));
            }
            ui.label(small("·", palette().TEXT_MUTED));
            let (cause_icon, cause) = match change.cause {
                PlanChangeCause::ModelUpdate => (icon, kind),
                PlanChangeCause::RuntimeInterrupt => {
                    (icons::WARNING_CIRCLE, "Interrupted by runtime")
                }
            };
            labeled_icon(ui, cause_icon, palette().TEXT_MUTED, cause);
            if let Some(new) = new_status(item) {
                let old = task.and_then(|task| previous_status(entries, index, task));
                ui.label(small("·", palette().TEXT_MUTED));
                if let Some(old) = old {
                    status_icon(ui, old);
                    ui.label(small("→", palette().TEXT_MUTED));
                    label.push_str(&format!(
                        ", {} to {}",
                        status_style(old).1,
                        status_style(new).1
                    ));
                } else {
                    label.push_str(&format!(", {}", status_style(new).1));
                }
                status_icon(ui, new);
            }
            if let Some(title) = title {
                ui.add(egui::Label::new(small(title, palette().TEXT)).truncate());
                label.push_str(&format!(", {title}"));
            }
        })
        .response
        .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &label));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn progress(
        completed: u32,
        in_progress: u32,
        blocked: u32,
        interrupted: u32,
        leaves: u32,
    ) -> PlanProgress {
        PlanProgress {
            node_count: leaves,
            leaf_count: leaves,
            leaf_counts: PlanStatusValues {
                pending: leaves - completed - in_progress - blocked - interrupted,
                in_progress,
                completed,
                blocked,
                interrupted,
            },
            weighted_ppm: PlanStatusValues {
                completed: 400_000,
                ..PlanStatusValues::default()
            },
        }
    }

    #[test]
    fn band_label_counts_only_completed_blocked_and_interrupted() {
        let summary = PlanSummary {
            thread: workbench_protocol::ids::ThreadId::new("t").expect("id"),
            plan_id: Some("p".into()),
            revision: workbench_protocol::wire::Revision(3),
            progress: progress(3, 2, 1, 1, 8),
            updated_at_unix_ms: None,
        };
        assert_eq!(
            band_label(Some(&summary)),
            "Plan 40% complete, 3 of 8 tasks completed, 1 blocked, 1 interrupted"
        );
    }

    #[test]
    fn an_empty_plan_is_not_planned_rather_than_complete() {
        let summary = PlanSummary {
            thread: workbench_protocol::ids::ThreadId::new("t").expect("id"),
            plan_id: None,
            revision: workbench_protocol::wire::Revision(0),
            progress: PlanProgress::default(),
            updated_at_unix_ms: None,
        };
        assert_eq!(band_label(Some(&summary)), NOT_PLANNED);
        assert_eq!(band_label(None), NOT_PLANNED);
    }

    #[test]
    fn overall_status_prefers_running_work_over_problems() {
        assert_eq!(
            overall_status(&progress(8, 0, 0, 0, 8)),
            PlanTaskStatus::Completed
        );
        assert_eq!(
            overall_status(&progress(3, 1, 1, 1, 8)),
            PlanTaskStatus::InProgress
        );
        assert_eq!(
            overall_status(&progress(3, 0, 1, 1, 8)),
            PlanTaskStatus::Interrupted
        );
        assert_eq!(
            overall_status(&progress(3, 0, 1, 0, 8)),
            PlanTaskStatus::Blocked
        );
        assert_eq!(
            overall_status(&progress(0, 0, 0, 0, 8)),
            PlanTaskStatus::Pending
        );
    }
}
