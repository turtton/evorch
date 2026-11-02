//! A call that has not finished shows what it was given: the prompt of a
//! delegate and the command of a shell over several lines, redacted, readable
//! while it is composed and while it runs.

use std::cell::Cell;
use std::rc::Rc;

use egui_kittest::{Harness, kittest::Queryable};
use event_bus::{Event, MessageEvent, ToolEvent};
use gui::model::transcript::{ToolStatus, TranscriptEntry, TranscriptModel};
use serde_json::json;

const RUN: &str = "run-1";
const MORE: &str = "Select the header to read all of it";

struct State {
    entry: TranscriptEntry,
    height: Rc<Cell<f32>>,
}

fn card(entry: TranscriptEntry) -> (Harness<'static, State>, Rc<Cell<f32>>) {
    let height = Rc::new(Cell::new(0.0));
    let state = State {
        entry,
        height: height.clone(),
    };
    let mut harness = Harness::builder()
        .with_size(egui::vec2(640.0, 900.0))
        .build_ui_state(
            |ui, state: &mut State| {
                gui::theme::install(ui.ctx());
                ui.ctx()
                    .global_style_mut(|style| style.interaction.tooltip_delay = 0.0);
                let top = ui.cursor().top();
                gui::panes::transcript_tool::tool_card(ui, &state.entry, egui::Id::new("card"));
                state.height.set(ui.cursor().top() - top);
            },
            state,
        );
    harness.run_steps(3);
    (harness, height)
}

fn draft(name: &str, call: &str, arguments: &str) -> Event {
    Event::new(MessageEvent::ToolCallDelta {
        run_id: Some(RUN.into()),
        index: 0,
        call_id: Some(call.into()),
        tool_name: Some(name.into()),
        arguments_delta: arguments.into(),
    })
}

fn started(name: &str, call: &str, input: serde_json::Value) -> Event {
    Event::new(ToolEvent::ToolStarted {
        tool_name: name.into(),
        call_id: call.into(),
        input: Some(input),
        run_id: Some(RUN.into()),
        parent_call_id: None,
    })
}

fn only_entry(model: &TranscriptModel) -> TranscriptEntry {
    assert_eq!(model.entries().len(), 1);
    model.entries()[0].clone()
}

fn numbered_lines(count: usize) -> String {
    (0..count)
        .map(|n| format!("Step {n:03}: do the next thing"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The prompt as a provider streams it: `{"name":..., "prompt":"<escaped>"}`.
fn delegate_arguments(prompt: &str) -> String {
    format!(
        "{{\"name\":\"worker-w1\",\"prompt\":{}}}",
        serde_json::to_string(prompt).unwrap()
    )
}

#[test]
fn a_composing_delegate_shows_its_multi_line_prompt_and_expands_inside_a_bounded_height() {
    // Given: a delegate whose long prompt is still streaming in.
    let prompt = numbered_lines(500);
    let mut model = TranscriptModel::new();
    let arguments = delegate_arguments(&prompt);
    let (first, second) = arguments.split_at(arguments.len() / 2);
    model.apply(&draft("delegate", "call-1", first));
    model.apply(&draft("delegate", "call-1", second));
    let (mut harness, height) = card(only_entry(&model));

    // Then: the header stays a short identifier, and the prompt is readable below it.
    harness.get_by_label_contains("Delegate · composing…");
    harness.get_by_label_contains("Step 000: do the next thing\nStep 001");
    harness.get_by_label(MORE);
    let collapsed = height.get();

    // When: the reader expands the still-composing card.
    harness
        .get_by_label_contains("Delegate · composing…")
        .click();
    harness.run_steps(3);

    // Then: it grows, to a bounded height, and no longer asks to be expanded.
    assert!(harness.query_by_label(MORE).is_none());
    let expanded = height.get();
    assert!(expanded > collapsed, "{collapsed} -> {expanded}");
    assert!(expanded < 420.0, "expanded card is {expanded}px tall");
}

#[test]
fn an_accepted_delegate_keeps_its_name_header_and_shows_the_whole_prompt() {
    // Given: a delegate the runtime started, named and with a multi-line prompt.
    let prompt = "Review the change.\n\n1. Read the diff\n2. Run the tests\nReport findings.";
    let mut model = TranscriptModel::new();
    model.apply(&started(
        "delegate",
        "call-1",
        json!({"name": "worker-w1", "prompt": prompt, "target": {"role": "worker"}}),
    ));
    let (harness, _) = card(only_entry(&model));

    // Then: the header names the agent and the prompt is still shown.
    harness.get_by_label_contains("Delegate worker-w1 (worker)");
    harness.get_by_label_contains(prompt);
    assert!(
        harness.query_by_label(MORE).is_none(),
        "short enough to read"
    );
}

#[test]
fn a_running_shell_command_keeps_its_line_breaks() {
    // Given: a running multi-line command.
    let command = "cd crates/gui &&\n  cargo test --locked \\\n  -- --nocapture";
    let mut model = TranscriptModel::new();
    model.apply(&started("shell", "call-1", json!({"command": command})));
    let (harness, _) = card(only_entry(&model));

    // Then: the header is one line and the detail has the command as written.
    harness.get_by_label_contains("Shell cd crates/gui && cargo test --locked \\ -- --nocapture");
    harness.get_by_label(command);
}

#[test]
fn a_running_card_can_be_expanded_to_read_a_long_command() {
    // Given: a running command taller than the collapsed view.
    let command = numbered_lines(60);
    let mut model = TranscriptModel::new();
    model.apply(&started("shell", "call-1", json!({"command": command})));
    let (mut harness, height) = card(only_entry(&model));
    harness.get_by_label(MORE);
    let collapsed = height.get();

    // When: it is expanded and collapsed again.
    harness.get_by_label_contains("Shell Step 000").click();
    harness.run_steps(3);
    let expanded = height.get();
    assert!(harness.query_by_label(MORE).is_none());
    assert!(expanded > collapsed && expanded < 420.0, "{expanded}");
    harness.get_by_label_contains("Shell Step 000").click();
    harness.run_steps(3);

    // Then: expansion only changed the display.
    harness.get_by_label(MORE);
    assert!((height.get() - collapsed).abs() < 1.0);
}

#[test]
fn a_running_card_with_nothing_more_to_read_stays_inert() {
    let mut model = TranscriptModel::new();
    model.apply(&started(
        "shell",
        "call-1",
        json!({"command": "cargo test"}),
    ));
    let (mut harness, height) = card(only_entry(&model));
    let before = height.get();
    harness.get_by_label_contains("Shell cargo test").click();
    harness.run_steps(3);
    assert!((height.get() - before).abs() < 1.0);
    assert!(harness.query_by_label(MORE).is_none());
}

#[test]
fn a_running_call_never_displays_a_credential_but_keeps_its_input() {
    // Given: a running call whose accepted input carries a token.
    const BODY: &str = "Zx9Qw3Lm7Tb2Vc5Nk8Rd4Hf6Jg1Ps0YaEu";
    let command = format!("curl -H 'Authorization: ghp_{BODY}'\nunset TOKEN");
    let mut model = TranscriptModel::new();
    model.apply(&started("shell", "call-1", json!({ "command": command })));
    let entry = only_entry(&model);
    let (mut harness, _) = card(entry.clone());

    // When: the card is read, including the tooltip of its header.
    harness.get_by_role(egui::accesskit::Role::Button).hover();
    harness.run_steps(3);

    // Then: no label, tooltip or accessible name has any stretch of the token.
    for window in BODY.as_bytes().windows(6) {
        let fragment = std::str::from_utf8(window).unwrap();
        assert!(
            harness.query_by_label_contains(fragment).is_none(),
            "{fragment} is displayed"
        );
    }
    assert!(
        harness
            .query_all_by_label_contains("[REDACTED:github-token]")
            .count()
            >= 1
    );

    // And: the recorded input, which the runtime executes, is untouched.
    assert!(matches!(
        &entry,
        TranscriptEntry::Tool { input: Some(input), .. } if input["command"] == command
    ));
}

#[test]
fn a_card_expanded_while_composing_stays_expanded_once_the_call_starts() {
    // Given: a long prompt being composed and expanded by the reader.
    let prompt = numbered_lines(120);
    let mut model = TranscriptModel::new();
    model.apply(&draft("delegate", "call-1", &delegate_arguments(&prompt)));
    let (mut harness, height) = card(only_entry(&model));
    harness.get_by_label(MORE);
    harness
        .get_by_label_contains("Delegate · composing…")
        .click();
    harness.run_steps(3);
    assert!(harness.query_by_label(MORE).is_none());
    let expanded = height.get();

    // When: the runtime starts the call, in the same entry.
    model.apply(&started(
        "delegate",
        "call-1",
        json!({"name": "worker-w1", "prompt": prompt}),
    ));
    let running = only_entry(&model);
    assert!(matches!(
        &running,
        TranscriptEntry::Tool {
            status: ToolStatus::Running,
            ..
        }
    ));
    harness.state_mut().entry = running;
    harness.run_steps(3);

    // Then: it is still expanded, in a bounded height, now showing the accepted input.
    assert!(harness.query_by_label(MORE).is_none());
    harness.get_by_label_contains("Delegate worker-w1");
    assert!(height.get() < 420.0 && height.get() > 100.0, "{expanded}");
}
