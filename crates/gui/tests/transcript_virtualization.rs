//! Geometry and widget-count contracts, not wall-clock performance thresholds.

use egui_kittest::{Harness, kittest::Queryable};
use event_bus::{Event, MessageEvent, ToolEvent};
use gui::model::transcript::TranscriptModel;
use gui::panes::agent::{set_transcript_virtualization_for_tests, transcript_body};

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
    let mut harness = Harness::builder()
        .with_size(egui::vec2(640.0, 480.0))
        .build_ui_state(
            |ui, state| {
                gui::theme::install(ui.ctx());
                if let Some(force) = state.force {
                    set_transcript_virtualization_for_tests(ui.ctx(), force);
                }
                transcript_body(ui, &state.model);
            },
            State { model, force },
        );
    harness.run();
    harness
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
