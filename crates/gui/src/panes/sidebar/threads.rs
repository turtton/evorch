use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use egui::{Align, Layout, Sense, Ui};
use workspace_ui::{
    ProjectRecord, SidebarState, ThreadId, ThreadRecord, ThreadRunPhase, ThreadState,
};

use crate::model::telemetry::{TelemetryOverlay, WorkspaceWaitEntry};
use crate::theme::icons;
use crate::theme::text::section;
use crate::theme::tokens::{FONT_ICON, FONT_SMALL, ROW_DENSE, SP_1, SP_2, SP_3, palette};
use crate::theme::widgets::{
    compact_row, empty_state, ghost, ghost_icon_button, icon_button, icon_text, labeled,
    primary_button, row_title,
};

use super::SidebarAction;

/// Side of the square pin/archive/fork action buttons.
const ACTION_SIZE: f32 = ROW_DENSE - SP_1;
/// Below this row width Fork moves into the overflow menu.
const NARROW_ROW_WIDTH: f32 = 240.0;

pub(crate) fn thread_family(threads: &[ThreadRecord], root: &ThreadId) -> BTreeSet<ThreadId> {
    let mut family = BTreeSet::from([root.clone()]);
    let mut pending = vec![root.clone()];
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

pub fn render(
    ui: &mut Ui,
    sidebar: &SidebarState,
    project: &ProjectRecord,
    phases: &BTreeMap<String, ThreadRunPhase>,
    telemetry: &TelemetryOverlay,
    question_threads: &BTreeSet<workspace_ui::ThreadId>,
    action: &mut Option<SidebarAction>,
) {
    let indicators = ThreadIndicators {
        phases,
        telemetry,
        question_threads,
    };
    let (project_threads, archived) =
        ThreadRecord::partition_for_project(&sidebar.threads, &project.id);

    ui.add_space(SP_3);
    ui.horizontal(|ui| {
        ui.set_min_height(ROW_DENSE);
        ui.add_space(SP_2);
        ui.label(section("Threads"));
        let has_threads = !project_threads.is_empty();
        let new_thread_clicked = ui
            .with_layout(Layout::right_to_left(Align::Center), |ui| {
                if has_threads {
                    icon_button(ui, icons::PLUS, "New thread").clicked()
                } else {
                    primary_button(ui, "New thread").clicked()
                }
            })
            .inner;
        if new_thread_clicked {
            let title = format!("thread-{}", sidebar.threads.len() + 1);
            *action = Some(SidebarAction::CreateThread(title));
        }
    });

    if project_threads.is_empty() {
        empty_state(
            ui,
            "No threads yet",
            "Start a thread to begin a conversation.",
            None,
        );
    }

    render_tree(ui, sidebar, &project_threads, &indicators, false, action);

    egui::CollapsingHeader::new(format!("アーカイブ済み ({})", archived.len()))
        .id_salt(("archived-threads", &project.id))
        .show(ui, |ui| {
            render_tree(ui, sidebar, &archived, &indicators, true, action);
        });
    ui.add_space(SP_2);
}

struct ThreadIndicators<'a> {
    phases: &'a BTreeMap<String, ThreadRunPhase>,
    telemetry: &'a TelemetryOverlay,
    question_threads: &'a BTreeSet<workspace_ui::ThreadId>,
}

fn render_tree(
    ui: &mut Ui,
    sidebar: &SidebarState,
    threads: &[&ThreadRecord],
    indicators: &ThreadIndicators<'_>,
    archived: bool,
    action: &mut Option<SidebarAction>,
) {
    let rows = nested_threads(threads);
    let mut collapsed_depth = None;
    for (index, (thread, depth)) in rows.iter().copied().enumerate() {
        if collapsed_depth.is_some_and(|hidden_depth| depth > hidden_depth) {
            continue;
        }
        collapsed_depth = None;
        let has_children = rows.get(index + 1).is_some_and(|(_, next)| *next > depth);
        // This ID deliberately excludes the active/archive section and parent
        // widget, so moving a family preserves each branch's expansion state.
        let id = egui::Id::new(("sidebar-thread-children", &thread.project_id, &thread.id));
        let mut expansion =
            egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, true);
        let active = sidebar.active_thread.as_ref() == Some(&thread.id);
        ui.push_id(&thread.id, |ui| {
            compact_row(ui, active, |ui| {
                ui.spacing_mut().item_spacing.x = SP_1;
                ui.spacing_mut().button_padding.x = SP_1;
                ui.add_space(depth as f32 * 16.0);
                let (rect, toggle) = ui.allocate_exact_size(
                    egui::vec2(16.0, ROW_DENSE),
                    if has_children {
                        Sense::click()
                    } else {
                        Sense::hover()
                    },
                );
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
                }
                if archived {
                    archived_row(ui, thread, sidebar, indicators.telemetry, action);
                } else {
                    let state = thread.state(indicators.phases);
                    let family_running = thread.parent_thread_id.is_none()
                        && family_has_running_runs(
                            &sidebar.threads,
                            &thread_family(&sidebar.threads, &thread.id),
                            indicators.phases,
                        );
                    active_row(
                        ui,
                        thread,
                        state,
                        indicators.question_threads.contains(&thread.id),
                        family_running,
                        (sidebar, indicators.telemetry),
                        action,
                    );
                }
            });
        });
        expansion.store(ui.ctx());
        if has_children && !expansion.is_open() {
            collapsed_depth = Some(depth);
        }
        if !archived && let (Some(branch), Some(worktree)) = (&thread.branch, &thread.worktree_path)
        {
            ui.label(crate::theme::text::muted(format!(
                "{branch} @ {}",
                worktree.display()
            )));
        } else if !archived
            && thread.worktree_path.is_none()
            && let Some(root) = &thread.active_root
            && sidebar
                .projects
                .iter()
                .find(|project| project.id == thread.project_id)
                .is_some_and(|project| project.repo_root != *root)
        {
            // Reconciliation populates this no-worktree root only for Shared mode.
            ui.label(crate::theme::text::muted(format!(
                "Shared workspace: {}",
                root.display()
            )));
        }
    }
}

fn active_row(
    ui: &mut Ui,
    thread: &ThreadRecord,
    state: ThreadState,
    has_question: bool,
    family_running: bool,
    wait_context: (&SidebarState, &TelemetryOverlay),
    action: &mut Option<SidebarAction>,
) {
    // At the minimum window size the action buttons leave little room for the
    // title, so Fork moves into an overflow menu and the title keeps its own
    // hit region.
    let narrow = ui.available_width() < NARROW_ROW_WIDTH;
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        // Runtime/question indicators stay at the trailing edge; workspace
        // contention is an additional independent signal beside them.
        ui.spacing_mut().item_spacing.x = SP_1;
        thread_status_icon(ui, state, has_question);
        workspace_wait_icon(ui, thread, wait_context.0, wait_context.1);
        ui.add_space(SP_1);
        ui.spacing_mut().item_spacing.x = 0.0;
        if narrow {
            let menu = egui::containers::menu::MenuButton::from_button(
                ghost(icon_text(icons::DOTS_THREE)).min_size(egui::vec2(ACTION_SIZE, ACTION_SIZE)),
            )
            .ui(ui, |ui| {
                if ghost_icon_button(ui, icons::GIT_FORK, "Fork").clicked() {
                    *action = Some(SidebarAction::ForkThread(thread.id.clone()));
                    ui.close();
                }
            })
            .0;
            labeled(menu, "⋯").on_hover_text("Thread actions");
        } else if icon_button(ui, icons::GIT_FORK, "Fork").clicked() {
            *action = Some(SidebarAction::ForkThread(thread.id.clone()));
        }
        if thread.parent_thread_id.is_none() {
            let can_archive = !thread.pinned && !family_running;
            let disabled_reason = if family_running {
                "Archive unavailable while a thread in this family is running"
            } else {
                "Unpin this thread before archiving"
            };
            let archive = ui
                .add_enabled(
                    can_archive,
                    ghost(icon_text(icons::ARCHIVE)).min_size(egui::vec2(ACTION_SIZE, ACTION_SIZE)),
                )
                .on_hover_text("アーカイブ")
                .on_disabled_hover_text(disabled_reason);
            archive.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::Button, can_archive, "Archive")
            });
            if archive.clicked() {
                *action = Some(SidebarAction::ToggleArchive(thread.id.clone()));
            }
        } else {
            // Keep pins of child rows aligned with their root's pin.
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
        thread_title(ui, thread, action);
    });
}

fn workspace_wait_icon(
    ui: &mut Ui,
    thread: &ThreadRecord,
    sidebar: &SidebarState,
    telemetry: &TelemetryOverlay,
) {
    if telemetry.workspace_waits(&thread.run_ids).next().is_none() {
        return;
    }
    let response = ui.add(
        egui::Label::new(
            egui::RichText::new(icons::HOURGLASS_MEDIUM)
                .size(FONT_SMALL + 2.0)
                .color(palette().WARNING_FG),
        )
        .sense(Sense::hover()),
    );
    let label = format!("作業領域の使用待ち: {}", thread.id);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &label));
    response.on_hover_ui(|ui| {
        ui.set_max_width(420.0);
        let now = Instant::now();
        for (index, (run_id, call_id, entry)) in
            telemetry.workspace_waits(&thread.run_ids).enumerate()
        {
            if index > 0 {
                ui.separator();
            }
            ui.label(workspace_wait_tooltip(sidebar, run_id, call_id, entry, now));
        }
        // Only a visible tooltip needs the elapsed-time timer.
        ui.ctx().request_repaint_after(Duration::from_secs(1));
    });
}

fn workspace_wait_tooltip(
    sidebar: &SidebarState,
    run_id: &str,
    call_id: &str,
    entry: &WorkspaceWaitEntry,
    now: Instant,
) -> String {
    let seconds = entry.elapsed_at(now).as_secs();
    let elapsed = if seconds >= 3600 {
        format!(
            "{}時間{}分{}秒",
            seconds / 3600,
            seconds % 3600 / 60,
            seconds % 60
        )
    } else if seconds >= 60 {
        format!("{}分{}秒", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}秒")
    };
    let wait = &entry.waiting;
    let mut text = format!(
        "作業領域の使用待ち · {elapsed}\n待機中: {} ({run_id} / {call_id})",
        wait.tool_name,
    );
    if let Some(command) = &wait.command {
        text.push_str(&format!("\nコマンド: {command}"));
    }
    if let Some(holder) = &wait.holder {
        // Lookup includes other projects and archived threads. Only the runtime's
        // holder identity is authoritative; activity cannot identify a blocker.
        let title = sidebar
            .threads
            .iter()
            .find(|thread| thread.run_ids.contains(&holder.run_id))
            .map(|thread| format!("「{}」 ({})", thread.title, holder.run_id))
            .unwrap_or_else(|| holder.run_id.clone());
        text.push_str(&format!(
            "\n使用中: {title}\n処理: {} ({})",
            holder.tool_name, holder.call_id
        ));
        if let Some(command) = &holder.command {
            text.push_str(&format!("\nコマンド: {command}"));
        }
    } else {
        text.push_str("\n使用中: 不明");
    }
    text.push_str(&format!("\n対象: {}", wait.workspace_root.display()));
    text
}

fn archived_row(
    ui: &mut Ui,
    thread: &ThreadRecord,
    sidebar: &SidebarState,
    telemetry: &TelemetryOverlay,
    action: &mut Option<SidebarAction>,
) {
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = SP_1;
        workspace_wait_icon(ui, thread, sidebar, telemetry);
        if thread.parent_thread_id.is_none() && restore_button(ui).clicked() {
            *action = Some(SidebarAction::ToggleArchive(thread.id.clone()));
        }
        thread_title(ui, thread, action);
    });
}

fn restore_button(ui: &mut Ui) -> egui::Response {
    let response = ui.add(
        ghost(icon_text(icons::ARROW_U_UP_LEFT)).min_size(egui::vec2(ACTION_SIZE, ACTION_SIZE)),
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Restore"));
    response.on_hover_text("アーカイブを解除")
}

fn thread_title(ui: &mut Ui, thread: &ThreadRecord, action: &mut Option<SidebarAction>) {
    // Indentation and the caret already show nesting; "↳" stays in the
    // accessible name only.
    let title = row_title(ui, thread.title.as_str());
    if thread.parent_thread_id.is_some() {
        let label = format!("↳ {}", thread.title);
        title.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &label));
    }
    if title
        .on_hover_text(format!("thread ID: {}", thread.id))
        .clicked()
    {
        *action = Some(SidebarAction::SwitchThread(thread.id.clone()));
    }
}

fn nested_threads<'a>(threads: &[&'a ThreadRecord]) -> Vec<(&'a ThreadRecord, usize)> {
    fn append<'a>(
        thread: &'a ThreadRecord,
        depth: usize,
        threads: &[&'a ThreadRecord],
        seen: &mut BTreeSet<workspace_ui::ThreadId>,
        output: &mut Vec<(&'a ThreadRecord, usize)>,
    ) {
        if !seen.insert(thread.id.clone()) {
            return;
        }
        output.push((thread, depth));
        for child in threads
            .iter()
            .copied()
            .filter(|candidate| candidate.parent_thread_id.as_ref() == Some(&thread.id))
        {
            append(child, depth + 1, threads, seen, output);
        }
    }

    let mut seen = BTreeSet::new();
    let mut output = Vec::with_capacity(threads.len());
    for thread in threads.iter().copied() {
        if thread
            .parent_thread_id
            .as_ref()
            .is_none_or(|parent| !threads.iter().any(|candidate| &candidate.id == parent))
        {
            append(thread, 0, threads, &mut seen, &mut output);
        }
    }
    // Corrupt or cyclic parent links still leave every thread reachable.
    for thread in threads.iter().copied() {
        append(thread, 0, threads, &mut seen, &mut output);
    }
    output
}

fn thread_status_icon(ui: &mut Ui, state: ThreadState, has_question: bool) {
    // Runtime status retains the display aggregation; questions are independent
    // and must remain visible alongside either the spinner or the error icon.
    let status = if matches!(state, ThreadState::Running) {
        Some((
            ui.add(
                egui::Spinner::new()
                    .size(FONT_SMALL)
                    .color(palette().RUNNING),
            ),
            "Thread status: Running",
        ))
    } else if matches!(state, ThreadState::Error) {
        let response = ui.add(
            egui::Label::new(
                egui::RichText::new(icons::WARNING)
                    .size(FONT_SMALL + 2.0)
                    .color(palette().ERROR_FG),
            )
            .sense(Sense::hover()),
        );
        Some((response, "Thread status: Error"))
    } else {
        None
    };
    if let Some((response, label)) = status {
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Label, ui.is_enabled(), label)
        });
        response.on_hover_text(label);
    }
    if has_question {
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(FONT_SMALL, FONT_SMALL), Sense::hover());
        ui.painter()
            .circle_filled(rect.center(), SP_1, palette().WARNING_FG);
        let label = "Answer needed: this thread has an unanswered question";
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Label, ui.is_enabled(), label)
        });
        response.on_hover_text(label);
    }
}

#[cfg(test)]
#[path = "threads_wait_tests.rs"]
mod wait_tests;

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use egui_kittest::{Harness, kittest::Queryable};
    use workspace_ui::{ProjectId, ThreadId, ThreadRecord, ThreadRunPhase, ThreadState};

    use super::{active_row, family_has_running_runs, nested_threads, thread_family};
    use crate::panes::sidebar::SidebarAction;

    fn assert_thread_actions_without_pause(width: f32, narrow: bool) {
        // Given: a thread row rendered at the requested width.
        let thread = ThreadRecord::new(ThreadId::new("thread"), ProjectId::new("demo"), "Thread");
        let sidebar = workspace_ui::SidebarState::default();
        let telemetry = crate::model::telemetry::TelemetryOverlay::new();
        let mut harness = Harness::builder()
            .with_size(egui::vec2(width, 100.0))
            .build_ui_state(
                |ui, action| {
                    crate::theme::install(ui.ctx());
                    ui.horizontal(|ui| {
                        active_row(
                            ui,
                            &thread,
                            ThreadState::Running,
                            false,
                            true,
                            (&sidebar, &telemetry),
                            action,
                        );
                    });
                },
                None,
            );
        // A spinner continuously requests repaint; advance bounded frames.
        harness.run_steps(2);

        // When: opening the thread actions menu if the row is narrow.
        if narrow {
            harness.get_by_label("⋯").click();
            harness.run_steps(2);
        } else {
            assert!(harness.query_by_label("⋯").is_none());
        }

        // Then: Pause/Resume are absent, while runtime status and Fork still work.
        assert!(harness.query_by_label("Pause").is_none());
        assert!(harness.query_by_label("Resume").is_none());
        assert!(harness.query_by_label("Thread status: Running").is_some());
        harness.get_by_label("Fork").click();
        harness.run_steps(2);
        assert_eq!(
            harness.state(),
            &Some(SidebarAction::ForkThread(thread.id.clone()))
        );
    }

    #[test]
    fn wide_thread_row_has_no_pause_or_resume_control() {
        assert_thread_actions_without_pause(600.0, false);
    }

    #[test]
    fn narrow_thread_menu_has_no_pause_or_resume_control() {
        assert_thread_actions_without_pause(220.0, true);
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
        let rows = nested_threads(&[&grandchild, &child, &parent]);
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
}
