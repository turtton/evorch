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
