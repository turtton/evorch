use egui::{Id, Rect, epaint::Shape, vec2};
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};
use gui::model::transcript::{ToolStatus, TranscriptEntry, shell_jobs::ShellJobs};
use gui::panes::transcript_tool::{ToolCardContext, tool_card_group};

const TITLE: &str = "✓ Shell long-output";

struct State {
    entries: Vec<TranscriptEntry>,
    jobs: ShellJobs,
    viewport: Rect,
    offset: f32,
    scroll_to: Option<f32>,
}

fn harness() -> Harness<'static, State> {
    let state = State {
        entries: vec![TranscriptEntry::Tool {
            run_id: Some("run-sticky".into()),
            tool_name: "shell".into(),
            call_id: "sticky-call".into(),
            input: Some(serde_json::json!({"command": "long-output"})),
            output: Some((0..100).map(|i| format!("output line {i}\n")).collect()),
            detail: None,
            is_error: false,
            status: ToolStatus::Succeeded,
        }],
        jobs: ShellJobs::default(),
        viewport: Rect::NOTHING,
        offset: 0.0,
        scroll_to: None,
    };
    let mut harness = Harness::builder()
        .with_size(vec2(640.0, 400.0))
        .build_ui_state(
            |ui, state: &mut State| {
                gui::theme::install(ui.ctx());
                ui.label("Chat log");
                ui.label("Thread header stays outside the scroll area");
                let scroll = egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if let Some(target) = state.scroll_to.take() {
                            ui.scroll_with_delta(vec2(0.0, state.offset - target));
                        }
                        ui.label("Earlier conversation");
                        ui.add_space(80.0);
                        tool_card_group(
                            ui,
                            &state.entries,
                            Id::new("sticky-card"),
                            ToolCardContext {
                                repo_root: None,
                                jobs: &state.jobs,
                                latest_for_job: false,
                            },
                        );
                        ui.label("Following conversation");
                        for i in 0..35 {
                            ui.label(format!("following message {i}"));
                        }
                    });
                state.viewport = scroll.inner_rect;
                state.offset = scroll.state.offset.y;
            },
            state,
        );
    harness.run_steps(3);
    harness
}

fn scroll(harness: &mut Harness<'_, State>, offset: f32) {
    harness.state_mut().scroll_to = Some(offset);
    harness.run_steps(4);
}

fn expanded() -> Harness<'static, State> {
    let mut harness = harness();
    harness.get_by_label(TITLE).click();
    harness.run_steps(4);
    harness
}

fn title_rect(harness: &Harness<'_, State>) -> Rect {
    harness.get_by_label(TITLE).rect()
}

#[test]
fn expanded_title_sticks_to_log_viewport_not_thread_header() {
    // Given: a completed, expanded call starts at its normal card position.
    let mut harness = expanded();
    let initial = title_rect(&harness);
    let title_id = harness.get_by_label(TITLE).accesskit_node().id();
    assert!(initial.top() > harness.state().viewport.top() + 50.0);
    // When: the user reads the middle of its long output.
    scroll(&mut harness, 450.0);
    // Then: the same single title remains below the thread header.
    let title = title_rect(&harness);
    assert!((title.top() - harness.state().viewport.top()).abs() < 1.0);
    assert_eq!(harness.get_by_label(TITLE).accesskit_node().id(), title_id);
    assert!(title.top() > harness.get_by_label("Chat log").rect().bottom());
    assert!(harness.query_by_label("Output").is_some());
    // A body shape must be painted before the opaque title surface and text.
    let shapes = &harness.output().shapes;
    let output_index = shapes
        .iter()
        .position(|s| {
            matches!(&s.shape,
        Shape::Text(t) if t.galley.text().contains("output line 0"))
        })
        .unwrap();
    let title_index = shapes
        .iter()
        .position(|s| {
            matches!(&s.shape,
        Shape::Text(t) if t.galley.text().contains("Shell long-output"))
        })
        .unwrap();
    assert!(output_index < title_index);
    assert!(
        shapes[output_index..title_index]
            .iter()
            .any(|s| matches!(&s.shape,
        Shape::Rect(r) if r.fill == gui::theme::tokens::palette().SURFACE
            && r.rect.contains(title.center())))
    );
}

#[test]
fn pinned_title_pointer_collapse_keeps_title_visible_and_can_reopen() {
    // Given
    let mut harness = expanded();
    scroll(&mut harness, 800.0);
    // When: clicking the actual on-screen rectangle, not an accessibility action.
    let center = title_rect(&harness).center();
    harness.event(egui::Event::PointerMoved(center));
    for pressed in [true, false] {
        harness.event(egui::Event::PointerButton {
            pos: center,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        });
        harness.step();
    }
    harness.run_steps(4);
    // Then: the body disappears, the collapsed title remains reachable.
    assert!(harness.query_by_label("Output").is_none());
    assert!(harness.state().viewport.contains_rect(title_rect(&harness)));
    assert!(harness.state().offset < 200.0);
    harness.get_by_label(TITLE).click();
    harness.run_steps(4);
    assert!(harness.query_by_label("Output").is_some());
}

#[test]
fn pinned_title_supports_keyboard_collapse() {
    // Given: a keyboard-focused title keeps its identity while scrolling.
    let mut harness = expanded();
    harness.get_by_label(TITLE).focus();
    harness.run_steps(2);
    assert!(harness.get_by_label(TITLE).is_focused());
    scroll(&mut harness, 450.0);
    assert!(harness.get_by_label(TITLE).is_focused());
    // When
    harness.key_press(egui::Key::Space);
    harness.run_steps(4);
    // Then
    assert!(harness.query_by_label("Output").is_none());
    assert!(harness.state().viewport.contains_rect(title_rect(&harness)));
}

#[test]
fn title_leaves_when_card_passes_and_returns_to_normal_position() {
    // Given
    let mut harness = expanded();
    let initial = title_rect(&harness);
    scroll(&mut harness, 450.0);
    // When: scrolling the card's end beyond the top edge.
    let following = harness.get_by_label("Following conversation").rect();
    let end_offset = harness.state().offset + following.top() - harness.state().viewport.top();
    scroll(&mut harness, end_offset + 100.0);
    // Then: the preceding title does not linger over the following messages.
    assert!(title_rect(&harness).bottom() < harness.state().viewport.top());
    scroll(&mut harness, 0.0);
    assert!((title_rect(&harness).top() - initial.top()).abs() < 1.0);
}

#[test]
fn collapsed_title_does_not_stick_and_narrow_resizing_keeps_expanded_title() {
    // Given / When: a closed card passes the top edge.
    let mut harness = harness();
    scroll(&mut harness, 450.0);
    // Then
    assert!(title_rect(&harness).bottom() < harness.state().viewport.top());
    scroll(&mut harness, 0.0);
    harness.get_by_label(TITLE).click();
    harness.run_steps(4);
    scroll(&mut harness, 450.0);
    // When: wrapping and DPI change the body and viewport geometry.
    harness.set_size(vec2(320.0, 300.0));
    harness.set_pixels_per_point(1.5);
    harness.run_steps(4);
    // Then
    let title = title_rect(&harness);
    assert!((title.top() - harness.state().viewport.top()).abs() < 1.0);
    assert!(harness.state().viewport.contains_rect(title));
}

#[test]
fn folded_poll_title_and_shell_log_action_remain_reachable_while_pinned() {
    use event_bus::{Event, ToolEvent};
    use gui::model::transcript::{TranscriptModel, shell_jobs::ShellJobKey};
    let mut harness = harness();
    let mut model = TranscriptModel::new();
    for index in 0..2 {
        model.apply(&Event::new(ToolEvent::ToolStarted {
            tool_name: "shell".into(),
            call_id: format!("poll-{index}"),
            input: Some(serde_json::json!({"action":"poll", "job_id":"job-1"})),
            run_id: Some("run-sticky".into()),
        }));
        model.apply(&Event::new(ToolEvent::ToolCompleted {
            tool_name: "shell".into(),
            call_id: format!("poll-{index}"),
            is_error: false,
            output: Some(format!(
                "shell job: job-1\nstatus: running\ncursor: {index}\n{}",
                "poll output\n".repeat(70)
            )),
            detail: Some(serde_json::json!({"shell_job":{"job_id":"job-1", "status":"running"}})),
            run_id: Some("run-sticky".into()),
        }));
    }
    harness.state_mut().entries = model.entries().to_vec();
    harness.state_mut().jobs = model.shell_jobs().clone();
    harness.run_steps(3);
    let title = "✓ Poll #job-1 · ×2 · running";
    harness.get_by_label(title).click();
    harness.run_steps(4);
    scroll(&mut harness, 450.0);
    assert!(
        (harness.get_by_label(title).rect().top() - harness.state().viewport.top()).abs() < 1.0
    );
    assert!(harness.query_by_label("Output of 2 polls").is_some());
    let log = harness.get_by_label("Open shell job log").rect();
    assert!(harness.state().viewport.contains_rect(log));
    harness.get_by_label("Open shell job log").click();
    harness.run_steps(3);
    assert_eq!(
        gui::panes::shell_jobs::take_open_request(&harness.ctx),
        Some(ShellJobKey::new(Some("run-sticky"), "job-1", None))
    );
    assert!(harness.query_by_label("Output of 2 polls").is_some());
}

#[test]
fn running_call_remains_disabled_and_finished_call_can_stick() {
    let mut harness = harness();
    if let TranscriptEntry::Tool { status, .. } = &mut harness.state_mut().entries[0] {
        *status = ToolStatus::Running;
    }
    harness.run_steps(3);
    let running = "Shell long-output";
    harness.get_by_label(running).click();
    harness.run_steps(3);
    assert!(harness.query_by_label("Output").is_none());
    scroll(&mut harness, 450.0);
    assert!(harness.get_by_label(running).rect().bottom() < harness.state().viewport.top());
    scroll(&mut harness, 0.0);
    if let TranscriptEntry::Tool { status, .. } = &mut harness.state_mut().entries[0] {
        *status = ToolStatus::Succeeded;
    }
    harness.run_steps(3);
    harness.get_by_label(TITLE).click();
    harness.run_steps(4);
    scroll(&mut harness, 450.0);
    assert!((title_rect(&harness).top() - harness.state().viewport.top()).abs() < 1.0);
}

#[test]
fn changed_output_releases_title_without_a_cached_card_height() {
    let mut harness = expanded();
    scroll(&mut harness, 450.0);
    if let TranscriptEntry::Tool { output, .. } = &mut harness.state_mut().entries[0] {
        *output = Some("shortened output".into());
    }
    harness.run_steps(3);
    assert!(title_rect(&harness).bottom() < harness.state().viewport.top());
}

#[test]
fn pinned_title_capture_when_evidence_directory_is_set() {
    let Some(directory) = std::env::var_os("EVORCH_TOOL_CAPTURE_DIR") else {
        return;
    };
    let directory = std::path::Path::new(&directory);
    let mut harness = expanded();
    scroll(&mut harness, 450.0);
    harness
        .render()
        .expect("render pinned title")
        .save(directory.join("tool-sticky-expanded.png"))
        .expect("save pinned title");
    harness.get_by_label(TITLE).click();
    harness.run_steps(4);
    harness.remove_cursor();
    harness
        .render()
        .expect("render collapsed title")
        .save(directory.join("tool-sticky-collapsed.png"))
        .expect("save collapsed title");
}
