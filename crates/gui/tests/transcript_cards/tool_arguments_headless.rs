//! A call that has not finished shows what it was given: the prompt of a
//! delegate and the command of a shell over several lines, redacted, readable
//! while it is composed and while it runs.

use std::cell::Cell;
use std::rc::Rc;

use egui_kittest::{Harness, kittest::Queryable};
use event_bus::{Event, MessageEvent, ToolEvent};
use gui::model::transcript::shell_jobs::ShellJobs;
use gui::model::transcript::{ToolStatus, TranscriptEntry, TranscriptModel};
use gui::panes::transcript_tool::ToolCardContext;
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
                // The transcript names a card by its entry, not by its call.
                let jobs = ShellJobs::default();
                gui::panes::transcript_tool::tool_card_group(
                    ui,
                    std::slice::from_ref(&state.entry),
                    egui::Id::new("card"),
                    ToolCardContext {
                        repo_root: None,
                        jobs: &jobs,
                        latest_for_job: false,
                        agents: &[],
                    },
                );
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
fn a_short_running_command_is_kept_behind_the_header_until_it_is_expanded() {
    // Given: a one-line command that the header shows whole.
    let mut model = TranscriptModel::new();
    model.apply(&started(
        "shell",
        "call-1",
        json!({"command": "cargo test"}),
    ));
    let (mut harness, _) = card(only_entry(&model));
    assert_eq!(
        harness.query_all_by_label_contains("cargo test").count(),
        1,
        "not repeated below the header"
    );

    // When: it is expanded, e.g. because a narrow pane cut the header.
    harness.get_by_label_contains("Shell cargo test").click();
    harness.run_steps(3);

    // Then: the command is available in full below the header.
    assert_eq!(harness.query_all_by_label_contains("cargo test").count(), 2);
    assert!(harness.query_by_label(MORE).is_none());
}

#[test]
fn a_running_write_shows_a_short_body_the_header_cannot_read() {
    // Given: a running write whose arguments are short but whose header names only the path.
    let mut model = TranscriptModel::new();
    model.apply(&started(
        "write",
        "call-1",
        json!({"path": "a.txt", "content": "first\nsecond"}),
    ));
    let (harness, _) = card(only_entry(&model));

    // Then: the body is readable while it runs, and the path is not repeated as detail.
    harness.get_by_label_contains("first\\nsecond");
    let mut model = TranscriptModel::new();
    model.apply(&started("read", "call-2", json!({"path": "a.txt"})));
    let (harness, _) = card(only_entry(&model));
    assert!(harness.query_by_label_contains("\"path\"").is_none());
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

fn completed(name: &str, call: &str, is_error: bool) -> Event {
    Event::new(ToolEvent::ToolCompleted {
        tool_name: name.into(),
        call_id: call.into(),
        is_error,
        output: Some("done".into()),
        detail: None,
        run_id: Some(RUN.into()),
        parent_call_id: None,
    })
}

fn assert_hides_token(harness: &Harness<'_, State>, body: &str, context: &str) {
    for window in body.as_bytes().windows(6) {
        let fragment = std::str::from_utf8(window).unwrap();
        assert!(
            harness.query_by_label_contains(fragment).is_none(),
            "{context}: {fragment} is displayed"
        );
    }
}

const BODY: &str = "Zx9Qw3Lm7Tb2Vc5Nk8Rd4Hf6Jg1Ps0YaEu";

#[test]
fn a_credential_stays_hidden_when_the_same_card_finishes() {
    // Given: a call with a token in its input, shown while it runs.
    let command = format!("curl -H 'Authorization: ghp_{BODY}'\nunset TOKEN");
    let mut model = TranscriptModel::new();
    model.apply(&started("shell", "call-1", json!({ "command": command })));
    let (mut harness, _) = card(only_entry(&model));
    assert_hides_token(&harness, BODY, "running");

    // When: the same card completes, and is expanded and hovered.
    model.apply(&completed("shell", "call-1", false));
    harness.state_mut().entry = only_entry(&model);
    harness.run_steps(3);
    harness.get_by_role(egui::accesskit::Role::Button).click();
    harness.run_steps(3);
    harness.get_by_role(egui::accesskit::Role::Button).hover();
    harness.run_steps(3);

    // Then: the token is not displayed in any label, tooltip or section, and the
    // input the runtime holds is still the original.
    assert_hides_token(&harness, BODY, "succeeded");
    assert!(harness.query_by_label_contains("Input").is_some());
    assert!(matches!(
        &harness.state().entry,
        TranscriptEntry::Tool { input: Some(input), .. } if input["command"] == command
    ));
}

#[test]
fn a_run_js_program_never_displays_a_credential_running_or_finished() {
    // Given: a run_js program that contains a token.
    let code = format!("const key = 'ghp_{BODY}';\nawait tools.read({{path: 'a'}});");
    let mut model = TranscriptModel::new();
    model.apply(&started("run_js", "js-1", json!({ "code": code })));
    let (mut harness, _) = card(only_entry(&model));

    // When/Then: running, hovered, expanded, then finished and expanded.
    harness.get_by_role(egui::accesskit::Role::Button).hover();
    harness.run_steps(3);
    assert_hides_token(&harness, BODY, "running");
    harness.get_by_role(egui::accesskit::Role::Button).click();
    harness.run_steps(3);
    harness.get_by_label("Code");
    assert_hides_token(&harness, BODY, "running, expanded");
    model.apply(&completed("run_js", "js-1", false));
    harness.state_mut().entry = only_entry(&model);
    harness.run_steps(3);
    harness.get_by_role(egui::accesskit::Role::Button).hover();
    harness.run_steps(3);
    assert_hides_token(&harness, BODY, "finished, expanded");
    assert!(matches!(
        &harness.state().entry,
        TranscriptEntry::Tool { input: Some(input), .. } if input["code"] == code
    ));
}

fn scroll_state(harness: &Harness<'_, State>) -> (f32, f32) {
    gui::panes::transcript_tool::live_detail_scroll_for_tests(&harness.ctx, egui::Id::new("card"))
        .expect("the detail area was drawn")
}

fn wheel_over_detail(harness: &mut Harness<'_, State>, header: &str, delta: f32) {
    let rect = harness.get_by_label_contains(header).rect();
    let over = egui::pos2(rect.center().x, rect.bottom() + 60.0);
    harness.event(egui::Event::PointerMoved(over));
    harness.run_steps(2);
    harness.event(egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Point,
        delta: egui::vec2(0.0, delta),
        phase: egui::TouchPhase::Move,
        modifiers: egui::Modifiers::NONE,
    });
    harness.run_steps(3);
}

#[test]
fn a_reader_who_scrolled_up_stays_put_and_state_survives_a_call_without_an_id() {
    // Given: a draft whose call ID the provider never sent, long enough to scroll.
    let lines: Vec<String> = (0..300)
        .map(|n| format!("Step {n:03}: do the next thing"))
        .collect();
    let open_prompt = |count: usize| {
        let mut arguments = delegate_arguments(&lines[..count].join("\n"));
        arguments.truncate(arguments.len() - 2);
        arguments
    };
    let mut model = TranscriptModel::new();
    model.apply(&Event::new(MessageEvent::ToolCallDelta {
        run_id: Some(RUN.into()),
        index: 0,
        call_id: None,
        tool_name: Some("delegate".into()),
        arguments_delta: open_prompt(250),
    }));
    let (mut harness, _) = card(only_entry(&model));
    harness
        .get_by_label_contains("Delegate · composing…")
        .click();
    harness.run_steps(3);

    // Then: expanded, it follows the newest text.
    let (offset, furthest) = scroll_state(&harness);
    assert!(
        furthest > 100.0 && (furthest - offset).abs() < 2.0,
        "{offset}/{furthest}"
    );

    // When: the reader scrolls up and more text arrives.
    wheel_over_detail(&mut harness, "Delegate · composing…", 200.0);
    let (read_at, _) = scroll_state(&harness);
    assert!(read_at < offset - 50.0, "{read_at} vs {offset}");
    let more = &open_prompt(300)[open_prompt(250).len()..];
    model.apply(&Event::new(MessageEvent::ToolCallDelta {
        run_id: Some(RUN.into()),
        index: 0,
        call_id: None,
        tool_name: None,
        arguments_delta: more.into(),
    }));
    harness.state_mut().entry = only_entry(&model);
    harness.run_steps(3);

    // Then: the view was not moved to the end.
    let (now, furthest_now) = scroll_state(&harness);
    assert!((now - read_at).abs() < 2.0, "{now} vs {read_at}");
    assert!(furthest_now > furthest);

    // And: when the runtime starts the call under its own ID, the card is the
    // same: still expanded and still where the reader left it.
    model.apply(&started(
        "delegate",
        "runtime-call",
        json!({"name": "worker-w1", "prompt": lines.join("\n")}),
    ));
    harness.state_mut().entry = only_entry(&model);
    harness.run_steps(3);
    assert!(harness.query_by_label(MORE).is_none(), "still expanded");
    let (started_at, _) = scroll_state(&harness);
    assert!((started_at - now).abs() < 2.0, "{started_at} vs {now}");
}

#[test]
fn a_body_that_ends_like_a_path_is_still_shown_for_a_running_write() {
    // Given: a write whose body is a URL ending in the file's own name.
    let mut model = TranscriptModel::new();
    model.apply(&started(
        "write",
        "call-1",
        json!({"path": "out.txt", "content": "https://example.invalid/out.txt"}),
    ));
    let (harness, _) = card(only_entry(&model));

    // Then: the body is readable, not mistaken for the path in the header.
    harness.get_by_label_contains("https://example.invalid/out.txt");
}

struct Pane {
    models: Vec<TranscriptModel>,
    shown: usize,
}

fn started_in(run: &str, name: &str, input: serde_json::Value) -> Event {
    Event::new(ToolEvent::ToolStarted {
        tool_name: name.into(),
        call_id: "call-1".into(),
        input: Some(input),
        run_id: Some(run.into()),
        parent_call_id: None,
    })
}

/// One pane that shows a different transcript at a time, like switching runs.
fn pane(models: Vec<TranscriptModel>) -> Harness<'static, Pane> {
    let mut harness = Harness::builder()
        .with_size(egui::vec2(640.0, 900.0))
        .build_ui_state(
            |ui, pane: &mut Pane| {
                gui::theme::install(ui.ctx());
                gui::panes::agent::transcript_body(ui, &pane.models[pane.shown]);
            },
            Pane { models, shown: 0 },
        );
    harness.run_steps(3);
    harness
}

fn switch_to(harness: &mut Harness<'_, Pane>, shown: usize) {
    harness.state_mut().shown = shown;
    harness.run_steps(3);
}

#[test]
fn a_card_keeps_its_state_per_transcript_when_one_pane_switches_between_them() {
    // Given: two transcripts whose first entries both have entry ID 0 and carry
    // a call taller than the collapsed view, and one of them is a run_js call.
    let command = numbered_lines(60);
    let program = numbered_lines(60);
    for (kind, first, second, expanded_marker) in [
        (
            "tool",
            started_in("run-a", "shell", json!({ "command": command })),
            started_in("run-b", "shell", json!({ "command": command })),
            None,
        ),
        (
            "run_js",
            started_in("run-a", "run_js", json!({ "code": program })),
            started_in("run-b", "run_js", json!({ "code": program })),
            Some("Code"),
        ),
    ] {
        let mut a = TranscriptModel::new();
        a.apply(&first);
        let mut b = TranscriptModel::new();
        b.apply(&second);
        let mut harness = pane(vec![a, b]);
        let is_open = |harness: &Harness<'_, Pane>| match expanded_marker {
            Some(marker) => harness.query_by_label(marker).is_some(),
            None => harness.query_by_label(MORE).is_none(),
        };
        assert!(!is_open(&harness), "{kind}: starts collapsed");

        // When: A's card is expanded, then the pane shows B and then A again.
        harness
            .get_by_label_contains(if kind == "tool" {
                "Shell Step 000"
            } else {
                "Run JS"
            })
            .click();
        harness.run_steps(3);
        assert!(is_open(&harness), "{kind}: A expanded");
        switch_to(&mut harness, 1);

        // Then: B's card has its own, collapsed state; A's is still expanded.
        assert!(!is_open(&harness), "{kind}: B is independent of A");
        switch_to(&mut harness, 0);
        assert!(is_open(&harness), "{kind}: A kept its state");
    }
}
