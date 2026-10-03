use std::collections::{BTreeMap, BTreeSet};

use egui::{Align, Layout, Sense, Ui};
use workspace_ui::{
    ProjectRecord, SidebarState, ThreadId, ThreadRecord, ThreadRunPhase, ThreadState,
};

use crate::model::telemetry::TelemetryOverlay;
use crate::theme::text::h4;
use crate::theme::tokens::{FONT_SMALL, ROW_DENSE, SP_1, SP_2, palette};
use crate::theme::widgets::{compact_row, empty_state, primary_button};

use super::SidebarAction;

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
    _telemetry: &TelemetryOverlay,
    question_threads: &BTreeSet<workspace_ui::ThreadId>,
    action: &mut Option<SidebarAction>,
) {
    let (project_threads, archived) =
        ThreadRecord::partition_for_project(&sidebar.threads, &project.id);

    ui.separator();
    ui.horizontal(|ui| {
        ui.label(h4("Threads"));
        let has_threads = !project_threads.is_empty();
        let new_thread_clicked = if has_threads {
            ui.button("New thread").clicked()
        } else {
            primary_button(ui, "New thread").clicked()
        };
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

    render_tree(
        ui,
        sidebar,
        &project_threads,
        phases,
        question_threads,
        false,
        action,
    );

    egui::CollapsingHeader::new(format!("アーカイブ済み ({})", archived.len()))
        .id_salt(("archived-threads", &project.id))
        .show(ui, |ui| {
            render_tree(
                ui,
                sidebar,
                &archived,
                phases,
                question_threads,
                true,
                action,
            );
        });
    ui.add_space(SP_2);
}

fn render_tree(
    ui: &mut Ui,
    sidebar: &SidebarState,
    threads: &[&ThreadRecord],
    phases: &BTreeMap<String, ThreadRunPhase>,
    question_threads: &BTreeSet<workspace_ui::ThreadId>,
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
                if has_children {
                    let (rect, toggle) =
                        ui.allocate_exact_size(egui::vec2(16.0, ROW_DENSE), Sense::click());
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
                    egui::collapsing_header::paint_default_icon(
                        ui,
                        if expansion.is_open() { 1.0 } else { 0.0 },
                        &toggle.with_new_rect(rect.shrink(2.0)),
                    );
                }
                if archived {
                    archived_row(ui, thread, action);
                } else {
                    let state = thread.state(phases);
                    let family_running = thread.parent_thread_id.is_none()
                        && family_has_running_runs(
                            &sidebar.threads,
                            &thread_family(&sidebar.threads, &thread.id),
                            phases,
                        );
                    active_row(
                        ui,
                        thread,
                        state,
                        question_threads.contains(&thread.id),
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
    action: &mut Option<SidebarAction>,
) {
    let pin = if thread.pinned { "★" } else { "☆" };
    if ui.button(pin).clicked() {
        *action = Some(SidebarAction::TogglePin(thread.id.clone()));
    }
    if thread.parent_thread_id.is_none() {
        let can_archive = !thread.pinned && !family_running;
        let disabled_reason = if family_running {
            "Archive unavailable while a thread in this family is running"
        } else {
            "Unpin this thread before archiving"
        };
        let archive = ui
            .add_enabled(can_archive, archive_button)
            .on_hover_text("アーカイブ")
            .on_disabled_hover_text(disabled_reason);
        archive.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, can_archive, "Archive")
        });
        if archive.clicked() {
            *action = Some(SidebarAction::ToggleArchive(thread.id.clone()));
        }
    } else {
        ui.add_space(ui.text_style_height(&egui::TextStyle::Small) + SP_2);
    }
    // At the minimum window size the action button leaves little
    // room for the title, and egui's minimum Label width overlaps earlier
    // controls. Keep the title and controls in distinct hit regions.
    let narrow = ui.available_width() < 180.0;
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        // The first widget in this layout is the row's trailing indicator.
        thread_status_icon(ui, state, has_question);
        if narrow {
            ui.menu_button("⋯", |ui| {
                if ui.button("Fork").clicked() {
                    *action = Some(SidebarAction::ForkThread(thread.id.clone()));
                    ui.close();
                }
            })
            .response
            .on_hover_text("Thread actions");
        } else {
            if ui.small_button("Fork").clicked() {
                *action = Some(SidebarAction::ForkThread(thread.id.clone()));
            }
        }
        thread_title(ui, thread, action);
    });
}

fn archived_row(ui: &mut Ui, thread: &ThreadRecord, action: &mut Option<SidebarAction>) {
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if thread.parent_thread_id.is_none()
            && ui
                .small_button("Restore")
                .on_hover_text("アーカイブを解除")
                .clicked()
        {
            *action = Some(SidebarAction::ToggleArchive(thread.id.clone()));
        }
        thread_title(ui, thread, action);
    });
}

fn thread_title(ui: &mut Ui, thread: &ThreadRecord, action: &mut Option<SidebarAction>) {
    if ui
        .add_sized(
            egui::vec2(ui.available_width().max(0.0), ROW_DENSE),
            egui::Label::new(format!(
                "{}{}",
                if thread.parent_thread_id.is_some() {
                    "↳ "
                } else {
                    ""
                },
                thread.title
            ))
            .truncate()
            .halign(Align::LEFT)
            .sense(Sense::click()),
        )
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

fn archive_button(ui: &mut Ui) -> egui::Response {
    let size = ui.text_style_height(&egui::TextStyle::Small);
    let response = ui.add(
        egui::Button::new("")
            .small()
            .min_size(egui::vec2(size + SP_2, size + SP_2)),
    );
    let rect = egui::Rect::from_center_size(response.rect.center(), egui::vec2(size, size));
    let color = ui.style().interact(&response).text_color();
    paint_archive_box_icon(ui.painter(), rect, color);
    response
}

fn paint_archive_box_icon(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let stroke = egui::Stroke::new(1.2, color);
    let point = |x: f32, y: f32| rect.min + egui::vec2(x * rect.width(), y * rect.height());
    // A balanced outline with an overhanging lid and a centered drawer pull.
    painter.add(egui::Shape::line(
        vec![
            point(0.18, 0.34),
            point(0.18, 0.87),
            point(0.82, 0.87),
            point(0.82, 0.34),
        ],
        stroke,
    ));
    painter.rect_stroke(
        egui::Rect::from_min_max(point(0.08, 0.12), point(0.92, 0.34)),
        1,
        stroke,
        egui::StrokeKind::Inside,
    );
    painter.line_segment([point(0.38, 0.53), point(0.62, 0.53)], stroke);
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
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(FONT_SMALL, FONT_SMALL), Sense::hover());
        let point = |x: f32, y: f32| rect.min + egui::vec2(x * rect.width(), y * rect.height());
        let stroke = egui::Stroke::new(1.2, palette().ERROR_FG);
        ui.painter().add(egui::Shape::closed_line(
            vec![point(0.5, 0.08), point(0.94, 0.88), point(0.06, 0.88)],
            stroke,
        ));
        ui.painter()
            .line_segment([point(0.5, 0.34), point(0.5, 0.57)], stroke);
        ui.painter()
            .circle_filled(point(0.5, 0.72), 0.8, palette().ERROR_FG);
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
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use egui_kittest::{Harness, kittest::Queryable};
    use workspace_ui::{ProjectId, ThreadId, ThreadRecord, ThreadRunPhase, ThreadState};

    use super::{active_row, family_has_running_runs, nested_threads, thread_family};
    use crate::panes::sidebar::SidebarAction;

    fn assert_thread_actions_without_pause(width: f32, narrow: bool) {
        // Given: a thread row rendered at the requested width.
        let thread = ThreadRecord::new(ThreadId::new("thread"), ProjectId::new("demo"), "Thread");
        let mut harness = Harness::builder()
            .with_size(egui::vec2(width, 100.0))
            .build_ui_state(
                |ui, action| {
                    crate::theme::install(ui.ctx());
                    ui.horizontal(|ui| {
                        active_row(ui, &thread, ThreadState::Running, false, true, action);
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
