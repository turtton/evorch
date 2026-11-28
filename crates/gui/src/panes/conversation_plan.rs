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

use crate::model::conversation_plan::{
    PlanState, completed_percent, is_planned, summary_is_cleared,
};
use crate::theme::icons;
use crate::theme::tokens::{
    FONT_SMALL, R_MD, ROW_DENSE, SP_1, SP_2, SP_3, palette, text_style_badge,
};
use crate::theme::widgets::icon_button;

/// What the band above the composer shows. `summary` is `None` until the
/// first read answered, which looks the same as a conversation without a plan.
#[derive(Debug, Clone, Copy)]
pub struct PlanBand<'a> {
    pub summary: Option<&'a PlanSummary>,
}

impl PlanBand<'_> {
    /// Whether the band has a row to draw: a plan with tasks, or one that was
    /// cleared (its history still opens). Until a plan is made there is none.
    pub fn has_row(&self) -> bool {
        self.summary
            .is_some_and(|summary| is_planned(&summary.progress) || summary_is_cleared(summary))
    }
}

/// What the panel asks of its owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanPanelAction {
    Select(String),
    HistoryPage(u32),
}

pub const OPEN_LABEL: &str = "Open plan";
const NOT_PLANNED: &str = "Not planned";
const CLEARED: &str = "Plan cleared";
const BAR_WIDTH: f32 = 120.0;
const MINI_BAR_WIDTH: f32 = 44.0;
const BAR_HEIGHT: f32 = 6.0;
const PANEL_BAR_HEIGHT: f32 = 10.0;
/// Room the summary row keeps beside its bar for the percentage and count.
const PANEL_BAR_TAIL: f32 = 96.0;
/// The legend's five values need about this much on one line; narrower than
/// that plus the meta, the meta moves into the bar's tooltip.
const LEGEND_MIN_WIDTH: f32 = 260.0;
/// From this width the tree and the selected task sit side by side.
const SPLIT_MIN_WIDTH: f32 = 600.0;
/// The tree's share of a side-by-side panel (1.15 : 1, as in the design).
const TREE_SHARE: f32 = 1.15 / 2.15;
/// The tree's share of the height when stacked above the selected task, so
/// the task's text stays readable in a short region. The tree keeps at least
/// two rows, but never more than [`STACKED_TREE_MAX_SHARE`] of a short panel.
const STACKED_TREE_SHARE: f32 = 0.3;
const STACKED_TREE_MAX_SHARE: f32 = 0.4;
/// Without a selected task the tree is the content and the history follows.
const STACKED_UNSELECTED_TREE_SHARE: f32 = 0.6;
const EMPTY_ICON_SIZE: f32 = 28.0;

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

/// Order the legend lists states in.
const LEGEND_ORDER: [PlanTaskStatus; 5] = [
    PlanTaskStatus::Completed,
    PlanTaskStatus::InProgress,
    PlanTaskStatus::Interrupted,
    PlanTaskStatus::Pending,
    PlanTaskStatus::Blocked,
];

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

/// A task's state; a parent has none of its own and shows the state that
/// best describes its children's shares, in [`overall_status`]'s order.
pub const fn node_status(status: Option<PlanTaskStatus>, ppm: &PlanStatusValues) -> PlanTaskStatus {
    if let Some(status) = status {
        return status;
    }
    if ppm.completed > 0
        && ppm.pending == 0
        && ppm.in_progress == 0
        && ppm.blocked == 0
        && ppm.interrupted == 0
    {
        PlanTaskStatus::Completed
    } else if ppm.in_progress > 0 {
        PlanTaskStatus::InProgress
    } else if ppm.interrupted > 0 {
        PlanTaskStatus::Interrupted
    } else if ppm.blocked > 0 {
        PlanTaskStatus::Blocked
    } else {
        PlanTaskStatus::Pending
    }
}

const fn percent(ppm: u32) -> u32 {
    if ppm > 1_000_000 { 100 } else { ppm / 10_000 }
}

/// The band's sentence for screen readers and its hover text.
pub fn band_label(summary: Option<&PlanSummary>) -> String {
    if summary.is_some_and(summary_is_cleared) {
        return CLEARED.to_owned();
    }
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
fn progress_bar(
    ui: &mut Ui,
    size: egui::Vec2,
    shares: Option<&PlanStatusValues>,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter_at(rect);
    let radius = CornerRadius::same((size.y / 2.0) as u8);
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
    if !band.has_row() {
        return false;
    }
    let progress = band.summary.map(|s| &s.progress).filter(|p| is_planned(p));
    let label = band_label(band.summary);
    let mut open = false;
    let row = ui.horizontal(|ui| {
        ui.set_min_height(ROW_DENSE - SP_1);
        let Some(progress) = progress else {
            // A plan whose tasks were all deleted still has history to open.
            labeled_icon(ui, icons::BROOM, palette().TEXT_MUTED, CLEARED);
            progress_bar(ui, egui::vec2(BAR_WIDTH, BAR_HEIGHT), None);
            ui.label(small(CLEARED, palette().TEXT_MUTED));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                open = icon_button(ui, icons::ARROW_SQUARE_OUT, OPEN_LABEL).clicked();
            });
            return;
        };
        status_icon(ui, overall_status(progress));
        progress_bar(
            ui,
            egui::vec2(BAR_WIDTH, BAR_HEIGHT),
            Some(&progress.weighted_ppm),
        )
        .on_hover_text(&label);
        ui.label(small(
            format!("{}%", completed_percent(progress)),
            palette().TEXT,
        ));
        completed_count(ui, progress);
        let leaves = &progress.leaf_counts;
        for status in [PlanTaskStatus::Blocked, PlanTaskStatus::Interrupted] {
            let count = value(leaves, status);
            if count > 0 {
                let (icon, name, color) = status_style(status);
                counted(
                    ui,
                    icon,
                    color,
                    &count.to_string(),
                    &format!("{name}: {count}"),
                );
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

fn completed_count(ui: &mut Ui, progress: &PlanProgress) {
    let (icon, _, color) = status_style(PlanTaskStatus::Completed);
    let count = format!("{}/{}", progress.leaf_counts.completed, progress.leaf_count);
    counted(
        ui,
        icon,
        color,
        &count,
        &format!("Completed tasks: {count}"),
    );
}

/// An icon and its value as one unwrapped label. Items are set apart by the
/// layout's spacing, not by a separator glyph, so a wrapped line never starts
/// with one.
fn counted(ui: &mut Ui, icon: &str, color: Color32, count: &str, label: &str) {
    let font = egui::FontId::proportional(FONT_SMALL);
    let style = |color| egui::TextFormat::simple(font.clone(), color);
    let mut job = egui::text::LayoutJob::default();
    job.append(icon, 0.0, style(color));
    job.append(count, 2.0, style(palette().TEXT));
    let response = ui.add(egui::Label::new(job).wrap_mode(egui::TextWrapMode::Extend));
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, label));
    let _ = response.on_hover_text(label);
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
            let planned = progress.filter(|p| is_planned(p));
            summary(ui, state, planned);
            if let Some(error) = state.error() {
                ui.label(small(error, palette().ERROR_FG));
            }
            ui.separator();
            let Some(tree) = state.tree().filter(|tree| !tree.nodes.is_empty()) else {
                if planned.is_none() {
                    if state.is_cleared() {
                        // No task, but a history to look back on.
                        let mut page = None;
                        egui::ScrollArea::vertical()
                            .id_salt("plan-detail")
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                cleared_notice(ui);
                                page = history(ui, state, &[]);
                            });
                        if let Some(page) = page {
                            action = Some(PlanPanelAction::HistoryPage(page));
                        }
                    } else {
                        not_planned(ui);
                    }
                }
                return;
            };
            let mut tree_rows = |ui: &mut Ui| {
                egui::ScrollArea::vertical()
                    .id_salt("plan-tree")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for node in &tree.nodes {
                            if node_row(ui, node, state.selected() == Some(node.task_id.as_str())) {
                                action = Some(PlanPanelAction::Select(node.task_id.clone()));
                            }
                        }
                    });
            };
            let mut page = None;
            let mut selected = |ui: &mut Ui| {
                egui::ScrollArea::vertical()
                    .id_salt("plan-detail")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if let Some(task) = state.task() {
                            detail(ui, &task.node);
                        }
                        page = history(ui, state, &tree.nodes);
                    });
            };
            let width = ui.available_width();
            let height = ui.available_height();
            if width >= SPLIT_MIN_WIDTH {
                ui.horizontal_top(|ui| {
                    let layout = egui::Layout::top_down(egui::Align::Min);
                    let tree_width = (width - SP_3) * TREE_SHARE;
                    ui.allocate_ui_with_layout(egui::vec2(tree_width, height), layout, |ui| {
                        ui.set_min_height(height);
                        tree_rows(ui);
                    });
                    ui.separator();
                    let rest = egui::vec2(ui.available_width(), height);
                    ui.allocate_ui_with_layout(rest, layout, |ui| {
                        ui.set_min_height(height);
                        selected(ui);
                    });
                });
            } else {
                let tree_height = if state.task().is_some() {
                    (height * STACKED_TREE_SHARE)
                        .max(ROW_DENSE * 2.0)
                        .min(height * STACKED_TREE_MAX_SHARE)
                } else {
                    height * STACKED_UNSELECTED_TREE_SHARE
                };
                let layout = egui::Layout::top_down(egui::Align::Min);
                ui.allocate_ui_with_layout(egui::vec2(width, tree_height), layout, |ui| {
                    ui.set_max_height(tree_height);
                    tree_rows(ui);
                });
                ui.separator();
                selected(ui);
            }
            if let Some(page) = page {
                action = Some(PlanPanelAction::HistoryPage(page));
            }
        });
    action
}

/// The line above the history of a plan whose tasks were all deleted.
fn cleared_notice(ui: &mut Ui) {
    ui.horizontal(|ui| {
        labeled_icon(ui, icons::BROOM, palette().TEXT_MUTED, CLEARED);
        ui.label(small(CLEARED, palette().TEXT_MUTED));
    });
}

/// The empty panel of a conversation without a plan.
fn not_planned(ui: &mut Ui) {
    ui.vertical_centered(|ui| {
        ui.add_space((ui.available_height() * 0.3).max(SP_3));
        let response = ui.label(
            RichText::new(icons::LIST_CHECKS)
                .size(EMPTY_ICON_SIZE)
                .color(palette().TEXT_MUTED),
        );
        response
            .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, NOT_PLANNED));
        let _ = response.on_hover_text(NOT_PLANNED);
        ui.label(small(NOT_PLANNED, palette().TEXT_MUTED));
    });
}

/// `r12 · 14:32 · run-7`: the revision shown, its time and the latest run.
fn meta_text(state: &PlanState) -> Option<String> {
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
    (!meta.is_empty()).then(|| meta.join(" · "))
}

/// The summary: the bar with its share and count, then the legend of each
/// state's share of the weight with the meta at its right end. The meta gives
/// way to the legend when the panel is narrow and stays in the bar's tooltip.
fn summary(ui: &mut Ui, state: &PlanState, planned: Option<&PlanProgress>) {
    let meta = meta_text(state).map(|text| format!("Revision and last run: {text}"));
    let Some(progress) = planned else {
        let response = progress_bar(ui, egui::vec2(BAR_WIDTH, PANEL_BAR_HEIGHT), None);
        let _ = response.on_hover_text(band_label(state.summary()));
        return;
    };
    let band = band_label(state.summary());
    ui.horizontal(|ui| {
        let width = (ui.available_width() - PANEL_BAR_TAIL).max(BAR_WIDTH);
        let bar = progress_bar(
            ui,
            egui::vec2(width, PANEL_BAR_HEIGHT),
            Some(&progress.weighted_ppm),
        );
        let hover = match &meta {
            Some(meta) => format!("{band}\n{meta}"),
            None => band.clone(),
        };
        let _ = bar.on_hover_text(hover);
        ui.label(RichText::new(format!("{}%", completed_percent(progress))).strong());
        completed_count(ui, progress);
    });
    let meta = meta.and_then(|label| {
        let text = RichText::new(format!(
            "{} {}",
            icons::CLOCK_COUNTER_CLOCKWISE,
            label.trim_start_matches("Revision and last run: ")
        ))
        .text_style(text_style_badge())
        .color(palette().TEXT_MUTED);
        let galley = egui::WidgetText::from(text).into_galley(
            ui,
            Some(egui::TextWrapMode::Extend),
            f32::INFINITY,
            egui::FontSelection::Default,
        );
        (ui.available_width() - galley.size().x - SP_3 >= LEGEND_MIN_WIDTH)
            .then_some((galley, label))
    });
    ui.horizontal_top(|ui| {
        let legend_width = ui.available_width()
            - meta
                .as_ref()
                .map_or(0.0, |(galley, _)| galley.size().x + SP_3);
        let wrapping = egui::Layout::left_to_right(egui::Align::Center).with_main_wrap(true);
        ui.allocate_ui_with_layout(egui::vec2(legend_width, 0.0), wrapping, |ui| {
            ui.spacing_mut().item_spacing.x = SP_3;
            for status in LEGEND_ORDER {
                let (icon, name, color) = status_style(status);
                let share = format!("{}%", percent(value(&progress.weighted_ppm, status)));
                counted(ui, icon, color, &share, &format!("{name} weight: {share}"));
            }
        });
        if let Some((galley, label)) = meta {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                let response = ui.label(galley);
                response.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &label)
                });
                let _ = response.on_hover_text(label);
            });
        }
    });
}

fn weight_label(ui: &mut Ui, weight: u8) {
    let response = ui.label(small(
        format!("{} {weight}", icons::SCALES),
        palette().TEXT_MUTED,
    ));
    let label = format!("Weight {weight}");
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &label));
    let _ = response.on_hover_text(label);
}

/// A parent's mini bar and completed share.
fn parent_share(ui: &mut Ui, ppm: &PlanStatusValues) {
    ui.label(small(
        format!("{}%", percent(ppm.completed)),
        palette().TEXT_MUTED,
    ));
    progress_bar(ui, egui::vec2(MINI_BAR_WIDTH, BAR_HEIGHT), Some(ppm));
}

/// A tree row; returns whether it was clicked. The selected row scrolls into
/// view once each time the selection changes.
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
                status_icon(ui, node_status(node.status, &node.progress_ppm));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if node.status.is_none() {
                        parent_share(ui, &node.progress_ppm);
                    }
                    weight_label(ui, node.weight);
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        let title = RichText::new(&node.title);
                        let title = if node.status.is_none() {
                            title.strong()
                        } else {
                            title
                        };
                        ui.add(egui::Label::new(title).truncate());
                    });
                });
            });
        });
    let rect = inner.response.rect;
    if selected {
        let edge = egui::Rect::from_min_size(rect.min, egui::vec2(2.0, rect.height()));
        ui.painter().rect_filled(edge, 1.0, palette().ACCENT);
        let shown = ui.id().with("plan-selection-shown");
        let changed = ui.data_mut(|data| {
            let changed = data.get_temp::<String>(shown).as_deref() != Some(&node.task_id);
            data.insert_temp(shown, node.task_id.clone());
            changed
        });
        if changed {
            ui.scroll_to_rect(rect, None);
        }
    }
    let response = ui.interact(
        rect,
        ui.id().with(("plan-node", &node.task_id)),
        Sense::click(),
    );
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, &label)
    });
    response.clicked()
}

/// A tree row's accessible name: its title, state, a parent's completion
/// share, and its weight.
pub fn node_label(node: &PlanNodeView) -> String {
    let state = status_style(node_status(node.status, &node.progress_ppm)).1;
    match node.status {
        Some(_) => format!("{}, {state}, weight {}", node.title, node.weight),
        None => format!(
            "{}, {state}, {}% complete, weight {}",
            node.title,
            percent(node.progress_ppm.completed),
            node.weight
        ),
    }
}

fn section(ui: &mut Ui, icon: &str, name: &str, color: Color32, body: &str) {
    ui.horizontal_top(|ui| {
        labeled_icon(ui, icon, color, name);
        ui.add(egui::Label::new(body).wrap());
    });
}

/// The selected task: its title row, then what blocks it, its description and
/// its result.
fn detail(ui: &mut Ui, node: &workbench_protocol::namespaces::execution::plan::PlanNodeDetail) {
    ui.horizontal(|ui| {
        status_icon(ui, node_status(node.status, &node.progress_ppm));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            weight_label(ui, node.weight);
            if node.status.is_none() {
                parent_share(ui, &node.progress_ppm);
            }
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.add(egui::Label::new(RichText::new(&node.title).strong()).wrap());
            });
        });
    });
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

/// A task's title for the history: the current tree's, else the latest one
/// this page's changes gave it. `None` when neither knows the task.
fn task_title<'a>(
    entries: &'a [PlanHistoryEntry],
    nodes: &'a [PlanNodeView],
    task: &str,
) -> Option<&'a str> {
    if let Some(node) = nodes.iter().find(|node| node.task_id == task) {
        return Some(node.title.as_str());
    }
    entries.iter().find_map(|entry| {
        entry
            .change
            .as_ref()?
            .changes
            .iter()
            .rev()
            .find_map(|change| match change {
                PlanChange::Added { task_id, title, .. } if task_id == task => Some(title.as_str()),
                PlanChange::Updated {
                    task_id,
                    title: Some(title),
                    ..
                } if task_id == task => Some(title.as_str()),
                _ => None,
            })
    })
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
            response
                .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &hover));
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
        // The task may be gone from the tree (deleted since): name it by the
        // title this history saved, and by its id when no entry on the page has one.
        let title = task.map(|task| task_title(entries, nodes, task));
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
            if let (Some(title), Some(task)) = (title, task) {
                let shown = title.unwrap_or(task);
                ui.add(egui::Label::new(small(shown, palette().TEXT)).truncate())
                    .on_hover_text(format!("{shown} ({task})"));
                label.push_str(&format!(", {shown}"));
                if title.is_some() {
                    label.push_str(&format!(", task {task}"));
                }
            }
            if let PlanChange::SubtreeRemoved { task_ids, .. } = item {
                let more = task_ids.len().saturating_sub(1);
                if more > 0 {
                    let text = format!("+{more} subtasks");
                    ui.label(small(&text, palette().TEXT_MUTED));
                    label.push_str(&format!(", {text}"));
                }
            }
            if let PlanChange::Cleared { removed } = item {
                let text = format!("{removed} tasks");
                ui.label(small(&text, palette().TEXT_MUTED));
                label.push_str(&format!(", {text}"));
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

#[cfg(test)]
mod render_tests;
