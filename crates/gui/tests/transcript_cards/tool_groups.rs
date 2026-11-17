//! Tool calls and reasoning fold under one header once the model has said
//! something to the reader. The fold is display only and always derived from
//! the current entries plus the reader's own toggles.

use std::collections::BTreeMap;

use egui_kittest::{Harness, kittest::Queryable};
use event_bus::{Event, MessageEvent, ToolEvent};
use gui::model::pull_requests::{CandidateChip, ChipState, PullRequestAction};
use gui::model::transcript::TranscriptModel;
use gui::panes::agent::{
    set_transcript_perf_enabled, set_transcript_virtualization_for_tests, take_transcript_perf,
    transcript_body, transcript_body_with_chips_for_tests, transcript_layout_for_tests,
};
use gui::panes::pull_request_chip::{ChipsByCall, PullRequestChips};

struct State {
    model: TranscriptModel,
    force: Option<bool>,
}

fn harness(model: TranscriptModel) -> Harness<'static, State> {
    let mut harness = Harness::builder()
        .with_size(egui::vec2(640.0, 480.0))
        .build_ui_state(
            |ui, state: &mut State| {
                gui::theme::install(ui.ctx());
                if let Some(force) = state.force {
                    set_transcript_virtualization_for_tests(ui.ctx(), force);
                }
                transcript_body(ui, &state.model);
            },
            State { model, force: None },
        );
    harness.run_steps(3);
    harness
}

fn tool_start(model: &mut TranscriptModel, run: &str, call: &str, name: &str, input: &str) {
    model.apply(&Event::new(ToolEvent::ToolStarted {
        tool_name: name.into(),
        call_id: call.into(),
        input: Some(match name {
            "read" => serde_json::json!({ "path": input }),
            _ => serde_json::json!({}),
        }),
        run_id: Some(run.into()),
        parent_call_id: None,
    }));
}

fn tool_done(model: &mut TranscriptModel, run: &str, call: &str, name: &str, output: &str) {
    model.apply(&Event::new(ToolEvent::ToolCompleted {
        tool_name: name.into(),
        call_id: call.into(),
        is_error: false,
        output: Some(output.into()),
        detail: None,
        run_id: Some(run.into()),
        parent_call_id: None,
    }));
}

fn read(model: &mut TranscriptModel, run: &str, call: &str, path: &str) {
    tool_start(model, run, call, "read", path);
    tool_done(model, run, call, "read", "contents");
}

fn reasoning(model: &mut TranscriptModel, run: &str, text: &str) {
    model.apply(&Event::new(MessageEvent::ReasoningDelta {
        delta: text.into(),
        run_id: Some(run.into()),
    }));
}

fn say(model: &mut TranscriptModel, run: &str, text: &str) {
    model.apply(&Event::new(MessageEvent::MessageDelta {
        run_id: Some(run.into()),
        delta: text.into(),
    }));
}

fn say_completed(model: &mut TranscriptModel, run: &str, text: &str) {
    model.apply(&Event::new(MessageEvent::MessageCompleted {
        run_id: run.into(),
        text: text.into(),
    }));
}

fn shown(harness: &Harness<'_, State>, label: &str) -> usize {
    harness.query_all_by_label_contains(label).count()
}

/// Tool, reasoning, tool, then the answer, as a live turn produces them.
fn finished_turn(run: &str) -> TranscriptModel {
    let mut model = TranscriptModel::new();
    read(&mut model, run, "c1", "a.rs");
    reasoning(&mut model, run, "thinking it over");
    read(&mut model, run, "c2", "b.rs");
    say(&mut model, run, "All done.");
    model
}

#[test]
fn a_finished_turn_folds_into_one_closed_group_that_the_reader_toggles() {
    // Given: two tool calls with reasoning between them, then the answer.
    let mut harness = harness(finished_turn("run-1"));

    // Then: one closed header stands in for the cards; the answer stays visible.
    assert_eq!(shown(&harness, "2 tool calls"), 1);
    assert_eq!(shown(&harness, "Read a.rs"), 0);
    assert_eq!(shown(&harness, "Read b.rs"), 0);
    assert_eq!(shown(&harness, "All done."), 1);

    // When: the reader opens it.
    harness.get_by_label_contains("2 tool calls").click();
    harness.run_steps(3);

    // Then: the ordinary cards appear, once each, and the header stays.
    assert_eq!(shown(&harness, "Read a.rs"), 1);
    assert_eq!(shown(&harness, "Read b.rs"), 1);
    assert_eq!(shown(&harness, "2 tool calls"), 1);

    // When: the reader closes it again.
    harness.get_by_label_contains("2 tool calls").click();
    harness.run_steps(3);
    assert_eq!(shown(&harness, "Read a.rs"), 0);
    assert_eq!(shown(&harness, "All done."), 1);
}

#[test]
fn nothing_folds_until_the_model_has_text_for_the_reader() {
    // Given: tool calls and reasoning with no answer yet.
    let mut model = TranscriptModel::new();
    read(&mut model, "run-1", "c1", "a.rs");
    reasoning(&mut model, "run-1", "weighing options");
    let mut harness = harness(model);
    assert_eq!(shown(&harness, "Read a.rs"), 1);
    assert_eq!(shown(&harness, "tool call"), 0);

    // When: the first text arrives, while it is still streaming.
    say(&mut harness.state_mut().model, "run-1", "Starting");
    harness.run_steps(3);

    // Then: the segment folds at once.
    assert_eq!(shown(&harness, "1 tool call"), 1);
    assert_eq!(shown(&harness, "Read a.rs"), 0);
}

#[test]
fn reasoning_alone_and_a_boundary_never_form_or_extend_a_group() {
    // Given: reasoning then an answer (no tool card), and a tool call cut off
    // from a later answer by a notice.
    let mut model = TranscriptModel::new();
    reasoning(&mut model, "run-1", "just thinking");
    say(&mut model, "run-1", "Plain answer");
    read(&mut model, "run-1", "c1", "a.rs");
    model.push_notice("compaction notice".to_owned());
    say(&mut model, "run-1", "Late answer");
    let harness = harness(model);

    // Then: no header, and the call before the notice is still a card.
    assert_eq!(shown(&harness, "tool call"), 0);
    assert_eq!(shown(&harness, "Read a.rs"), 1);
}

#[test]
fn a_running_call_keeps_its_segment_open() {
    // Given: a finished call and one still running when text appears.
    let mut model = TranscriptModel::new();
    read(&mut model, "run-1", "c1", "a.rs");
    tool_start(&mut model, "run-1", "c2", "read", "b.rs");
    say(&mut model, "run-1", "Working on it");
    let mut harness = harness(model);
    assert_eq!(shown(&harness, "tool call"), 0);
    assert_eq!(shown(&harness, "Read a.rs"), 1);

    // When: the call completes.
    tool_done(&mut harness.state_mut().model, "run-1", "c2", "read", "ok");
    harness.run_steps(3);

    // Then: the whole segment folds.
    assert_eq!(shown(&harness, "2 tool calls"), 1);
    assert_eq!(shown(&harness, "Read a.rs"), 0);
}

#[test]
fn runs_with_the_same_call_id_fold_separately() {
    // Given: two runs reuse the call ID "c1" and each answers.
    let mut model = TranscriptModel::new();
    read(&mut model, "run-1", "c1", "a.rs");
    say_completed(&mut model, "run-1", "First answer");
    read(&mut model, "run-2", "c1", "b.rs");
    say_completed(&mut model, "run-2", "Second answer");
    let mut harness = harness(model);

    // Then: two headers; opening one leaves the other closed.
    assert_eq!(shown(&harness, "1 tool call"), 2);
    harness
        .query_all_by_label_contains("1 tool call")
        .next()
        .unwrap()
        .click();
    harness.run_steps(3);
    assert_eq!(
        shown(&harness, "Read a.rs") + shown(&harness, "Read b.rs"),
        1
    );
}

#[test]
fn an_opened_group_stays_open_while_the_turn_keeps_changing() {
    // Given: a folded turn that the reader opened.
    let mut harness = harness(finished_turn("run-1"));
    harness.get_by_label_contains("2 tool calls").click();
    harness.run_steps(3);
    assert_eq!(shown(&harness, "Read a.rs"), 1);

    // When: the answer streams on, the canonical text replaces it, and a
    // later call starts and completes in the next segment.
    say(&mut harness.state_mut().model, "run-1", " More text.");
    harness.run_steps(3);
    say_completed(
        &mut harness.state_mut().model,
        "run-1",
        "All done. More text.",
    );
    harness.run_steps(3);
    tool_start(
        &mut harness.state_mut().model,
        "run-1",
        "c3",
        "read",
        "c.rs",
    );
    harness.run_steps(3);
    tool_done(&mut harness.state_mut().model, "run-1", "c3", "read", "ok");
    say(&mut harness.state_mut().model, "run-1", "Next answer");
    harness.run_steps(3);

    // Then: the first group is still open and the new one is closed.
    assert_eq!(shown(&harness, "Read a.rs"), 1);
    assert_eq!(shown(&harness, "Read b.rs"), 1);
    assert_eq!(shown(&harness, "Read c.rs"), 0);
    assert_eq!(shown(&harness, "1 tool call"), 1);
}

#[test]
fn live_updates_and_a_history_replay_show_the_same_rows() {
    // Given: one conversation applied event by event with a frame between each,
    // and the same events applied before the first frame.
    let build = |frames_between: bool| {
        let mut harness = harness(TranscriptModel::new());
        type Step = fn(&mut TranscriptModel);
        let steps: [Step; 5] = [
            |m| tool_start(m, "run-1", "c1", "read", "a.rs"),
            |m| tool_done(m, "run-1", "c1", "read", "contents"),
            |m| reasoning(m, "run-1", "hmm"),
            |m| say(m, "run-1", "Answer"),
            |m| say_completed(m, "run-1", "Answer"),
        ];
        for step in steps {
            step(&mut harness.state_mut().model);
            if frames_between {
                harness.run_steps(3);
            }
        }
        harness.run_steps(3);
        harness
    };
    let live = build(true);
    let replay = build(false);

    // Then: both fold the same way.
    for label in ["1 tool call", "Read a.rs", "Answer"] {
        assert_eq!(shown(&live, label), shown(&replay, label), "{label}");
    }
    assert_eq!(shown(&live, "1 tool call"), 1);
    assert_eq!(shown(&live, "Read a.rs"), 0);
}

#[test]
fn identical_polls_inside_a_group_keep_sharing_one_card() {
    // Given: three identical inbox checks, then the answer.
    let mut model = TranscriptModel::new();
    for call in ["p1", "p2", "p3"] {
        tool_start(&mut model, "run-1", call, "inbox", "");
        tool_done(&mut model, "run-1", call, "inbox", "[]");
    }
    say(&mut model, "run-1", "Nothing new");
    let mut harness = harness(model);
    assert_eq!(shown(&harness, "3 tool calls"), 1);

    // When: opened.
    harness.get_by_label_contains("3 tool calls").click();
    harness.run_steps(3);

    // Then: they still read as the single shared card they were before.
    let folded = shown(&harness, "inbox");
    assert!(folded >= 1, "the poll card is drawn");
    assert!(folded < 3, "the polls share a card, saw {folded}");
}

#[test]
fn a_candidate_chip_stays_visible_while_its_group_is_closed() {
    // Given: a tool call that produced a pull request candidate, then text.
    let mut model = TranscriptModel::new();
    read(&mut model, "run-1", "c1", "a.rs");
    say(&mut model, "run-1", "Opened it");
    let by_call: ChipsByCall = BTreeMap::from([(
        ("run-1".to_owned(), "c1".to_owned()),
        vec![CandidateChip {
            candidate_id: 7,
            label: "turtton/evorch#21".into(),
            state: ChipState::Offered,
        }],
    )])
    .into_iter()
    .collect();
    let mut harness = Harness::builder()
        .with_size(egui::vec2(640.0, 480.0))
        .build_ui_state(
            move |ui, action: &mut Option<PullRequestAction>| {
                gui::theme::install(ui.ctx());
                let mut chips = PullRequestChips {
                    thread_id: "t1",
                    by_call: &by_call,
                    action,
                };
                transcript_body_with_chips_for_tests(ui, &model, &mut chips);
            },
            None,
        );
    harness.run_steps(3);

    // Then: the card is folded away but the offer is still actionable.
    assert_eq!(harness.query_all_by_label_contains("Read a.rs").count(), 0);
    harness.get_by_label("Ignore");
    harness.get_by_label("Link PR turtton/evorch#21").click();
    harness.run_steps(3);
    assert_eq!(
        harness.state(),
        &Some(PullRequestAction::Confirm {
            thread_id: "t1".into(),
            candidate_id: 7
        })
    );
}

#[test]
fn closed_groups_neither_draw_nor_measure_their_members() {
    // Given: 400 finished turns of three calls each, as a long history.
    let turns = 400;
    let mut model = TranscriptModel::new();
    for turn in 0..turns {
        let run = format!("run-{turn}");
        for call in 0..3 {
            read(
                &mut model,
                &run,
                &format!("c{call}"),
                &format!("f{turn}_{call}.rs"),
            );
        }
        say_completed(&mut model, &run, &format!("Answer {turn}"));
    }
    set_transcript_perf_enabled(true);
    let _ = take_transcript_perf();
    let mut harness = harness(model);
    harness.run_steps(3);
    let perf = take_transcript_perf();
    set_transcript_perf_enabled(false);

    // Then: only headers and answers are rows; the 1,200 member cards are not
    // listed, drawn or measured at all.
    assert_eq!(perf.group_headers, perf.passes * turns);
    assert_eq!(perf.rows_hidden_by_groups, perf.passes * 3 * turns);
    assert_eq!(perf.rows_listed, perf.passes * 2 * turns);
    assert!(perf.rows_drawn + perf.rows_skipped >= perf.rows_listed);
    assert_eq!(shown(&harness, "Read f"), 0);
}

#[test]
fn toggling_keeps_the_header_in_place_and_matches_the_full_layout() {
    // Given: a long history of folded turns, scrolled to the middle.
    let mut model = TranscriptModel::new();
    for turn in 0..300 {
        let run = format!("run-{turn}");
        read(&mut model, &run, "c1", &format!("f{turn}.rs"));
        say_completed(&mut model, &run, &format!("Answer {turn}"));
    }
    let mut harness = harness(model);
    harness.state_mut().force = Some(true);
    harness.run_steps(3);
    harness.hover_at(egui::pos2(320.0, 240.0));
    for amount in [0.0, 4_000.0, 0.0] {
        harness.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, amount),
            phase: if amount == 0.0 {
                egui::TouchPhase::Start
            } else {
                egui::TouchPhase::Move
            },
            modifiers: egui::Modifiers::NONE,
        });
        harness.step();
    }
    harness.run_steps(3);
    let header = harness
        .query_all_by_label_contains("1 tool call")
        .find(|node| node.rect().top() > 100.0 && node.rect().bottom() < 380.0)
        .expect("a header is on screen");
    let before = header.rect();

    // When: that header is opened.
    header.click();
    harness.run_steps(3);

    // Then: it did not move, its card appeared once, and the virtualized
    assert!(
        harness
            .query_all_by_label_contains("1 tool call")
            .any(|node| (node.rect().top() - before.top()).abs() < 1.0),
        "the opened header kept its place at {}",
        before.top()
    );
    assert_eq!(shown(&harness, "Read f"), 1);
    let virtualized = transcript_layout_for_tests(&harness.ctx).unwrap();
    harness.state_mut().force = Some(false);
    harness.run_steps(3);
    let full = transcript_layout_for_tests(&harness.ctx).unwrap();
    assert_eq!(virtualized.0.y, full.0.y, "total content height");
}

fn turns(prefix: &str, count: usize) -> TranscriptModel {
    let mut model = TranscriptModel::new();
    for turn in 0..count {
        let run = format!("{prefix}-{turn}");
        read(&mut model, &run, "c1", &format!("f{turn}.rs"));
        say_completed(&mut model, &run, &format!("Answer {turn}"));
    }
    model
}

#[test]
fn what_a_pane_kept_for_a_transcript_it_no_longer_shows_is_dropped() {
    // Given: a long history whose first group the reader opened, and rows
    // whose heights are known, so that off-screen rows are skipped.
    let mut harness = harness(turns("a", 300));
    harness.state_mut().force = Some(true);
    harness.run_steps(4);
    harness
        .query_all_by_label_contains("1 tool call")
        .filter(|node| node.rect().top() >= 0.0)
        .min_by(|a, b| a.rect().top().total_cmp(&b.rect().top()))
        .expect("a header")
        .click();
    harness.run_steps(4);
    assert!(shown(&harness, "Read f") >= 1, "the reader opened a group");
    set_transcript_perf_enabled(true);
    let _ = take_transcript_perf();
    harness.run_steps(2);
    assert!(
        take_transcript_perf().rows_skipped > 0,
        "known heights let rows off screen be skipped"
    );

    // When: the pane shows another transcript, and then the first again.
    harness.state_mut().model = turns("b", 300);
    harness.run_steps(4);
    harness.state_mut().model = turns("a", 300);
    let _ = take_transcript_perf();
    harness.step();
    let perf = take_transcript_perf();
    set_transcript_perf_enabled(false);

    // Then: neither the open toggle nor the measured heights survived.
    assert_eq!(shown(&harness, "Read f"), 0, "the group is closed again");
    assert_eq!(perf.rows_skipped, 0, "every row is measured afresh");
    assert!(perf.rows_drawn > 0, "{perf:?}");
}

#[test]
fn full_scans_of_the_entries_are_counted_as_they_happen() {
    // Given: entries with no artifacts, a pass over them.
    let model = turns("run", 10);
    let entries = model.visible_entries().len() as u64;
    set_transcript_perf_enabled(true);
    let _ = take_transcript_perf();
    let _harness = harness(model);
    let perf = take_transcript_perf();
    set_transcript_perf_enabled(false);

    // Then: each pass scanned them seven times: the artifact poll, the
    // shell-job map, run_js nesting (two scans), the grouping, the row loop
    // and the height-cache prune.
    assert!(perf.passes > 0);
    assert_eq!(perf.entries_enumerated, perf.passes * 7 * entries);
}

struct PaneState {
    model: TranscriptModel,
    composer: gui::model::composer::ComposerModel,
    picker: gui::model::model_picker::ModelPickerState,
    draw: bool,
}

/// The whole conversation pane, which shows an empty state for an empty thread.
fn pane(model: TranscriptModel) -> Harness<'static, PaneState> {
    let mut harness = Harness::builder()
        .with_size(egui::vec2(800.0, 600.0))
        .build_ui_state(
            |ui, state: &mut PaneState| {
                gui::theme::install(ui.ctx());
                if !state.draw {
                    return;
                }
                gui::panes::agent::agent_pane(
                    ui,
                    &state.model,
                    None,
                    gui::panes::agent::ConversationContext {
                        goal: None,
                        requests: None,
                        task_rows: &[],
                        phase_unread: false,
                        has_project: true,
                        active_thread_title: Some("Chat"),
                        extra_read_dirs: &[],
                        parent_thread: None,
                        child_threads: Vec::new(),
                        lane: None,
                        lead: None,
                        thread_metrics: None,
                        phase: None,
                        model_picker: gui::panes::model_picker::ModelPickerContext {
                            profiles: &[],
                            preference: None,
                            default_model: None,
                            enabled: false,
                        },
                        sandbox_picker: Default::default(),
                        branch: None,
                        pull_request: None,
                        pull_request_chips: None,
                    },
                    &mut state.composer,
                    &mut state.picker,
                );
            },
            PaneState {
                model,
                composer: Default::default(),
                picker: Default::default(),
                draw: true,
            },
        );
    harness.run_steps(3);
    harness
}

fn open_first_group(harness: &mut Harness<'_, PaneState>) {
    harness.get_by_label_contains("1 tool call").click();
    harness.run_steps(3);
    assert!(
        harness.query_all_by_label_contains("Read f0.rs").count() > 0,
        "the group is open"
    );
}

#[test]
fn a_pane_that_shows_an_empty_thread_forgets_the_open_groups_of_the_one_before() {
    // Given: a reader who opened a group of thread A.
    let mut harness = pane(turns("a", 1));
    open_first_group(&mut harness);
    assert!(gui::panes::agent::holds_transcript_view_state_for_tests(
        &harness.ctx,
        "a-0"
    ));

    // When: the pane shows an empty thread, and then A again.
    harness.state_mut().model = TranscriptModel::new();
    harness.run_steps(3);
    let kept = gui::panes::agent::holds_transcript_view_state_for_tests(&harness.ctx, "a-0");
    harness.state_mut().model = turns("a", 1);
    harness.run_steps(3);

    // Then: nothing was kept while it was empty, and A comes back closed.
    assert!(!kept, "state kept for a thread nothing shows");
    assert_eq!(harness.query_all_by_label_contains("Read f0.rs").count(), 0);
    assert!(harness.query_by_label_contains("1 tool call").is_some());
}

#[test]
fn deleting_a_thread_drops_the_open_groups_a_pane_kept_for_it() {
    // Given: a group opened in a pane that is not drawn from now on (a hidden
    // tab), so no frame of it could retire anything.
    let mut harness = pane(turns("a", 1));
    open_first_group(&mut harness);
    harness.state_mut().draw = false;
    harness.run_steps(2);
    assert!(gui::panes::agent::holds_transcript_view_state_for_tests(
        &harness.ctx,
        "a-0"
    ));

    // When: the thread is deleted (the app passes its thread and run IDs).
    gui::panes::agent::forget_transcript_view_state(
        &harness.ctx,
        &["thread-a".to_owned(), "a-0".to_owned()],
    );

    // Then: the pane holds nothing for it, and shows it closed if it comes back.
    assert!(!gui::panes::agent::holds_transcript_view_state_for_tests(
        &harness.ctx,
        "a-0"
    ));
    harness.state_mut().draw = true;
    harness.run_steps(3);
    assert_eq!(harness.query_all_by_label_contains("Read f0.rs").count(), 0);
}
