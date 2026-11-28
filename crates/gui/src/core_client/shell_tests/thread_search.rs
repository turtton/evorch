//! The Ctrl+K thread search over the window: opening it, narrowing by title, choosing a
//! thread and closing it.
use egui::{Key, Modifiers};
use egui_kittest::kittest::Queryable;
use workbench_protocol::commands::Command;
use workbench_protocol::errors::ErrorCode;
use workbench_protocol::namespaces::ConversationCommand;
use workbench_protocol::namespaces::workspace::{
    SearchThreads, ThreadSearchCursor, ThreadSearchHit, ThreadSearchIndex, ThreadSearchResults,
};
use workbench_protocol::views::ThreadSummary;

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

// ---- The core's search of thread texts, beside the title rows ----

fn hit(thread: &str, snippet: &str) -> ThreadSearchHit {
    ThreadSearchHit {
        thread: ThreadId::new(thread).unwrap(),
        title: None,
        snippet_kind: Some("user".into()),
        snippet: Some(snippet.into()),
        archived: false,
    }
}

fn page(hits: Vec<ThreadSearchHit>, cursor: Option<&str>) -> ThreadSearchResults {
    ThreadSearchResults {
        hits,
        next_cursor: cursor.map(|c| ThreadSearchCursor::new(c).unwrap()),
        index: None,
        title_scan_limited: false,
    }
}

fn conflict() -> FeatureError {
    FeatureError {
        code: Some(ErrorCode::Conflict),
        ..FeatureError::new("the search cursor is no longer valid")
    }
}

impl Screen {
    fn type_query(&mut self, text: &str) {
        self.harness.get_by_label(QUERY_LABEL).type_text(text);
        self.harness.run_steps(2);
    }

    /// Answers the workspace queries as [`Screen::settle`] does, with `pages` for the
    /// core's thread search; returns the search requests in the order they left.
    fn searched(
        &mut self,
        mut pages: impl FnMut(&SearchThreads) -> Result<ThreadSearchResults, FeatureError>,
    ) -> Vec<SearchThreads> {
        let mut asked = Vec::new();
        self.settle_scripted(&|_, _| (), &mut |request| {
            asked.push(request.clone());
            pages(request)
        });
        asked
    }

    fn cursors(asked: &[SearchThreads]) -> Vec<Option<&str>> {
        asked
            .iter()
            .map(|request| request.cursor.as_ref().map(ThreadSearchCursor::as_str))
            .collect()
    }
}

// Given: a word that only a thread's text holds, so no title lists it
// When: it is typed and the core finds that thread (not yet in the cache's title rows),
//       Enter opens the row, and a message is then sent
// Then: the core was asked for the opened project's text once, the thread is listed with
//       the snippet, opening it selects it, and the message is a chat message of that
//       thread, never a new execution
#[test]
fn a_word_only_a_thread_text_holds_lists_that_thread_with_its_snippet_and_opens_it() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    open(&mut screen);
    screen.type_query("rollback");
    let asked = screen.searched(|_| {
        Ok(page(
            vec![hit("thr-3", "the rollback of the session cookie")],
            None,
        ))
    });
    screen.harness.run_steps(2);

    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].project.as_str(), "proj-1");
    assert_eq!(asked[0].query, "rollback");
    assert!(asked[0].cursor.is_none());
    screen
        .harness
        .get_by_label("the rollback of the session cookie");

    screen.harness.key_press(Key::Enter);
    screen.settle();
    let state = screen.harness.state();
    assert!(!state.thread_search.is_open());
    assert_eq!(state.selected.as_ref().map(|t| t.as_str()), Some("thr-3"));

    screen.harness.get_by_label("Message").focus();
    screen.harness.run_steps(2);
    screen.harness.get_by_label("Message").type_text("go on");
    screen.harness.run_steps(2);
    screen.harness.key_press(Key::Enter);
    screen.harness.run_steps(2);
    let commands: Vec<_> = screen
        .settle()
        .into_iter()
        .filter_map(|action| match action {
            Action::Command { command, .. } => Some(command),
            _ => None,
        })
        .collect();
    let [Command::Conversation(ConversationCommand::SendChat(chat))] = commands.as_slice() else {
        panic!("one chat message, got {} commands", commands.len());
    };
    assert_eq!(chat.thread.as_str(), "thr-3");
    assert_eq!(chat.text, "go on");
}

// Given: a thread whose title already matches, and the core finds it by its text as well
// When: the pages arrive
// Then: it is listed once, with the snippet from its text added to its title row
#[test]
fn the_core_adds_its_snippet_to_a_title_row_without_a_second_row() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    open(&mut screen);
    screen.type_query("redirect");
    screen.searched(|_| Ok(page(vec![hit("thr-3", "an unrelated cookie note")], None)));
    screen.harness.run_steps(2);
    assert_eq!(
        screen
            .harness
            .query_all_by_label("an unrelated cookie note")
            .count(),
        1
    );
    screen.harness.get_by_label("an unrelated cookie note");
}

// Given: pages with no hit, with only a thread of another project, then with a thread
//        this client can open
// When: the search runs
// Then: it asks for the next page by itself until that thread arrives, in order, and the
//       cursors are the core's own, passed back unchanged
#[test]
fn pages_with_nothing_to_show_continue_by_themselves() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    open(&mut screen);
    screen.type_query("cookie");
    let asked = screen.searched(|request| {
        Ok(
            match request.cursor.as_ref().map(ThreadSearchCursor::as_str) {
                None => page(vec![], Some("c1")),
                Some("c1") => page(vec![hit("thr-8", "other project")], Some("c2")),
                _ => page(vec![hit("thr-3", "a cookie")], Some("c3")),
            },
        )
    });
    assert_eq!(Screen::cursors(&asked), [None, Some("c1"), Some("c2")]);
    screen.harness.run_steps(2);
    screen.harness.get_by_label("a cookie");
    assert!(screen.harness.query_by_label("other project").is_none());
    // A row was shown, so it waits for the person: "Search more" goes on from the cursor.
    screen.harness.get_by_label("Search more").click();
    screen.harness.run_steps(2);
    let asked = screen.searched(|_| Ok(page(vec![], None)));
    assert_eq!(Screen::cursors(&asked), [Some("c3")]);
    assert!(screen.harness.query_by_label("Search more").is_none());
}

// Given: a core whose pages never hold a row to show
// When: the search runs and the person asks for more twice
// Then: each operation sends five requests and stops with the cursor kept
#[test]
fn an_operation_sends_at_most_five_requests() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    open(&mut screen);
    screen.type_query("cookie");
    let asked = screen.searched(|_| Ok(page(vec![], Some("more"))));
    assert_eq!(asked.len(), 5);
    for _ in 0..2 {
        screen.harness.run_steps(2);
        screen.harness.get_by_label("Search more").click();
        screen.harness.run_steps(2);
        let asked = screen.searched(|_| Ok(page(vec![], Some("more"))));
        assert_eq!(asked.len(), 5);
    }
}

// Given: a row shown, then a cursor the core refuses
// When: the person asks for more
// Then: the search starts over once from the first page, the shown row stays once, and
//       the retry counts in the five requests
#[test]
fn a_refused_cursor_starts_over_once_and_keeps_the_rows() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    open(&mut screen);
    screen.type_query("cookie");
    screen.searched(|_| Ok(page(vec![hit("thr-3", "a cookie")], Some("c1"))));
    screen.harness.run_steps(2);
    screen.harness.get_by_label("Search more").click();
    screen.harness.run_steps(2);
    let mut refused = false;
    let asked = screen.searched(|request| {
        if request.cursor.is_some() && !refused {
            refused = true;
            return Err(conflict());
        }
        Ok(match request.cursor {
            None => page(vec![hit("thr-3", "a cookie")], Some("d1")),
            Some(_) => page(vec![], Some("d2")),
        })
    });
    assert_eq!(asked.len(), 5, "{:?}", Screen::cursors(&asked));
    assert_eq!(Screen::cursors(&asked)[..3], [Some("c1"), None, Some("d1")]);
    screen.harness.run_steps(2);
    assert_eq!(screen.harness.query_all_by_label("a cookie").count(), 1);
}

// Given: a core that refuses every cursor
// When: the person asks for more
// Then: it retries once, stops, offers "Search again", keeps the rows, and searching again
//       starts from the first page
#[test]
fn a_second_refusal_stops_and_search_again_starts_from_the_first_page() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    open(&mut screen);
    screen.type_query("cookie");
    screen.searched(|_| Ok(page(vec![hit("thr-3", "a cookie")], Some("c1"))));
    screen.harness.run_steps(2);
    screen.harness.get_by_label("Search more").click();
    screen.harness.run_steps(2);
    let asked = screen.searched(|_| Err(conflict()));
    assert_eq!(Screen::cursors(&asked), [Some("c1"), None]);
    screen.harness.run_steps(2);
    screen.harness.get_by_label("a cookie");
    screen.harness.get_by_label("Search again").click();
    screen.harness.run_steps(2);
    let asked = screen.searched(|_| Ok(page(vec![], None)));
    assert_eq!(Screen::cursors(&asked), [None]);
}

// Given: another failure
// When: it arrives
// Then: the core's words are shown, nothing is resent, and "Search again" is offered
#[test]
fn another_failure_is_shown_and_not_resent() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    open(&mut screen);
    screen.type_query("cookie");
    let asked = screen.searched(|_| Err(FeatureError::new("the saved threads could not be read")));
    assert_eq!(asked.len(), 1);
    screen.harness.run_steps(2);
    screen
        .harness
        .get_by_label("the saved threads could not be read");
    screen.harness.get_by_label("Search again");
}

// Given: a request for "roll" still out
// When: the text becomes "rollback" and the old answer arrives, then the new one
// Then: the old answer lists nothing, and only the new query's answer does
#[test]
fn a_late_answer_for_an_older_query_does_not_list_its_threads() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    open(&mut screen);
    screen.type_query("roll");
    let held = held_search(&mut screen);
    assert_eq!(held.1.query, "roll");
    screen.type_query("back");
    reply_search(&screen, held.0, page(vec![hit("thr-3", "old")], None));
    screen.harness.run_steps(2);
    assert!(screen.harness.query_by_label("old").is_none());
    // One request at a time: the newer query waits for the older answer, and then goes.
    let asked = screen.searched(|_| Ok(page(vec![hit("thr-3", "new")], None)));
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].query, "rollback");
    screen.harness.run_steps(2);
    screen.harness.get_by_label("new");
}

// Given: a request out, and the search closed with Esc
// When: the answer arrives and the search is opened again with the same text
// Then: the answer is dropped and the core is asked again from the first page
#[test]
fn closing_the_search_drops_the_answer_and_opening_it_asks_again() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    open(&mut screen);
    screen.type_query("cookie");
    let held = held_search(&mut screen);
    screen.harness.key_press(Key::Escape);
    screen.harness.run_steps(2);
    reply_search(&screen, held.0, page(vec![hit("thr-3", "old")], None));
    screen.harness.run_steps(2);
    open(&mut screen);
    screen.type_query("cookie");
    let asked = screen.searched(|_| Ok(page(vec![], None)));
    assert_eq!(Screen::cursors(&asked), [None]);
}

// Given: a request out when the session is replaced (a reconnect)
// When: the old answer arrives later
// Then: it is dropped, and the same search asks again from the first page
#[test]
fn a_reconnect_drops_the_old_answer_and_asks_again() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    open(&mut screen);
    screen.type_query("cookie");
    let held = held_search(&mut screen);
    screen
        .shared
        .lock()
        .unwrap()
        .features
        .begin_session(&[], Vec::new());
    reply_search(&screen, held.0, page(vec![hit("thr-3", "old")], None));
    screen.harness.run_steps(2);
    assert!(screen.harness.query_by_label("old").is_none());
    let asked = screen.searched(|_| Ok(page(vec![hit("thr-3", "new")], None)));
    assert_eq!(Screen::cursors(&asked), [None]);
}

// Given: a blank query, then a query with no project selected
// When: they are typed
// Then: the core is never asked; without a project the search says it needs one
#[test]
fn a_blank_query_or_no_project_asks_the_core_nothing() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    open(&mut screen);
    screen.type_query("   ");
    let asked = screen.searched(|_| panic!("a blank query is not searched"));
    assert!(asked.is_empty());
    screen.harness.key_press(Key::Escape);
    screen.harness.run_steps(2);

    let state = screen.harness.state_mut();
    state.project = None;
    state.selected = None;
    open(&mut screen);
    screen.type_query("cookie");
    let asked = screen.searched(|_| panic!("no project, nothing to search"));
    assert!(asked.is_empty());
    screen
        .harness
        .get_by_label("Select a project to search the text of its threads");
}

// Given: pages that report the index, and a title-only page that does not
// When: they arrive
// Then: the unindexed count is shown from the report, and a page without one shows nothing
#[test]
fn the_index_report_is_shown_only_when_a_body_page_made_one() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    open(&mut screen);
    screen.type_query("cookie");
    screen.searched(|_| Ok(page(vec![], None)));
    screen.harness.run_steps(2);
    assert!(
        screen
            .harness
            .query_by_label_contains("not searchable yet")
            .is_none()
    );

    screen.harness.key_press(Key::Escape);
    screen.harness.run_steps(2);
    open(&mut screen);
    screen.type_query("cookie");
    screen.searched(|_| {
        Ok(ThreadSearchResults {
            index: Some(ThreadSearchIndex {
                enabled: Some(true),
                unindexed: 3,
                backfill_in_progress: false,
            }),
            ..page(vec![], None)
        })
    });
    screen.harness.run_steps(2);
    screen
        .harness
        .get_by_label_contains("3 runs are not searchable yet");
}

/// Runs frames until the thread search's request leaves, and keeps it unanswered, so the
/// test decides when its answer arrives. The other workspace queries are answered.
fn held_search(screen: &mut Screen) -> (u64, SearchThreads) {
    for _ in 0..16 {
        screen.harness.run_steps(1);
        while let Ok(action) = screen.actions.try_recv() {
            match action {
                Action::Feature(FeatureAction {
                    feature: FeatureId::Workspace,
                    tag,
                    request:
                        FeatureRequest::Query(Query::Workspace(WorkspaceQuery::SearchThreads(request))),
                }) => return (tag, request),
                Action::Feature(FeatureAction {
                    feature: FeatureId::Workspace,
                    tag,
                    request: FeatureRequest::Query(Query::Workspace(query)),
                }) => {
                    let reply = FeatureReply::Query(Ok(QueryReply::Workspace(answer(&query))));
                    screen
                        .shared
                        .lock()
                        .unwrap()
                        .features
                        .reply(FeatureId::Workspace, tag, reply);
                }
                _ => {}
            }
        }
    }
    panic!("the search request never left");
}

fn reply_search(screen: &Screen, tag: u64, page: ThreadSearchResults) {
    let reply = FeatureReply::Query(Ok(QueryReply::Workspace(
        WorkspaceReply::ThreadSearchResults(page),
    )));
    screen
        .shared
        .lock()
        .unwrap()
        .features
        .reply(FeatureId::Workspace, tag, reply);
}

/// The window with `count` more threads in proj-1: "thr-<kind>-N" titled "<Kind> N".
fn screen_with_threads(kind: &str, count: usize) -> Screen {
    screen_with_sized_threads(kind, count, egui::vec2(1440.0, 900.0))
}

/// As [`screen_with_threads`], in a window of `size`.
fn screen_with_sized_threads(kind: &str, count: usize, size: egui::Vec2) -> Screen {
    let local = super::super::local::LocalPanes::new(
        Arc::new(crate::diff::FixtureDiffSource::empty()),
        None,
    );
    let title = format!("{}{}", kind[..1].to_uppercase(), &kind[1..]);
    let mut screen = screen_from(size, local, |shared| {
        for number in 0..count {
            let thread: ThreadSummary = view(json!({
                "id": format!("thr-{kind}-{number}"), "project": "proj-1",
                "title": format!("{title} {number}"), "state": "idle", "revision": "3",
                "updated_at": (1_793_000_000_000_u64 + number as u64).to_string(),
            }));
            shared.cache.threads.insert(thread.id.to_string(), thread);
        }
    });
    screen.settle();
    screen
}

fn press_down(screen: &mut Screen, times: usize) {
    for _ in 0..times {
        screen.harness.key_press(Key::ArrowDown);
        screen.harness.run_steps(1);
    }
}

// Given: fifty-nine more thread titles that match, and a thread only its text matches
// When: the core finds that thread and the person moves down to the last row
// Then: no row is dropped for being past fifty: the text-only thread is the last row, is
//       shown with its snippet, and Enter opens it
#[test]
fn a_text_only_match_after_fifty_title_matches_can_be_reached_and_opened() {
    let mut screen = screen_with_threads("bulk", 60);
    open(&mut screen);
    screen.type_query("bulk");
    screen.searched(|_| Ok(page(vec![hit("thr-3", "needle in a haystack")], None)));
    screen.harness.run_steps(2);
    press_down(&mut screen, 60);
    screen.harness.get_by_label("needle in a haystack");
    screen.harness.key_press(Key::Enter);
    screen.settle();
    let state = screen.harness.state();
    assert!(!state.thread_search.is_open());
    assert_eq!(state.selected.as_ref().map(|t| t.as_str()), Some("thr-3"));
}

// Given: sixty threads that only the core's text search finds, twenty to a page
// When: the person asks for more twice and moves down to the last row
// Then: all sixty rows were added and the last one opens: results past fifty are reachable
#[test]
fn text_matches_continued_past_fifty_rows_can_be_reached_and_opened() {
    let mut screen = screen_with_threads("pool", 60);
    open(&mut screen);
    screen.type_query("needle");
    let mut pages = |request: &SearchThreads| {
        let page_number = match request.cursor.as_ref().map(ThreadSearchCursor::as_str) {
            None => 0,
            Some("p1") => 1,
            _ => 2,
        };
        let hits = (page_number * 20..page_number * 20 + 20)
            .map(|n| hit(&format!("thr-pool-{n}"), &format!("needle {n}")))
            .collect();
        let next = ["p1", "p2"].get(page_number).copied();
        Ok(page(hits, next))
    };
    screen.searched(&mut pages);
    for _ in 0..2 {
        screen.harness.run_steps(2);
        screen.harness.get_by_label("Search more").click();
        screen.harness.run_steps(2);
        screen.searched(&mut pages);
    }
    screen.harness.run_steps(2);
    assert!(screen.harness.query_by_label("Search more").is_none());
    press_down(&mut screen, 59);
    screen.harness.get_by_label("needle 59");
    screen.harness.key_press(Key::Enter);
    screen.settle();
    let state = screen.harness.state();
    assert_eq!(
        state.selected.as_ref().map(|t| t.as_str()),
        Some("thr-pool-59")
    );
}

// Given: forty threads whose hundred-character titles are far wider than a narrow window
// When: the person moves down through them one by one
// Then: the chosen row is always inside the scrolled view, so Enter opens a thread that
//       can be seen, and it is the one the person moved to
#[test]
fn the_chosen_row_stays_in_view_when_titles_are_longer_than_the_window() {
    let mut screen = screen_with_sized_threads("long", 40, egui::vec2(640.0, 700.0));
    open(&mut screen);
    screen.type_query("long");
    screen.harness.run_steps(3);
    // The sidebar lists the same titles; the modal is the one further right.
    let row_rect = |screen: &Screen, title: &str| {
        screen
            .harness
            .query_all_by_label_contains(title)
            .map(|node| node.rect())
            .max_by(|a, b| a.min.x.total_cmp(&b.min.x))
            .unwrap_or_else(|| panic!("no row {title}"))
    };
    let first = row_rect(&screen, "Long 39 ").min.y;
    for step in 1..40 {
        press_down(&mut screen, 1);
        screen.harness.run_steps(2);
        let (_, view_height) = screen.harness.state().thread_search.scroll_view();
        // Newest first: row `step` is "Long <39 - step>".
        let rect = row_rect(&screen, &format!("Long {} ", 39 - step));
        assert!(
            rect.min.y >= first - 2.0 && rect.max.y <= first + view_height + 2.0,
            "row {step} is at {rect:?}, outside the view of {view_height} from {first}"
        );
    }
    screen.harness.key_press(Key::Enter);
    screen.settle();
    let state = screen.harness.state();
    assert_eq!(
        state.selected.as_ref().map(|t| t.as_str()),
        Some("thr-long-0")
    );
}
