use std::collections::{BTreeMap, BTreeSet};

use egui::{Align, Layout, Rect, Sense, Stroke, Ui};
use workspace_ui::{
    LineageKind, ProjectId, ProjectRecord, SidebarState, ThreadId, ThreadRecord, ThreadRunPhase,
    ThreadState,
};

use crate::theme::icons;
use crate::theme::text::muted;
use crate::theme::tokens::{FONT_ICON, FONT_SMALL, ROW_DENSE, SP_1, SP_2, palette};
use crate::theme::widgets::{compact_row, ghost, icon_text, row_title};

use super::SidebarAction;

/// Side of the square pin/archive action buttons.
const ACTION_SIZE: f32 = ROW_DENSE - SP_1;

/// Top-level families a project shows before, and per, "See more".
const FAMILY_PAGE: usize = 5;

/// Horizontal step per tree level; also the width of the caret and kind slots.
const INDENT: f32 = 16.0;

/// Extra space separating a family with children from its neighbours.
const FAMILY_GAP: f32 = SP_1;

/// A root's family, including every rewound version and their forks.
pub(crate) fn thread_family(threads: &[ThreadRecord], root: &ThreadId) -> BTreeSet<ThreadId> {
    let root = workspace_ui::version_root(threads, root);
    let mut family = BTreeSet::from([root.clone()]);
    let mut pending = vec![root];
    while let Some(parent) = pending.pop() {
        for child in threads
            .iter()
            .filter(|thread| thread.parent_thread_id.as_ref() == Some(&parent))
        {
            if family.insert(child.id.clone()) {
                pending.push(child.id.clone());
            }
        }
    }
    family
}

pub(crate) fn family_has_running_runs(
    threads: &[ThreadRecord],
    family: &BTreeSet<ThreadId>,
    phases: &BTreeMap<String, ThreadRunPhase>,
) -> bool {
    // Display aggregation prioritizes Stopped/Error over Running. Archive
    // safety instead depends on every raw run phase, including descendants.
    threads.iter().any(|thread| {
        family.contains(&thread.id)
            && thread.run_ids.iter().any(|run_id| {
                matches!(
                    phases.get(run_id),
                    Some(ThreadRunPhase::Running | ThreadRunPhase::Pending)
                )
            })
    })
}

pub(super) fn render(
    ui: &mut Ui,
    sidebar: &SidebarState,
    project: &ProjectRecord,
    indicators: &ThreadIndicators<'_>,
    action: &mut Option<SidebarAction>,
) {
    let (project_threads, archived) =
        ThreadRecord::partition_for_project(&sidebar.threads, &project.id);

    if project_threads.is_empty() {
        ui.horizontal(|ui| {
            ui.add_space(SP_2 + 16.0);
            ui.label(crate::theme::text::muted("No threads yet"));
        });
    }

    let limit_id = family_limit_id(&project.id);
    let limit = ui
        .ctx()
        .data(|data| data.get_temp::<usize>(limit_id))
        .unwrap_or(FAMILY_PAGE);
    let rows = nested_threads(&project_threads, &sidebar.threads);
    let (rows, hidden) = limit_families(&rows, limit, |thread| {
        thread.pinned
            || sidebar.active_thread.as_ref() == Some(&thread.id)
            || indicators.question_threads.contains(&thread.id)
            || indicators.approval_threads.contains(&thread.id)
            || indicators.unread_threads.contains(&thread.id)
    });
    render_tree(ui, sidebar, &rows, indicators, false, action);
    if hidden > 0 {
        ui.indent(("more-threads-indent", &project.id), |ui| {
            if ui
                .add(ghost(muted(format!("See more ({hidden})"))))
                .on_hover_text(format!("Show up to {FAMILY_PAGE} more threads"))
                .clicked()
            {
                ui.ctx()
                    .data_mut(|data| data.insert_temp(limit_id, limit + FAMILY_PAGE));
            }
        });
    }

    if !archived.is_empty() {
        // Nested under the project row, level with its thread titles.
        ui.indent(("archived-threads-indent", &project.id), |ui| {
            egui::CollapsingHeader::new(format!("Archived ({})", archived.len()))
                .id_salt(("archived-threads", &project.id))
                .show(ui, |ui| {
                    let rows = nested_threads(&archived, &sidebar.threads);
                    render_tree(ui, sidebar, &rows, indicators, true, action);
                });
        });
    }
}

pub(super) struct ThreadIndicators<'a> {
    pub phases: &'a BTreeMap<String, ThreadRunPhase>,
    pub question_threads: &'a BTreeSet<workspace_ui::ThreadId>,
    /// Threads with a tool call, including a subagent's, awaiting approval.
    pub approval_threads: &'a BTreeSet<workspace_ui::ThreadId>,
    /// Threads whose finished work has not been opened since it finished.
    pub unread_threads: &'a BTreeSet<workspace_ui::ThreadId>,
}

#[derive(Clone, Copy, Default)]
struct Attention {
    question: bool,
    approval: bool,
    unread: bool,
}

fn render_tree(
    ui: &mut Ui,
    sidebar: &SidebarState,
    rows: &[(&ThreadRecord, usize)],
    indicators: &ThreadIndicators<'_>,
    archived: bool,
    action: &mut Option<SidebarAction>,
) {
    let rails = tree_rails(rows);
    // Rails bridge the vertical gap between rows so a branch reads as one line.
    let row_gap = ui.spacing().item_spacing.y;
    let mut collapsed_depth = None;
    for (index, (thread, depth)) in rows.iter().copied().enumerate() {
        if collapsed_depth.is_some_and(|hidden_depth| depth > hidden_depth) {
            continue;
        }
        collapsed_depth = None;
        let has_children = rows.get(index + 1).is_some_and(|(_, next)| *next > depth);
        if family_gap_before(rows, index) {
            ui.add_space(FAMILY_GAP);
        }
        // This ID deliberately excludes the active/archive section and parent
        // widget, so moving a family preserves each branch's expansion state.
        let id = egui::Id::new(("sidebar-thread-children", &thread.project_id, &thread.id));
        let mut expansion =
            egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, true);
        let active = sidebar.active_thread.as_ref() == Some(&thread.id);
        let display_root = workspace_ui::display_parent(&sidebar.threads, &thread.id).is_none();
        let kind = branch_kind(&sidebar.threads, thread).filter(|_| !display_root);
        ui.push_id(&thread.id, |ui| {
            compact_row(ui, active, |ui| {
                ui.spacing_mut().item_spacing.x = SP_1;
                ui.spacing_mut().button_padding.x = SP_1;
                ui.add_space(depth as f32 * INDENT);
                let (rect, toggle) = ui.allocate_exact_size(
                    egui::vec2(INDENT, ROW_DENSE),
                    if has_children {
                        Sense::click()
                    } else {
                        Sense::hover()
                    },
                );
                paint_rails(ui, rect, &rails[index], row_gap);
                if has_children {
                    if toggle.clicked() {
                        expansion.toggle(ui);
                    }
                    let label = if expansion.is_open() {
                        "Collapse"
                    } else {
                        "Expand"
                    };
                    toggle.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            egui::WidgetType::Button,
                            true,
                            format!("{label} children of {}", thread.id),
                        )
                    });
                    let caret = if expansion.is_open() {
                        icons::CARET_DOWN
                    } else {
                        icons::CARET_RIGHT
                    };
                    let color = if toggle.hovered() {
                        palette().TEXT
                    } else {
                        palette().TEXT_MUTED
                    };
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        caret,
                        egui::FontId::proportional(FONT_SMALL),
                        color,
                    );
                    if expansion.is_open() {
                        // Starts the children's rail just below the caret.
                        ui.painter().vline(
                            ui.painter().round_to_pixel_center(rect.center().x),
                            rect.center().y + FONT_SMALL / 2.0..=rect.bottom() + row_gap / 2.0,
                            rail_stroke(),
                        );
                    }
                }
                if let Some(kind) = kind {
                    // A leaf reuses its empty caret slot; a parent needs its own.
                    let (slot, response) = if has_children {
                        ui.allocate_exact_size(egui::vec2(INDENT, ROW_DENSE), Sense::hover())
                    } else {
                        (rect, toggle)
                    };
                    branch_kind_icon(ui, slot, response, kind);
                }
                if archived {
                    archived_row(ui, thread, display_root, action);
                } else {
                    let state = thread.state(indicators.phases);
                    let family_running = display_root
                        && family_has_running_runs(
                            &sidebar.threads,
                            &thread_family(&sidebar.threads, &thread.id),
                            indicators.phases,
                        );
                    active_row(
                        ui,
                        (thread, display_root),
                        state,
                        Attention {
                            question: indicators.question_threads.contains(&thread.id),
                            approval: indicators.approval_threads.contains(&thread.id),
                            unread: indicators.unread_threads.contains(&thread.id),
                        },
                        family_running,
                        action,
                    );
                }
            });
        });
        expansion.store(ui.ctx());
        if has_children && !expansion.is_open() {
            collapsed_depth = Some(depth);
        }
        if !archived
            && let Some(root) = &thread.active_root
            && sidebar
                .projects
                .iter()
                .find(|project| project.id == thread.project_id)
                .is_some_and(|project| project.repo_root != *root)
        {
            ui.label(crate::theme::text::muted(format!(
                "Shared workspace: {}",
                root.display()
            )));
        }
    }
}

/// Why a thread hangs under its parent in the tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BranchKind {
    /// A worker run escalated its task to this orchestrator conversation.
    Escalation,
    /// The user forked the parent conversation.
    Fork,
}

/// A rewind replaces its source in place, so the kind comes from the original
/// conversation of the thread's version group.
fn branch_kind(threads: &[ThreadRecord], thread: &ThreadRecord) -> Option<BranchKind> {
    let original_id = workspace_ui::version_root(threads, &thread.id);
    let original = threads
        .iter()
        .find(|candidate| candidate.id == original_id)
        .unwrap_or(thread);
    if original.escalation_source_run_id.is_some() {
        Some(BranchKind::Escalation)
    } else if original
        .lineage
        .as_ref()
        .is_some_and(|lineage| lineage.kind == LineageKind::Fork)
    {
        Some(BranchKind::Fork)
    } else {
        None
    }
}

fn branch_kind_icon(ui: &Ui, slot: Rect, response: egui::Response, kind: BranchKind) {
    let (icon, label, hint) = match kind {
        BranchKind::Escalation => (
            icons::ARROW_FAT_LINE_UP,
            "Escalated thread",
            "Escalated: a worker run handed its task to this orchestrator thread",
        ),
        BranchKind::Fork => (
            icons::GIT_FORK,
            "Forked thread",
            "Forked from its parent thread",
        ),
    };
    ui.painter().text(
        slot.center(),
        egui::Align2::CENTER_CENTER,
        icon,
        egui::FontId::proportional(FONT_SMALL),
        palette().TEXT_MUTED,
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, label));
    response.on_hover_text(hint);
}

/// A family with children is set apart from the rows around it; childless
/// roots stay packed together.
fn family_gap_before(rows: &[(&ThreadRecord, usize)], index: usize) -> bool {
    let starts_family = rows.get(index).is_some_and(|(_, depth)| *depth == 0);
    let has_children = rows.get(index + 1).is_some_and(|(_, depth)| *depth > 0);
    let follows_children = index
        .checked_sub(1)
        .and_then(|previous| rows.get(previous))
        .is_some_and(|(_, depth)| *depth > 0);
    starts_family && index > 0 && (has_children || follows_children)
}

/// Tree guides per row: entry `level - 1` tells whether the branch at `level`
/// (1..=depth) continues below the row, i.e. a later row at that level follows
/// before the tree climbs above it.
fn tree_rails(rows: &[(&ThreadRecord, usize)]) -> Vec<Vec<bool>> {
    rows.iter()
        .enumerate()
        .map(|(index, (_, depth))| {
            (1..=*depth)
                .map(|level| {
                    rows[index + 1..]
                        .iter()
                        .map(|(_, next)| *next)
                        .find(|next| *next <= level)
                        == Some(level)
                })
                .collect()
        })
        .collect()
}

fn rail_stroke() -> Stroke {
    // BORDER nearly vanishes on the black sidebar; a dimmed muted text tone
    // stays visible in both palettes without competing with titles.
    Stroke::new(1.0, palette().TEXT_MUTED.gamma_multiply(0.45))
}

/// Paints the rails left of `slot` (the row's caret slot): pass-through lines
/// for ancestor branches that continue, and an elbow joining this row to its
/// parent's rail.
fn paint_rails(ui: &Ui, slot: Rect, continues: &[bool], row_gap: f32) {
    let Some((own, ancestors)) = continues.split_last() else {
        return;
    };
    let painter = ui.painter();
    let stroke = rail_stroke();
    let depth = continues.len();
    let top = slot.top() - row_gap / 2.0;
    let bottom = slot.bottom() + row_gap / 2.0;
    let middle = painter.round_to_pixel_center(slot.center().y);
    let rail_x = |level: usize| {
        painter.round_to_pixel_center(slot.left() - (depth - level) as f32 * INDENT - INDENT / 2.0)
    };
    for (index, continues) in ancestors.iter().enumerate() {
        if *continues {
            painter.vline(rail_x(index + 1), top..=bottom, stroke);
        }
    }
    let x = rail_x(depth);
    painter.vline(x, top..=if *own { bottom } else { middle }, stroke);
    painter.hline(x..=slot.left(), middle, stroke);
}

fn active_row(
    ui: &mut Ui,
    (thread, display_root): (&ThreadRecord, bool),
    state: ThreadState,
    attention: Attention,
    family_running: bool,
    action: &mut Option<SidebarAction>,
) {
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        // Runtime/question indicators stay at the trailing edge.
        ui.spacing_mut().item_spacing.x = SP_1;
        thread_status_icon(ui, state, attention);
        ui.add_space(SP_1);
        ui.spacing_mut().item_spacing.x = 0.0;
        // The slot before Pin holds Archive, or the running spinner in its
        // place, so Pin stays put when a run starts or ends. A running family
        // cannot be archived, so the two never compete for the slot.
        if display_root && !family_running {
            let can_archive = !thread.pinned;
            let archive = ui
                .add_enabled(
                    can_archive,
                    ghost(icon_text(icons::ARCHIVE)).min_size(egui::vec2(ACTION_SIZE, ACTION_SIZE)),
                )
                .on_hover_text("Archive")
                .on_disabled_hover_text("Unpin this thread before archiving");
            archive.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::Button, can_archive, "Archive")
            });
            if archive.clicked() {
                *action = Some(SidebarAction::ToggleArchive(thread.id.clone()));
            }
        } else if matches!(state, ThreadState::Running) {
            running_spinner(ui);
        } else {
            ui.add_space(ACTION_SIZE);
        }
        let (pin_icon, pin_label, pin_hint) = if thread.pinned {
            (
                icons::filled(ui, icons::PUSH_PIN)
                    .size(FONT_ICON)
                    .color(palette().TEXT),
                "★",
                "Unpin",
            )
        } else {
            (
                icon_text(icons::PUSH_PIN).color(palette().TEXT_MUTED),
                "☆",
                "Pin",
            )
        };
        let pin = ui.add(ghost(pin_icon).min_size(egui::vec2(ACTION_SIZE, ACTION_SIZE)));
        pin.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, pin_label));
        if pin.on_hover_text(pin_hint).clicked() {
            *action = Some(SidebarAction::TogglePin(thread.id.clone()));
        }
        ui.add_space(SP_1);
        thread_title(ui, thread, !display_root, action);
    });
}

fn archived_row(
    ui: &mut Ui,
    thread: &ThreadRecord,
    display_root: bool,
    action: &mut Option<SidebarAction>,
) {
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = SP_1;
        if display_root && restore_button(ui).clicked() {
            *action = Some(SidebarAction::ToggleArchive(thread.id.clone()));
        }
        thread_title(ui, thread, !display_root, action);
    });
}

fn restore_button(ui: &mut Ui) -> egui::Response {
    let response = ui.add(
        ghost(icon_text(icons::ARROW_U_UP_LEFT)).min_size(egui::vec2(ACTION_SIZE, ACTION_SIZE)),
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Restore"));
    response.on_hover_text("Restore from archive")
}

fn thread_title(
    ui: &mut Ui,
    thread: &ThreadRecord,
    child: bool,
    action: &mut Option<SidebarAction>,
) {
    // Indentation and the caret already show nesting; "↳" stays in the
    // accessible name only.
    let title = row_title(ui, thread.title.as_str());
    if child {
        let label = format!("↳ {}", thread.title);
        title.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &label));
    }
    let title = title.on_hover_text(format!("thread ID: {}", thread.id));
    title.context_menu(|ui| {
        if ui.button("Copy thread ID").clicked() {
            ui.ctx().copy_text(thread.id.to_string());
            ui.close();
        }
        if ui.button("Copy thread title").clicked() {
            ui.ctx().copy_text(thread.title.to_string());
            ui.close();
        }
    });
    if title.clicked() {
        *action = Some(SidebarAction::SwitchThread(thread.id.clone()));
    }
}

/// Keeps the first `limit` top-level families plus any later family with a
/// member matching `keep`; returns the kept rows and the hidden family count.
fn limit_families<'a>(
    rows: &[(&'a ThreadRecord, usize)],
    limit: usize,
    keep: impl Fn(&ThreadRecord) -> bool,
) -> (Vec<(&'a ThreadRecord, usize)>, usize) {
    let mut kept = Vec::with_capacity(rows.len());
    let mut hidden = 0;
    let families = rows.chunk_by(|_, (_, depth)| *depth > 0);
    for (index, family) in families.enumerate() {
        if index < limit || family.iter().any(|(thread, _)| keep(thread)) {
            kept.extend_from_slice(family);
        } else {
            hidden += 1;
        }
    }
    (kept, hidden)
}

pub(super) fn family_limit_id(project: &ProjectId) -> egui::Id {
    egui::Id::new(("sidebar-project-family-limit", project))
}

fn nested_threads<'a>(
    threads: &[&'a ThreadRecord],
    all: &[ThreadRecord],
) -> Vec<(&'a ThreadRecord, usize)> {
    fn append<'a>(
        thread: &'a ThreadRecord,
        depth: usize,
        rows: &[(&'a ThreadRecord, Option<ThreadId>)],
        seen: &mut BTreeSet<workspace_ui::ThreadId>,
        output: &mut Vec<(&'a ThreadRecord, usize)>,
    ) {
        if !seen.insert(thread.id.clone()) {
            return;
        }
        output.push((thread, depth));
        for (child, _) in rows
            .iter()
            .filter(|(_, parent)| parent.as_ref() == Some(&thread.id))
        {
            append(child, depth + 1, rows, seen, output);
        }
    }

    // Rewound versions are hidden; children hang under their parent's visible version.
    let rows: Vec<_> = threads
        .iter()
        .map(|thread| (*thread, workspace_ui::display_parent(all, &thread.id)))
        .collect();
    let mut seen = BTreeSet::new();
    let mut output = Vec::with_capacity(threads.len());
    for (thread, parent) in &rows {
        if parent
            .as_ref()
            .is_none_or(|parent| !threads.iter().any(|candidate| &candidate.id == parent))
        {
            append(thread, 0, &rows, &mut seen, &mut output);
        }
    }
    // Corrupt or cyclic parent links still leave every thread reachable.
    for thread in threads.iter().copied() {
        append(thread, 0, &rows, &mut seen, &mut output);
    }
    output
}

fn thread_status_icon(ui: &mut Ui, state: ThreadState, attention: Attention) {
    // Runtime status retains the display aggregation; questions and approvals
    // are independent and must remain visible alongside either the spinner or
    // the error icon. The running spinner lives in the Archive slot instead
    // (see `running_spinner`); an unread report appears here once the work
    // has finished.
    let unread = attention.unread;
    let status = if matches!(state, ThreadState::Running) {
        None
    } else if matches!(state, ThreadState::Error) {
        let response = ui.add(
            egui::Label::new(
                egui::RichText::new(icons::WARNING)
                    .size(FONT_SMALL + 2.0)
                    .color(palette().ERROR_FG),
            )
            .sense(Sense::hover()),
        );
        Some((response, "Thread status: Failed"))
    } else if unread {
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(FONT_SMALL, FONT_SMALL), Sense::hover());
        ui.painter()
            .circle_filled(rect.center(), SP_1 + 1.0, palette().ACCENT);
        Some((response, "Thread status: Unread report"))
    } else {
        None
    };
    if let Some((response, label)) = status {
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Label, ui.is_enabled(), label)
        });
        response.on_hover_text(label);
    }
    if attention.question {
        attention_dot(ui, "Answer needed: this thread has an unanswered question");
    }
    if attention.approval {
        let label = "Approval needed: a tool call is waiting";
        if attention.question {
            // Two identical dots would be indistinguishable, so an approval
            // beside a question uses its own glyph.
            let response = ui.add(
                egui::Label::new(
                    egui::RichText::new(icons::SHIELD_WARNING)
                        .size(FONT_SMALL + 2.0)
                        .color(palette().WARNING_FG),
                )
                .sense(Sense::hover()),
            );
            response.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::Label, ui.is_enabled(), label)
            });
            response.on_hover_text(label);
        } else {
            attention_dot(ui, label);
        }
    }
}

fn running_spinner(ui: &mut Ui) {
    let label = "Thread status: Running";
    // add_sized reserves the whole Archive slot and centers the spinner in it.
    let response = ui.add_sized(
        egui::vec2(ACTION_SIZE, ACTION_SIZE),
        egui::Spinner::new()
            .size(FONT_SMALL)
            .color(palette().RUNNING),
    );
    response
        .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, ui.is_enabled(), label));
    response.on_hover_text(label);
}

fn attention_dot(ui: &mut Ui, label: &str) {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(FONT_SMALL, FONT_SMALL), Sense::hover());
    ui.painter()
        .circle_filled(rect.center(), SP_1, palette().WARNING_FG);
    response
        .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, ui.is_enabled(), label));
    response.on_hover_text(label);
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use egui_kittest::{Harness, kittest::Queryable};
    use workspace_ui::{
        LineageKind, ProjectId, ThreadId, ThreadLineage, ThreadRecord, ThreadRunPhase, ThreadState,
    };

    use super::{
        Attention, BranchKind, active_row, branch_kind, family_gap_before, family_has_running_runs,
        limit_families, nested_threads, thread_family, tree_rails,
    };
    use crate::panes::sidebar::SidebarAction;

    fn rows_at<'a>(
        threads: &'a [ThreadRecord],
        depths: &[usize],
    ) -> Vec<(&'a ThreadRecord, usize)> {
        threads.iter().zip(depths.iter().copied()).collect()
    }

    fn records(count: usize) -> Vec<ThreadRecord> {
        (0..count)
            .map(|index| {
                let id = format!("t{index}");
                ThreadRecord::new(ThreadId::new(&id), ProjectId::new("demo"), id)
            })
            .collect()
    }

    #[test]
    fn rails_continue_only_while_a_later_sibling_follows() {
        // root / a / a1 / a2 / b / b1 / other
        let threads = records(7);
        let rows = rows_at(&threads, &[0, 1, 2, 2, 1, 2, 0]);
        assert_eq!(
            tree_rails(&rows),
            [
                vec![],
                // a continues to its sibling b.
                vec![true],
                // a1: level 1 passes through toward b; a2 follows at level 2.
                vec![true, true],
                // a2 is a's last child, while level 1 still runs to b.
                vec![true, false],
                // b is the root's last child.
                vec![false],
                // b1 is b's last child, and nothing continues past it.
                vec![false, false],
                vec![],
            ]
        );
    }

    #[test]
    fn family_gap_sets_apart_only_families_with_children() {
        // solo / solo / family(root, child) / solo / solo
        let threads = records(6);
        let rows = rows_at(&threads, &[0, 0, 0, 1, 0, 0]);
        let gaps: Vec<_> = (0..rows.len())
            .map(|index| family_gap_before(&rows, index))
            .collect();
        assert_eq!(gaps, [false, false, true, false, true, false]);
        // A leading family needs no gap above the first row.
        let rows = rows_at(&threads, &[0, 1, 0]);
        assert!(!family_gap_before(&rows, 0));
        assert!(family_gap_before(&rows, 2));
    }

    #[test]
    fn branch_kind_names_escalations_and_forks_through_rewinds() {
        let project = ProjectId::new("demo");
        let root = ThreadRecord::new(ThreadId::new("root"), project.clone(), "Root");
        let mut escalated = ThreadRecord::new(ThreadId::new("esc"), project.clone(), "Esc");
        escalated.parent_thread_id = Some(root.id.clone());
        escalated.escalation_source_run_id = Some("run-worker".into());
        let mut rewound = ThreadRecord::new(ThreadId::new("esc-v2"), project.clone(), "Esc");
        rewound.parent_thread_id = Some(escalated.id.clone());
        rewound.lineage = Some(ThreadLineage {
            kind: LineageKind::Rewind,
            point: None,
        });
        let mut fork = ThreadRecord::new(ThreadId::new("fork"), project.clone(), "Fork");
        fork.parent_thread_id = Some(root.id.clone());
        fork.lineage = Some(ThreadLineage {
            kind: LineageKind::Fork,
            point: None,
        });
        let mut legacy = ThreadRecord::new(ThreadId::new("legacy"), project, "Legacy");
        legacy.parent_thread_id = Some(root.id.clone());
        let threads = [
            root.clone(),
            escalated.clone(),
            rewound.clone(),
            fork.clone(),
            legacy.clone(),
        ];
        let kind = |thread: &ThreadRecord| branch_kind(&threads, thread);
        assert_eq!(kind(&root), None);
        assert_eq!(kind(&escalated), Some(BranchKind::Escalation));
        // A rewind does not copy the escalation source, but replaces it in place.
        assert_eq!(kind(&rewound), Some(BranchKind::Escalation));
        assert_eq!(kind(&fork), Some(BranchKind::Fork));
        assert_eq!(kind(&legacy), None);
    }

    #[test]
    fn thread_rows_keep_status_and_pin_without_fork_controls() {
        for width in [220.0, 600.0] {
            // Given: a thread row rendered at the requested width.
            let thread =
                ThreadRecord::new(ThreadId::new("thread"), ProjectId::new("demo"), "Thread");
            let mut harness = Harness::builder()
                .with_size(egui::vec2(width, 100.0))
                .build_ui_state(
                    |ui, action| {
                        crate::theme::install(ui.ctx());
                        ui.horizontal(|ui| {
                            active_row(
                                ui,
                                (&thread, true),
                                ThreadState::Running,
                                Attention::default(),
                                true,
                                action,
                            );
                        });
                    },
                    None,
                );
            // A spinner continuously requests repaint; advance bounded frames.
            harness.run_steps(2);

            // Then: runtime status and Pin remain available at either width.
            assert!(harness.query_by_label("Fork").is_none());
            assert!(harness.query_by_label("⋯").is_none());
            assert!(harness.query_by_label("Pause").is_none());
            assert!(harness.query_by_label("Resume").is_none());
            assert!(harness.query_by_label("Thread status: Running").is_some());
            harness.get_by_label("☆").click();
            harness.run_steps(2);
            assert_eq!(
                harness.state(),
                &Some(SidebarAction::TogglePin(thread.id.clone()))
            );
        }
    }

    #[test]
    fn archive_guard_uses_raw_phases_instead_of_display_state() {
        let mut thread = ThreadRecord::new(ThreadId::new("thread"), ProjectId::new("p"), "Thread");
        thread.run_ids = vec!["terminal".into(), "active".into()];
        let family = BTreeSet::from([thread.id.clone()]);
        for (terminal, display) in [
            (ThreadRunPhase::Stopped, ThreadState::Stopped),
            (ThreadRunPhase::Error, ThreadState::Error),
        ] {
            for active in [ThreadRunPhase::Running, ThreadRunPhase::Pending] {
                let phases =
                    BTreeMap::from([("terminal".into(), terminal), ("active".into(), active)]);
                assert_eq!(thread.state(&phases), display);
                assert!(family_has_running_runs(
                    std::slice::from_ref(&thread),
                    &family,
                    &phases
                ));
            }
        }
        // Missing phases and runs not owned by this family cannot block it.
        for phase in [
            ThreadRunPhase::Waiting,
            ThreadRunPhase::Done,
            ThreadRunPhase::Stopped,
            ThreadRunPhase::Error,
        ] {
            let phases = BTreeMap::from([
                ("terminal".into(), phase),
                ("unrelated".into(), ThreadRunPhase::Running),
            ]);
            assert!(!family_has_running_runs(
                std::slice::from_ref(&thread),
                &family,
                &phases
            ));
        }
    }

    #[test]
    fn archive_family_walk_includes_descendants_and_handles_cycles() {
        let project = ProjectId::new("p");
        let mut root = ThreadRecord::new(ThreadId::new("root"), project.clone(), "Root");
        let mut child = ThreadRecord::new(ThreadId::new("child"), project.clone(), "Child");
        let mut grandchild =
            ThreadRecord::new(ThreadId::new("grandchild"), project.clone(), "Grandchild");
        child.parent_thread_id = Some(root.id.clone());
        grandchild.parent_thread_id = Some(child.id.clone());
        root.parent_thread_id = Some(grandchild.id.clone());
        grandchild.run_ids.push("active".into());
        let unrelated = ThreadRecord::new(ThreadId::new("other"), project, "Other");
        let root_id = root.id.clone();
        let expected = BTreeSet::from([root.id.clone(), child.id.clone(), grandchild.id.clone()]);
        let threads = [unrelated, grandchild, child, root];
        let family = thread_family(&threads, &root_id);
        assert_eq!(family, expected);
        let phases = BTreeMap::from([("active".into(), ThreadRunPhase::Pending)]);
        assert!(family_has_running_runs(&threads, &family, &phases));
        let other_family = thread_family(&threads, &threads[0].id);
        assert!(!family_has_running_runs(&threads, &other_family, &phases));
    }

    #[test]
    fn escalation_children_follow_parent_at_each_depth() {
        let project = ProjectId::new("demo");
        let parent = ThreadRecord::new(ThreadId::new("parent"), project.clone(), "Parent");
        let mut child = ThreadRecord::new(ThreadId::new("child"), project.clone(), "Child");
        child.parent_thread_id = Some(parent.id.clone());
        child.escalation_source_run_id = Some("run-worker".into());
        let mut grandchild = ThreadRecord::new(ThreadId::new("grandchild"), project, "Grandchild");
        grandchild.parent_thread_id = Some(child.id.clone());
        let all = [grandchild.clone(), child.clone(), parent.clone()];
        let rows = nested_threads(&[&grandchild, &child, &parent], &all);
        assert_eq!(
            rows.iter()
                .map(|(thread, depth)| (thread.id.to_string(), *depth))
                .collect::<Vec<_>>(),
            [
                ("parent".into(), 0),
                ("child".into(), 1),
                ("grandchild".into(), 2)
            ]
        );
    }

    #[test]
    fn family_limit_keeps_whole_families_and_flagged_ones_beyond_the_page() {
        let project = ProjectId::new("demo");
        let threads: Vec<_> = ["a", "a-child", "b", "c", "c-child"]
            .into_iter()
            .map(|id| ThreadRecord::new(ThreadId::new(id), project.clone(), id))
            .collect();
        let rows: Vec<_> = threads.iter().zip([0, 1, 0, 0, 1]).collect();
        let ids = |rows: &[(&ThreadRecord, usize)]| {
            rows.iter()
                .map(|(thread, _)| thread.id.to_string())
                .collect::<Vec<_>>()
        };

        let (kept, hidden) = limit_families(&rows, 1, |_| false);
        assert_eq!(
            (ids(&kept), hidden),
            (vec!["a".into(), "a-child".into()], 2)
        );

        let (kept, hidden) = limit_families(&rows, 1, |thread| thread.id.to_string() == "c-child");
        assert_eq!(
            (ids(&kept), hidden),
            (
                vec!["a".into(), "a-child".into(), "c".into(), "c-child".into()],
                1
            )
        );

        let (kept, hidden) = limit_families(&rows, 5, |_| false);
        assert_eq!((kept.len(), hidden), (rows.len(), 0));
    }
}
