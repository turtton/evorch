//! Geometry and widget-count contracts, not wall-clock performance thresholds.

use egui_kittest::{Harness, kittest::Queryable};
use event_bus::{AgentRunPhase, Event, LifecycleEvent, MessageEvent, ToolEvent};
use gui::model::transcript::TranscriptModel;
use gui::panes::agent::{
    set_transcript_virtualization_for_tests, transcript_body, transcript_layout_for_tests,
};

struct State {
    model: TranscriptModel,
    force: Option<bool>,
}

fn transcript(count: usize) -> TranscriptModel {
    let mut model = TranscriptModel::with_capacity(count + 1);
    for index in 0..count {
        model.push_user_message(format!("Entry {index}"));
    }
    model
}

fn harness(model: TranscriptModel, force: Option<bool>) -> Harness<'static, State> {
    let mut harness = build_harness(model, force);
    harness.run();
    harness
}

fn build_harness(model: TranscriptModel, force: Option<bool>) -> Harness<'static, State> {
    Harness::builder()
        .with_size(egui::vec2(640.0, 480.0))
        .build_ui_state(draw_transcript, State { model, force })
}

fn draw_transcript(ui: &mut egui::Ui, state: &mut State) {
    gui::theme::install(ui.ctx());
    if let Some(force) = state.force {
        set_transcript_virtualization_for_tests(ui.ctx(), force);
    }
    transcript_body(ui, &state.model);
}

fn content_height(harness: &Harness<'_, State>) -> f32 {
    transcript_layout_for_tests(&harness.ctx)
        .expect("layout observed")
        .0
        .y
}

/// Compare the complete extent before bringing a mutated offscreen row into
/// view. Scrolling to it first would let a stale-cache implementation pass.
fn assert_matches_full_layout(harness: &mut Harness<'_, State>) {
    let before = transcript_layout_for_tests(&harness.ctx).unwrap();
    harness.state_mut().force = Some(false);
    harness.run();
    let full = transcript_layout_for_tests(&harness.ctx).unwrap();
    assert_eq!(before.0.y, full.0.y, "total content height");
    assert_eq!(before.1.y, full.1.y, "scroll offset");
    harness.state_mut().force = Some(true);
    harness.run();
}

fn message(model: &mut TranscriptModel, text: &str) {
    model.apply(&Event::new(MessageEvent::MessageDelta {
        run_id: Some("revision-run".into()),
        delta: text.into(),
    }));
}

fn tool_start(model: &mut TranscriptModel, name: &str, call: &str, parent: Option<&str>) {
    model.apply(&Event::new(ToolEvent::ToolStarted {
        tool_name: name.into(),
        call_id: call.into(),
        input: Some(match name {
            "run_js" => serde_json::json!({"code": "return await read({path: 'offscreen.rs'});"}),
            "shell" => serde_json::json!({"command": "cargo test"}),
            _ => serde_json::json!({"path": "offscreen.rs"}),
        }),
        run_id: Some("revision-run".into()),
        parent_call_id: parent.map(str::to_owned),
    }));
}

fn tool_complete(model: &mut TranscriptModel, name: &str, call: &str, parent: Option<&str>) {
    tool_result(
        model,
        name,
        call,
        parent,
        &"offscreen tool result\n".repeat(12),
    );
}

fn tool_result(
    model: &mut TranscriptModel,
    name: &str,
    call: &str,
    parent: Option<&str>,
    output: &str,
) {
    model.apply(&Event::new(ToolEvent::ToolCompleted {
        tool_name: name.into(),
        call_id: call.into(),
        is_error: false,
        output: Some(output.into()),
        detail: None,
        run_id: Some("revision-run".into()),
        parent_call_id: parent.map(str::to_owned),
    }));
}

fn add_history(model: &mut TranscriptModel) {
    for index in 0..2_000 {
        model.push_notice(format!("History {index}"));
    }
}

#[test]
fn offscreen_stream_growth_updates_the_extent_before_scrolling_back() {
    let mut model = transcript(2_000);
    message(&mut model, "offscreen stream");
    let mut harness = harness(model, Some(true));
    scroll(&mut harness, 1_000_000.0);
    assert!(harness.query_by_label("offscreen stream").is_none());
    let before = content_height(&harness);
    message(
        &mut harness.state_mut().model,
        &"\n\nanother paragraph".repeat(20),
    );
    harness.run();
    assert!(content_height(&harness) > before);
    assert!(harness.query_by_label("offscreen stream").is_none());
    assert_matches_full_layout(&mut harness);
    assert_on_screen(&harness, "You: Entry 0");
}

#[test]
fn task_prompt_reordering_invalidates_both_slots_without_unsorting_ids() {
    use gui::model::transcript::TranscriptEntry;

    let mut model = TranscriptModel::new();
    model.push_notice("subagent worker started");
    let notice_id = model.visible_entry_id(0);
    let notice_revision = model.entry_revision(notice_id).unwrap();
    let event = Event::new(LifecycleEvent::TaskPromptPublished {
        run_id: "prompt-run".into(),
        parent_run_id: Some("parent-run".into()),
        agent_name: "worker".into(),
        role: "worker".into(),
        prompt: "delegated instruction".into(),
    });
    model.apply(&event);
    let ids: Vec<_> = model.visible_entry_ids().collect();
    assert_eq!(ids[0], notice_id);
    assert!(ids[0] < ids[1], "ID lookup remains sorted after reordering");
    assert_eq!(model.entry_revision(ids[0]), Some(notice_revision + 1));
    assert_eq!(model.entry_revision(ids[1]), Some(1));
    assert!(
        matches!(&model.entries()[0], TranscriptEntry::UserMessage { text, .. }
        if text == "delegated instruction")
    );
    assert!(
        matches!(&model.entries()[1], TranscriptEntry::Notice { text }
        if text == "subagent worker started")
    );
    let revisions: Vec<_> = ids.iter().map(|id| model.entry_revision(*id)).collect();
    model.apply(&event);
    assert_eq!(model.visible_entry_ids().collect::<Vec<_>>(), ids);
    assert_eq!(
        ids.iter()
            .map(|id| model.entry_revision(*id))
            .collect::<Vec<_>>(),
        revisions,
        "replaying an unchanged instruction does not invalidate either slot"
    );
}

#[test]
fn offscreen_tool_completion_remeasures_its_expanded_card() {
    let mut model = TranscriptModel::new();
    tool_start(&mut model, "read", "read", None);
    tool_result(&mut model, "read", "read", None, "short result");
    let mut harness = harness(model, Some(true));
    harness.get_by_label_contains("Read offscreen.rs").click();
    harness.run_steps(3);
    add_history(&mut harness.state_mut().model);
    // The new scrollbar changes the width: measure twice at that width,
    // then observe the offscreen row being skipped. The running spinner
    // requests repaints indefinitely, so Harness::run cannot settle it.
    harness.run_steps(4);
    assert!(
        harness
            .query_by_label_contains("Read offscreen.rs")
            .is_none()
    );
    let before = content_height(&harness);
    tool_complete(&mut harness.state_mut().model, "read", "read", None);
    harness.run();
    assert!(content_height(&harness) > before);
    assert_matches_full_layout(&mut harness);
    assert_on_screen(&harness, "History 1999");
}

#[test]
fn offscreen_thinking_end_remeasures_the_automatically_collapsed_body() {
    let mut model = TranscriptModel::new();
    model.apply(&Event::new(MessageEvent::ReasoningDelta {
        run_id: Some("revision-run".into()),
        delta: "thinking paragraph\n\n".repeat(30),
    }));
    add_history(&mut model);
    // Simulated frames smaller than the 200ms collapse animation expose
    // intermediate heights; the default kittest step is already 250ms.
    let mut harness = Harness::builder()
        .with_size(egui::vec2(640.0, 480.0))
        .with_step_dt(1.0 / 60.0)
        .with_max_steps(32)
        .build_ui_state(
            draw_transcript,
            State {
                model,
                force: Some(true),
            },
        );
    harness.run();
    assert!(harness.query_by_label("thinking").is_none());
    let before = content_height(&harness);
    harness
        .state_mut()
        .model
        .apply(&Event::new(LifecycleEvent::AgentRunStateChanged {
            run_id: "revision-run".into(),
            from: AgentRunPhase::Running,
            to: AgentRunPhase::Done,
            reason: None,
        }));
    harness.run();
    assert!(content_height(&harness) < before);
    assert_matches_full_layout(&mut harness);
}

#[test]
fn offscreen_run_js_parent_remeasures_when_its_nested_child_completes() {
    let mut model = TranscriptModel::new();
    tool_start(&mut model, "run_js", "js", None);
    tool_start(&mut model, "read", "child", Some("js"));
    tool_result(&mut model, "read", "child", Some("js"), "short result");
    tool_complete(&mut model, "run_js", "js", None);
    let mut harness = build_harness(model, Some(true));
    harness.run_steps(3);
    harness.get_by_label_contains("Run JS").click();
    harness.run_steps(3);
    harness.get_by_label_contains("Read offscreen.rs").click();
    harness.run_steps(3);
    add_history(&mut harness.state_mut().model);
    harness.run_steps(3);
    let before = content_height(&harness);
    tool_complete(&mut harness.state_mut().model, "read", "child", Some("js"));
    harness.run();
    assert!(content_height(&harness) > before);
    assert_matches_full_layout(&mut harness);
}

#[test]
fn live_shell_output_invalidates_an_offscreen_card_without_changing_its_entry() {
    let mut model = TranscriptModel::new();
    tool_start(&mut model, "shell", "shell", None);
    model.apply(&Event::new(ToolEvent::ToolCompleted {
        tool_name: "shell".into(),
        call_id: "shell".into(),
        is_error: false,
        output: Some("shell job: job-1\nstatus: running\ncursor: 0\n".into()),
        detail: Some(serde_json::json!({"shell_job": {"job_id": "job-1", "status": "running"}})),
        run_id: Some("revision-run".into()),
        parent_call_id: None,
    }));
    add_history(&mut model);
    let entry_id = model.visible_entry_id(0);
    let entry_revision = model.entry_revision(entry_id);
    let mut harness = harness(model, Some(true));
    let before = content_height(&harness);
    harness
        .state_mut()
        .model
        .apply(&Event::new(ToolEvent::ShellJobOutput {
            job_uid: None,
            job_id: "job-1".into(),
            call_id: Some("shell".into()),
            run_id: Some("revision-run".into()),
            offset: 0,
            chunk: "line 1\nline 2\nline 3\n".into(),
            status: "running".into(),
            exit_code: None,
        }));
    harness.run();
    assert_eq!(
        harness.state().model.entry_revision(entry_id),
        entry_revision
    );
    assert!(content_height(&harness) > before);
    assert_matches_full_layout(&mut harness);
}

#[test]
fn offscreen_poll_group_remeasures_when_a_result_breaks_the_group() {
    let mut model = TranscriptModel::new();
    for call in ["poll-a", "poll-b"] {
        tool_start(&mut model, "inbox", call, None);
        tool_result(&mut model, "inbox", call, None, "[]");
    }
    add_history(&mut model);
    let mut harness = harness(model, Some(true));
    let before = content_height(&harness);
    tool_result(
        &mut harness.state_mut().model,
        "inbox",
        "poll-b",
        None,
        "new message",
    );
    harness.run();
    assert!(content_height(&harness) > before);
    assert_matches_full_layout(&mut harness);
    scroll(&mut harness, 1_000_000.0);
    assert_eq!(harness.query_all_by_label_contains("Read inbox").count(), 2);
}

#[test]
fn response_consolidation_never_reassigns_removed_entry_ids() {
    let mut model = TranscriptModel::new();
    message(&mut model, "first fragment");
    model.apply(&Event::new(MessageEvent::ReasoningDelta {
        run_id: Some("revision-run".into()),
        delta: "interleaved reasoning".into(),
    }));
    message(&mut model, "second fragment");
    model.apply(&Event::new(MessageEvent::ToolCallDelta {
        run_id: Some("revision-run".into()),
        index: 0,
        call_id: Some("draft".into()),
        tool_name: Some("read".into()),
        arguments_delta: "{\"path\":".into(),
    }));
    add_history(&mut model);
    let ids: Vec<_> = model.visible_entry_ids().collect();
    let removed = ids[2];
    let mut harness = build_harness(model, Some(true));
    harness.run_steps(3);
    let before = content_height(&harness);
    harness
        .state_mut()
        .model
        .apply(&Event::new(MessageEvent::MessageCompleted {
            run_id: "revision-run".into(),
            text: "canonical answer".into(),
        }));
    harness.run_steps(3);
    let model = &harness.state().model;
    assert!(model.entry_by_id(removed).is_none());
    let retained: Vec<_> = model.visible_entry_ids().collect();
    assert_eq!(
        retained,
        ids.into_iter()
            .filter(|id| *id != removed)
            .collect::<Vec<_>>()
    );
    assert!(matches!(
        model.entry_by_id(retained[2]),
        Some(gui::model::transcript::TranscriptEntry::Tool {
            call_id,
            status: gui::model::transcript::ToolStatus::Drafting { sealed: true, .. },
            ..
        }) if call_id == "draft"
    ));
    assert!(content_height(&harness) < before);
    // End the drafting spinner before the full-layout comparison.
    tool_start(&mut harness.state_mut().model, "read", "draft", None);
    tool_complete(&mut harness.state_mut().model, "read", "draft", None);
    harness.run();
    assert_matches_full_layout(&mut harness);
    let previous_max = *retained.last().unwrap();
    harness.state_mut().model.push_notice("new entry");
    assert!(harness.state().model.visible_entry_ids().last().unwrap() > previous_max);
}

#[test]
fn visible_selection_survives_eviction_and_response_consolidation() {
    use egui_kittest::kittest::NodeT;
    for consolidate in [false, true] {
        let (model, label) = if consolidate {
            let mut model = TranscriptModel::new();
            message(&mut model, "first fragment");
            model.apply(&Event::new(MessageEvent::ReasoningDelta {
                run_id: Some("revision-run".into()),
                delta: "reasoning".into(),
            }));
            message(&mut model, "second fragment");
            add_history(&mut model);
            (model, "History 1999")
        } else {
            let mut model = transcript(2_000);
            model.push_user_message("Entry 2000");
            (model, "You: Entry 1999")
        };
        let mut harness = harness(model, Some(true));
        let id = harness.get_by_label(label).accesskit_node().id();
        let rect = harness.get_by_label(label).rect();
        let start = egui::pos2(rect.left() + 1.0, rect.center().y);
        let end = egui::pos2(rect.right() - 1.0, rect.center().y);
        harness.hover_at(start);
        harness.step();
        harness.drag_at(start);
        harness.step();
        harness.hover_at(end);
        harness.step();
        harness.drop_at(end);
        harness.run();
        assert!(
            harness
                .ctx
                .with_plugin(
                    |selection: &mut egui::text_selection::LabelSelectionState| selection
                        .has_selection()
                )
                .unwrap(),
            "drag selected text"
        );
        let copy = |harness: &mut Harness<'_, State>| {
            harness.event(egui::Event::Copy);
            harness.step();
            harness
                .output()
                .platform_output
                .commands
                .iter()
                .find_map(|command| match command {
                    egui::OutputCommand::CopyText(text) => Some(text.clone()),
                    _ => None,
                })
                .expect("selection copied")
        };
        let selected = copy(&mut harness);
        assert_eq!(
            selected,
            if consolidate {
                "History 1999"
            } else {
                "Entry 1999"
            }
        );
        if consolidate {
            harness
                .state_mut()
                .model
                .apply(&Event::new(MessageEvent::MessageCompleted {
                    run_id: "revision-run".into(),
                    text: "canonical answer".into(),
                }));
        } else {
            harness.state_mut().model.push_user_message("Entry 2001");
        }
        harness.run();
        assert!(
            harness
                .ctx
                .with_plugin(
                    |selection: &mut egui::text_selection::LabelSelectionState| selection
                        .has_selection()
                )
                .unwrap(),
            "selection survived preceding row removal"
        );
        assert_eq!(id, harness.get_by_label(label).accesskit_node().id());
        assert_eq!(copy(&mut harness), selected);
    }
}

fn assert_on_screen(harness: &Harness<'_, State>, label: &str) -> egui::Rect {
    let rect = harness.get_by_label(label).rect();
    assert!(
        harness.ctx.content_rect().contains_rect(rect),
        "{label}: {rect:?} is outside {:?}",
        harness.ctx.content_rect()
    );
    rect
}

fn assert_bounded(harness: &Harness<'_, State>) {
    let count = harness.query_all_by_label_contains("You: Entry ").count();
    assert!(
        (1..=60).contains(&count),
        "rendered {count} transcript labels"
    );
}

fn scroll(harness: &mut Harness<'_, State>, delta: f32) {
    harness.hover_at(egui::pos2(320.0, 240.0));
    // A complete trackpad gesture avoids wheel smoothing and its timer.
    for (phase, amount) in [
        (egui::TouchPhase::Start, 0.0),
        (egui::TouchPhase::Move, delta),
        (egui::TouchPhase::End, 0.0),
    ] {
        harness.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, amount),
            phase,
            modifiers: egui::Modifiers::NONE,
        });
        harness.step();
    }
    harness.run();
}

#[test]
fn two_thousand_entries_render_only_the_viewport_and_keep_following_the_tail() {
    let mut harness = harness(transcript(2_000), Some(true));
    assert_bounded(&harness);
    let before = assert_on_screen(&harness, "You: Entry 1999");

    // A full layout is the independent geometry reference for cached spacers.
    harness.state_mut().force = Some(false);
    harness.run();
    assert_eq!(
        harness.query_all_by_label_contains("You: Entry ").count(),
        2_000
    );
    assert_eq!(before, assert_on_screen(&harness, "You: Entry 1999"));
    harness.state_mut().force = Some(true);
    harness.run();
    assert_bounded(&harness);
    assert_eq!(before, assert_on_screen(&harness, "You: Entry 1999"));

    // First grow the content, then evict its oldest row on the next append.
    harness.state_mut().model.push_user_message("Entry 2000");
    harness.run();
    assert_on_screen(&harness, "You: Entry 2000");
    assert_bounded(&harness);
    harness.state_mut().model.push_user_message("Entry 2001");
    harness.run();
    assert_on_screen(&harness, "You: Entry 2001");
    assert_bounded(&harness);
}

#[test]
fn accesskit_keeps_the_complete_transcript_without_an_override() {
    let harness = harness(transcript(200), None);
    assert_eq!(
        harness.query_all_by_label_contains("You: Entry ").count(),
        200
    );
    assert_on_screen(&harness, "You: Entry 199");
}

#[test]
fn scrolling_up_reveals_history_and_does_not_resume_tail_following_on_append() {
    let mut harness = harness(transcript(2_000), Some(true));
    scroll(&mut harness, 1_000_000.0);
    let before = assert_on_screen(&harness, "You: Entry 0");
    assert_bounded(&harness);
    assert!(harness.query_by_label("You: Entry 1999").is_none());

    harness.state_mut().model.push_user_message("Entry 2000");
    harness.run();
    assert_on_screen(&harness, "You: Entry 0");
    assert!(harness.query_by_label("You: Entry 2000").is_none());
    assert_bounded(&harness);
    assert_eq!(
        before.top(),
        harness.get_by_label("You: Entry 0").rect().top()
    );
}

#[test]
fn width_changes_remeasure_wrapped_history_without_spacer_drift() {
    let mut model = TranscriptModel::new();
    for index in 0..200 {
        model.push_user_message(format!("Entry {index}: {}", "wrapped text ".repeat(12)));
    }
    let mut harness = harness(model, Some(true));
    harness.set_size(egui::vec2(360.0, 480.0));
    harness.run();
    scroll(&mut harness, 6_000.0);

    // Compare every rendered label's position with a full layout at the same
    // offset, away from the bottom where sticky scrolling could hide drift.
    let rects: Vec<_> = (0..200)
        .filter_map(|index| {
            let label = format!("You: Entry {index}: {}", "wrapped text ".repeat(12));
            let rect = harness.query_by_label(&label).map(|node| node.rect());
            rect.map(|rect| (label, rect))
        })
        .collect();
    assert!(!rects.is_empty());
    assert!(rects.len() <= 60);
    harness.state_mut().force = Some(false);
    harness.run();
    for (label, rect) in rects {
        assert_eq!(rect, harness.get_by_label(&label).rect(), "{label}");
    }
}

#[test]
fn visible_streaming_messages_are_remeasured_as_they_grow() {
    let mut model = transcript(2_000);
    model.apply(&Event::new(MessageEvent::MessageDelta {
        run_id: Some("stream".into()),
        delta: "Streaming start".into(),
    }));
    let mut harness = harness(model, Some(true));
    assert_on_screen(&harness, "Streaming start");
    harness
        .state_mut()
        .model
        .apply(&Event::new(MessageEvent::MessageDelta {
            run_id: Some("stream".into()),
            delta: "\n\nStreaming end".into(),
        }));
    harness.run();
    assert_on_screen(&harness, "Streaming end");
    assert_bounded(&harness);
}

#[test]
fn expanded_tool_keeps_its_state_after_offscreen_rows_are_skipped() {
    let mut model = transcript(2_000);
    model.apply(&Event::new(ToolEvent::ToolStarted {
        tool_name: "read".into(),
        call_id: "read-tail".into(),
        input: Some(serde_json::json!({"path": "tail.rs"})),
        run_id: Some("tools".into()),
        parent_call_id: None,
    }));
    model.apply(&Event::new(ToolEvent::ToolCompleted {
        tool_name: "read".into(),
        call_id: "read-tail".into(),
        is_error: false,
        output: Some("tail contents".into()),
        detail: None,
        run_id: Some("tools".into()),
        parent_call_id: None,
    }));
    let mut harness = harness(model, Some(true));
    harness.get_by_label_contains("Read tail.rs").click();
    harness.run();
    assert_on_screen(&harness, "tail contents");
    scroll(&mut harness, 1_000_000.0);
    assert!(harness.query_by_label("tail contents").is_none());
    scroll(&mut harness, -1_000_000.0);
    assert_on_screen(&harness, "tail contents");
    assert_bounded(&harness);
}

#[test]
fn changing_runs_keeps_height_caches_and_scroll_positions_separate() {
    fn run_transcript(run: &str, body: &str) -> TranscriptModel {
        let mut model = TranscriptModel::new();
        model.apply(&Event::new(MessageEvent::MessageDelta {
            run_id: Some(run.into()),
            delta: format!("{run} start"),
        }));
        for index in 0..2_000 {
            model.push_user_message(format!("Entry {index}: {body}"));
        }
        model
    }
    let mut harness = harness(run_transcript("run-a", "short"), Some(true));
    scroll(&mut harness, 1_000_000.0);
    assert_on_screen(&harness, "run-a start");

    let first = std::mem::replace(
        &mut harness.state_mut().model,
        run_transcript("run-b", "taller\nrow"),
    );
    harness.run();
    assert_on_screen(&harness, "You: Entry 1999: taller\nrow");
    assert!(harness.query_by_label("run-a start").is_none());
    assert_bounded(&harness);

    harness.state_mut().model = first;
    harness.run();
    assert_on_screen(&harness, "run-a start");
    assert!(harness.query_by_label("You: Entry 1999: short").is_none());
}

#[test]
fn skipped_poll_groups_stay_folded_and_run_js_children_stay_nested() {
    fn complete(model: &mut TranscriptModel, id: &str, name: &str, parent: Option<&str>) {
        model.apply(&Event::new(ToolEvent::ToolStarted {
            tool_name: name.into(),
            call_id: id.into(),
            input: Some(match name {
                "run_js" => serde_json::json!({"code": "return await read({path: 'tail.rs'});"}),
                "read" => serde_json::json!({"path": "tail.rs"}),
                _ => serde_json::json!({}),
            }),
            run_id: Some("tools".into()),
            parent_call_id: parent.map(str::to_owned),
        }));
        model.apply(&Event::new(ToolEvent::ToolCompleted {
            tool_name: name.into(),
            call_id: id.into(),
            is_error: false,
            output: Some("[]".into()),
            detail: None,
            run_id: Some("tools".into()),
            parent_call_id: parent.map(str::to_owned),
        }));
    }

    let mut model = TranscriptModel::new();
    for id in ["poll-a", "poll-b", "poll-c"] {
        complete(&mut model, id, "inbox", None);
    }
    for index in 0..2_000 {
        model.push_user_message(format!("Entry {index}"));
    }
    complete(&mut model, "js", "run_js", None);
    complete(&mut model, "js/read", "read", Some("js"));
    let mut harness = harness(model, Some(true));
    assert_bounded(&harness);
    harness.get_by_label_contains("Run JS").click();
    harness.run();
    assert_eq!(
        harness.query_all_by_label_contains("Read tail.rs").count(),
        1
    );
    assert!(harness.query_by_label_contains("Read inbox").is_none());
    let before = harness.get_by_label_contains("Run JS").rect();

    harness.state_mut().force = Some(false);
    harness.run();
    assert_eq!(before, harness.get_by_label_contains("Run JS").rect());
    assert_eq!(
        harness.query_all_by_label_contains("Read tail.rs").count(),
        1
    );
    scroll(&mut harness, 1_000_000.0);
    assert_eq!(harness.query_all_by_label_contains("Read inbox").count(), 1);
    let first_message = harness.get_by_label("You: Entry 0").rect();
    harness.state_mut().force = Some(true);
    harness.run();
    assert_eq!(harness.query_all_by_label_contains("Read inbox").count(), 1);
    assert_eq!(first_message, harness.get_by_label("You: Entry 0").rect());
    assert_bounded(&harness);
}
