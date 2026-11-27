//! The Ctrl+K thread search over the window: opening it, narrowing by title, choosing a
//! thread and closing it.
use egui::{Key, Modifiers};
use egui_kittest::kittest::Queryable;

use super::*;
use crate::core_client::thread_search::QUERY_LABEL;

fn open(screen: &mut Screen) {
    screen
        .harness
        .key_press_modifiers(Modifiers::COMMAND, Key::K);
    screen.harness.run_steps(3);
}

// Given: thr-1 selected in evorch, whose "storage" threads are thr-2 and the older thr-5
// When: Ctrl+K opens the search, "storage" is typed, Down is pressed and Enter
// Then: the second match, thr-5, is opened, the search is closed and the core is asked for
//       its history
#[test]
fn ctrl_k_narrows_by_title_and_enter_opens_the_chosen_thread() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    open(&mut screen);
    assert!(screen.harness.state().thread_search.is_open());

    screen
        .harness
        .get_by_label(QUERY_LABEL)
        .type_text("storage");
    screen.harness.run_steps(2);
    screen.harness.key_press(Key::ArrowDown);
    screen.harness.run_steps(2);
    screen.harness.key_press(Key::Enter);
    let actions = screen.settle();

    let state = screen.harness.state();
    assert!(!state.thread_search.is_open());
    assert_eq!(state.selected.as_ref().map(|t| t.as_str()), Some("thr-5"));
    assert!(actions.iter().any(
        |action| matches!(action, Action::History(thread, None, false) if thread.as_str() == "thr-5")
    ));
}

// Given: the search opened with Ctrl+K
// When: Esc is pressed, and then Ctrl+K twice
// Then: Esc closes it without changing the selection, and Ctrl+K opens it and closes it again
#[test]
fn esc_and_ctrl_k_close_the_search_and_keep_the_selection() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    open(&mut screen);
    screen.harness.key_press(Key::Escape);
    screen.harness.run_steps(2);
    assert!(!screen.harness.state().thread_search.is_open());

    open(&mut screen);
    assert!(screen.harness.state().thread_search.is_open());
    open(&mut screen);
    let state = screen.harness.state();
    assert!(!state.thread_search.is_open());
    assert_eq!(state.selected.as_ref().map(|t| t.as_str()), Some("thr-1"));
}

// Given: an IME conversion open in the search field
// When: Esc, which cancels the conversion, is pressed
// Then: the search stays open
#[test]
fn esc_that_cancels_a_conversion_does_not_close_the_search() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    open(&mut screen);
    screen
        .harness
        .event(egui::Event::Ime(egui::ImeEvent::Preedit {
            text: "に".into(),
            active_range_chars: None,
        }));
    screen.harness.run_steps(1);
    screen.harness.key_press(Key::Escape);
    screen.harness.run_steps(2);
    assert!(screen.harness.state().thread_search.is_open());
}

// Given: the composer focused, where Esc stops the running chat
// When: the search is opened and closed with Esc
// Then: no command was sent, and the search is closed
#[test]
fn esc_in_the_search_does_not_stop_the_chat_behind_it() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    screen.harness.get_by_label("Message").focus();
    screen.harness.run_steps(2);
    open(&mut screen);
    screen.harness.key_press(Key::Escape);
    screen.harness.run_steps(2);
    let others = screen.settle();
    assert!(!screen.harness.state().thread_search.is_open());
    assert!(
        !others
            .iter()
            .any(|action| matches!(action, Action::Command { .. })),
        "{}",
        others.len()
    );
}

// Given: another modal, the theme picker, open
// When: Ctrl+K is pressed
// Then: the search does not open over it
#[test]
fn ctrl_k_does_not_open_over_another_modal() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    screen.harness.state_mut().theme.open = true;
    screen.harness.run_steps(2);
    open(&mut screen);
    assert!(!screen.harness.state().thread_search.is_open());
}
