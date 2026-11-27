//! The cards of running tool calls: what they show, how they fold, and how they give way to
//! the committed rows.
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use egui_kittest::{Harness, kittest::Queryable};
use workbench_protocol::namespaces::execution::ToolActivityStatus;

use super::tests::{State, body, history, inline, tool_row};
use super::*;
use crate::core_client::tool_activity::{Clock, LiveTools};

fn harness(cache: Cache) -> Harness<'static, State> {
    let state = State {
        cache,
        requested: BTreeSet::new(),
        effects: Vec::new(),
    };
    Harness::new_ui_state(
        |ui, state: &mut State| {
            let thread = ThreadId::new("thr").unwrap();
            transcript(
                ui,
                &mut state.requested,
                TranscriptView {
                    cache: &state.cache,
                    scope: Scope::Thread(&thread),
                    base: None,
                    chips: None,
                },
                |_| {},
                |ui, _| {
                    ui.label("after the cards");
                },
                &mut state.effects,
            );
        },
        state,
    )
}

/// A cache whose live calls are timed by a clock the test moves, in milliseconds.
fn clocked(mut cache: Cache) -> (Cache, Arc<AtomicU64>) {
    let base = Instant::now();
    let moved = Arc::new(AtomicU64::new(0));
    let clock = Clock::from_fn({
        let moved = moved.clone();
        move || base + Duration::from_millis(moved.load(Ordering::SeqCst))
    });
    cache.live_tools = LiveTools::with_clock(clock);
    (cache, moved)
}

fn start(cache: &mut Cache, call: u64, tool_name: &str, input: Option<&str>, elapsed_ms: u64) {
    let mut tool = serde_json::json!({
        "thread": "thr", "run": "r", "call_id": format!("call-{call}"),
        "tool_name": tool_name, "input_summary": format!("summary {call}"),
        "elapsed_ms": elapsed_ms,
    });
    if let Some(input) = input {
        tool["input"] = inline(input);
    }
    cache
        .live_tools
        .start(serde_json::from_value(tool).unwrap());
}

fn finish(cache: &mut Cache, call: u64, duration_ms: u64) {
    cache.live_tools.finish(
        "r",
        &format!("call-{call}"),
        ToolActivityStatus::Succeeded,
        Some(duration_ms),
    );
}

fn lines(count: usize) -> String {
    (0..count)
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\n")
}

// Given: a running call whose input is far taller than the card may be
// Then: it is open without being pressed, shows its whole input, and the card stops growing
//       at the limit (what follows the cards stays close), the input scrolling inside
#[test]
fn a_running_card_shows_its_input_and_scrolls_it_inside() {
    let mut cache = Cache::default();
    start(&mut cache, 0, "shell", Some(&lines(300)), 0);
    let mut harness = harness(cache);
    harness.run_steps(3);
    harness.get_by_label("Running");
    harness.get_by_label("Input");
    harness.get_by_label_contains("line 0");
    harness.get_by_label_contains("line 299");
    let top = harness.get_by_label("Running").rect().min.y;
    let after = harness.get_by_label("after the cards").rect().min.y;
    assert!(
        after - top < 420.0,
        "the card is {} tall for 300 lines",
        after - top
    );
}

// Given: a running card, a card the person closed while it ran, and a finished card the
//        person opened
// When: the first two finish
// Then: the first closes itself and shows its core duration; the one closed by hand stays
//       closed; the one opened by hand stays open
#[test]
fn a_finished_card_folds_unless_the_person_set_it() {
    let mut cache = Cache::default();
    start(&mut cache, 0, "shell", Some("auto"), 0);
    start(&mut cache, 1, "shell", Some("closed by hand"), 0);
    start(&mut cache, 2, "shell", Some("opened by hand"), 0);
    finish(&mut cache, 2, 100);
    let mut harness = harness(cache);
    harness.run_steps(3);
    harness.get_by_label("auto");
    harness.get_by_label("closed by hand");
    assert!(
        harness.query_by_label("opened by hand").is_none(),
        "a finished card is closed"
    );
    harness.get_by_label("summary 1").click();
    harness.run_steps(3);
    assert!(harness.query_by_label("closed by hand").is_none());
    harness.get_by_label("summary 2").click();
    harness.run_steps(3);
    harness.get_by_label("opened by hand");

    finish(&mut harness.state_mut().cache, 0, 3_400);
    finish(&mut harness.state_mut().cache, 1, 2_000);
    harness.run_steps(3);
    assert!(harness.query_by_label("auto").is_none(), "folded itself");
    assert!(harness.query_by_label("closed by hand").is_none());
    harness.get_by_label("opened by hand");
    harness.get_by_label("3.4s");
    harness.get_by_label("2.0s");
    harness.get_by_label("0.1s");
}

// Given: a running card, pressed on its padding, and its input text dragged over
// Then: the press closes and opens it; selecting text leaves it open
#[test]
fn a_running_card_opens_on_a_press_and_not_on_a_selection() {
    let mut cache = Cache::default();
    start(&mut cache, 0, "run_js", Some("first line\nsecond line"), 0);
    let mut harness = harness(cache);
    harness.run_steps(3);
    let text = harness.get_by_label("first line\nsecond line").rect();
    harness.hover_at(text.left_top() + egui::vec2(2.0, 2.0));
    harness.run_steps(2);
    harness.drag_at(text.left_top() + egui::vec2(2.0, 2.0));
    harness.run_steps(1);
    harness.hover_at(text.right_bottom() - egui::vec2(2.0, 2.0));
    harness.run_steps(1);
    harness.drop_at(text.right_bottom() - egui::vec2(2.0, 2.0));
    harness.run_steps(3);
    harness.get_by_label("Input");

    let status = harness.get_by_label("Running").rect();
    let padding = egui::pos2(status.right() + 3.0, status.center().y);
    harness.hover_at(padding);
    harness.run_steps(2);
    harness.drag_at(padding);
    harness.drop_at(padding);
    harness.run_steps(3);
    assert!(harness.query_by_label("Input").is_none());
    harness.get_by_label("Running").click();
    harness.run_steps(3);
    harness.get_by_label("Input");
}

// Given: a running run_js call, a running read call, and a clock moved by hand
// Then: the first shows the core's elapsed time plus what the clock moved, the second shows no
//       time, running or after it finishes
#[test]
fn only_the_listed_tools_show_a_time_and_it_follows_the_clock() {
    let (mut cache, clock) = clocked(Cache::default());
    start(&mut cache, 0, "run_js", None, 1_000);
    start(&mut cache, 1, "read", None, 1_000);
    let mut harness = harness(cache);
    harness.run_steps(3);
    harness.get_by_label("1.0s");
    clock.store(11_500, Ordering::SeqCst);
    harness.run_steps(3);
    harness.get_by_label("12.5s");
    assert_eq!(harness.query_all_by_label("12.5s").count(), 1);

    finish(&mut harness.state_mut().cache, 0, 12_400);
    finish(&mut harness.state_mut().cache, 1, 12_400);
    clock.store(60_000, Ordering::SeqCst);
    harness.run_steps(3);
    harness.get_by_label("12.4s");
    assert!(harness.query_by_label("12.5s").is_none());
    assert_eq!(harness.query_all_by_label("12.4s").count(), 1);
}

// Given: committed calls with a duration, one of a listed tool and one of another
// Then: only the listed tool's row shows its time, in the new form
#[test]
fn a_committed_row_shows_its_duration_for_listed_tools_only() {
    let timed = tool_row(
        "r",
        0,
        serde_json::json!({"duration_ms": 65_000}),
        inline("x"),
    );
    let other = tool_row(
        "r",
        1,
        serde_json::json!({"tool_name": "read", "duration_ms": 4_000}),
        inline("x"),
    );
    let mut harness = harness(history(vec![timed, other]));
    harness.run_steps(3);
    harness.get_by_label("1m 05s");
    assert!(harness.query_by_label("4.0s").is_none());
}

// Given: a call that is both running and committed (its start came late)
// Then: only the committed card is drawn
#[test]
fn a_call_with_a_committed_row_has_no_live_card() {
    let row = tool_row("r", 0, serde_json::json!({}), inline("done"));
    let mut cache = history(vec![row]);
    start(&mut cache, 0, "run_js", None, 0);
    start(&mut cache, 1, "run_js", None, 0);
    let mut harness = harness(cache);
    harness.run_steps(3);
    assert_eq!(harness.query_all_by_label("summary 0").count(), 1);
    harness.get_by_label("Succeeded");
    harness.get_by_label("Running");
}

// Given: a finished card the person opened
// When: its committed row replaces it
// Then: the card is still open, now with the result
#[test]
fn the_state_of_a_card_carries_over_to_its_committed_row() {
    let mut cache = Cache::default();
    start(&mut cache, 0, "run_js", Some("the code"), 0);
    finish(&mut cache, 0, 100);
    let mut harness = harness(cache);
    harness.run_steps(3);
    harness.get_by_label("summary 0").click();
    harness.run_steps(3);
    harness.get_by_label("the code");

    let row = tool_row(
        "r",
        0,
        serde_json::json!({"input": inline("the code")}),
        inline("the result"),
    );
    let state = harness.state_mut();
    state.cache.history.insert("thr".into(), vec![row]);
    state.cache.live_tools.committed("r", "call-0");
    harness.run_steps(3);
    harness.get_by_label("the code");
    harness.get_by_label("the result");
    assert_eq!(harness.query_all_by_label("summary 0").count(), 1);
}

// Given: twenty running cards with tall inputs, scrolled so the ninth is in view
// When: the first finishes and folds, and then its committed row replaces it
// Then: the ninth stays where it was on screen after each
#[test]
fn a_card_folding_above_the_view_keeps_the_view_in_place() {
    let mut cache = Cache::default();
    for call in 0..20 {
        start(&mut cache, call, "shell", Some(&lines(14)), 0);
    }
    let mut harness = harness(cache);
    harness.run_steps(4);
    harness.get_by_label("summary 8").scroll_to_me();
    harness.run_steps(12);
    let before = harness.get_by_label("summary 8").rect();
    assert_eq!(harness.query_all_by_label("Input").count(), 20);

    finish(&mut harness.state_mut().cache, 0, 100);
    harness.run_steps(4);
    assert_eq!(harness.query_all_by_label("Input").count(), 19, "it folded");
    let folded = harness.get_by_label("summary 8").rect();
    assert!(
        (folded.min.y - before.min.y).abs() < 1.0,
        "the card moved from {before:?} to {folded:?} as the one above folded"
    );

    let row = tool_row("r", 0, serde_json::json!({}), inline("done"));
    let state = harness.state_mut();
    state.cache.history.insert("thr".into(), vec![row]);
    state.cache.live_tools.committed("r", "call-0");
    harness.run_steps(4);
    let after = harness.get_by_label("summary 8").rect();
    assert!(
        (after.min.y - folded.min.y).abs() < 1.0,
        "the card moved from {folded:?} to {after:?} as its neighbour was recorded"
    );
}

fn label_top(harness: &Harness<'_, State>, call: u64) -> f32 {
    harness
        .get_by_label(&format!("summary {call}"))
        .rect()
        .min
        .y
}

// Given: twenty running cards with tall inputs, scrolled so that a card sits at the top of the
//        view, where the view is anchored on it, below committed calls
// When: that very call is committed, after its finish was told and before it
// Then: the call stays where it was on screen, though its row goes in front of the live cards
#[test]
fn the_card_the_view_is_anchored_on_keeps_its_place_when_it_is_committed() {
    for finished_first in [true, false] {
        // Committed calls above, so that the row of the call lands well below the top of the
        // transcript and the view can follow it there.
        let earlier = (100..130)
            .map(|index| tool_row("r", index, serde_json::json!({}), inline("done")))
            .collect();
        let mut cache = history(earlier);
        for call in 0..20 {
            start(&mut cache, call, "shell", Some(&lines(14)), 0);
        }
        let mut harness = harness(cache);
        harness.run_steps(4);
        harness.get_by_label("summary 8").scroll_to_me();
        harness.run_steps(12);
        // The first card that starts in view is the anchor.
        let anchor = (0..20)
            .filter(|&call| label_top(&harness, call) >= 20.0)
            .min_by(|&a, &b| label_top(&harness, a).total_cmp(&label_top(&harness, b)))
            .unwrap();
        let before = label_top(&harness, anchor);
        if finished_first {
            finish(&mut harness.state_mut().cache, anchor, 100);
            harness.run_steps(4);
            let folded = label_top(&harness, anchor);
            assert!((folded - before).abs() < 1.0, "{before} to {folded}");
        }
        let row = tool_row("r", anchor, serde_json::json!({}), inline("done"));
        let state = harness.state_mut();
        state.cache.history.get_mut("thr").unwrap().push(row);
        state
            .cache
            .live_tools
            .committed("r", &format!("call-{anchor}"));
        harness.run_steps(4);
        let after = label_top(&harness, anchor);
        assert!(
            (after - before).abs() < 1.0,
            "call {anchor} moved from {before} to {after} (finish told first: {finished_first})"
        );
    }
}

// Given: twenty running cards whose inputs are bodies not read yet, the view at the newest
// When: it is scrolled to the first card
// Then: only the cards that can be in view ask for their body, each with its place kept, and
//       the first asks once it comes into view
#[test]
fn a_running_cards_input_is_read_only_while_it_can_be_in_view() {
    // A transcript already at its newest, as one being read while the agent works is.
    let earlier = (100..130)
        .map(|index| tool_row("r", index, serde_json::json!({}), inline("done")))
        .collect();
    let mut harness = harness(history(earlier));
    harness.run_steps(6);
    let cache = &mut harness.state_mut().cache;
    for call in 0..20 {
        let mut tool = serde_json::json!({
            "thread": "thr", "run": "r", "call_id": format!("call-{call}"),
            "tool_name": "shell", "input_summary": format!("summary {call}"),
            "elapsed_ms": 0,
        });
        tool["input"] = body(&format!("in-{call}"));
        cache
            .live_tools
            .start(serde_json::from_value(tool).unwrap());
    }
    harness.run_steps(6);
    let requested = harness.state().requested.clone();
    assert!(requested.contains("in-19"), "the newest is in view");
    assert!(!requested.contains("in-0"), "{requested:?}");
    assert!(requested.len() < 15, "{requested:?}");
    assert_eq!(
        harness
            .query_all_by_label_contains("Loading message")
            .count(),
        20,
        "each keeps its line"
    );

    harness.get_by_label("summary 0").scroll_to_me();
    harness.run_steps(12);
    assert!(harness.state().requested.contains("in-0"));
}
