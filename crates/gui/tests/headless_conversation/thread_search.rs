use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use egui::{Key, Modifiers};
use event_bus::{AgentRunPhase, Event, LifecycleEvent};
use gui::app::WorkbenchState;
use gui::headless::HeadlessWorkbench;
use gui::model::commands::{CommandSink, LoopEvent, WorkbenchCommand};
use gui::model::tasks::AgentRunSource;
use gui::model::thread_search::{SEARCH_LIMIT, UNAVAILABLE};
use gui::panes::thread_search::{
    NO_MATCHES_LABEL, REFINE_LABEL, SEARCHING_LABEL, TITLE_SCAN_LIMITED_LABEL, index_notice,
};
use runtime::{
    AgentInspection, AgentSummary, HostSearchIndex, HostThreadHit, HostThreadSearch, RunId,
};
use workspace_ui::{ProjectId, SidebarState, ThreadId, ThreadRecord, UiSettings};

struct EmptySource;

impl AgentRunSource for EmptySource {
    fn list(&self) -> Vec<AgentSummary> {
        Vec::new()
    }

    fn inspect(&self, _: RunId) -> Option<AgentInspection> {
        None
    }
}

type Answer = Sender<Result<HostThreadSearch, String>>;
/// Query, continuation cursor and reply channel of one search request.
type Search = (String, Option<String>, Answer);

/// Holds every search request so a test decides when, and in which order, they answer.
#[derive(Clone, Default)]
struct Requests {
    searches: Arc<Mutex<Vec<Search>>>,
    commands: Arc<Mutex<Vec<WorkbenchCommand>>>,
}

impl Requests {
    fn queries(&self) -> Vec<String> {
        self.searches
            .lock()
            .unwrap()
            .iter()
            .map(|(query, _, _)| query.clone())
            .collect()
    }

    fn cursor(&self, index: usize) -> Option<String> {
        self.searches.lock().unwrap()[index].1.clone()
    }

    fn answer(&self, index: usize, result: Result<HostThreadSearch, String>) -> bool {
        self.searches.lock().unwrap()[index].2.send(result).is_ok()
    }

    fn stops(&self) -> usize {
        self.commands
            .lock()
            .unwrap()
            .iter()
            .filter(|command| matches!(command, WorkbenchCommand::StopChat { .. }))
            .count()
    }
}

struct ManualSink(Requests);

impl CommandSink for ManualSink {
    fn submit(&mut self, command: WorkbenchCommand) -> Vec<LoopEvent> {
        self.0.commands.lock().unwrap().push(command);
        Vec::new()
    }

    fn search_threads(
        &self,
        _project: std::path::PathBuf,
        query: String,
        _limit: usize,
        cursor: Option<String>,
    ) -> Option<gui::model::thread_search::ThreadSearchReceiver> {
        let (sender, receiver) = std::sync::mpsc::channel();
        self.0
            .searches
            .lock()
            .unwrap()
            .push((query, cursor, sender));
        Some(receiver)
    }
}

fn state(root: &std::path::Path) -> WorkbenchState<EmptySource> {
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("project");
    sidebar
        .add_project(project.clone(), "Project", root)
        .unwrap();
    sidebar.select_project(&project).unwrap();
    for (id, title) in [("alpha", "Alpha"), ("beta", "Beta")] {
        sidebar
            .threads
            .push(ThreadRecord::new(ThreadId::new(id), project.clone(), title));
    }
    sidebar.switch_thread(&ThreadId::new("alpha")).unwrap();
    WorkbenchState::new(EmptySource, &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar)
        .with_sidebar_path(root.join("sidebar.json"))
}

fn fixture(root: &std::path::Path) -> (HeadlessWorkbench<EmptySource>, Requests) {
    let requests = Requests::default();
    let state = state(root).with_command_sink(Box::new(ManualSink(requests.clone())));
    (HeadlessWorkbench::new(state, [1200.0, 800.0]), requests)
}

fn hit(thread: &str, snippet: Option<&str>) -> HostThreadHit {
    HostThreadHit {
        thread_id: thread.into(),
        title: None,
        snippet_kind: snippet.map(|_| "conversation".into()),
        snippet: snippet.map(Into::into),
        archived: false,
        updated_at_ns: 0,
    }
}

fn page(hits: Vec<HostThreadHit>, next_cursor: Option<&str>) -> Result<HostThreadSearch, String> {
    Ok(HostThreadSearch {
        hits,
        next_cursor: next_cursor.map(Into::into),
        index: HostSearchIndex::default(),
        title_scan_limited: false,
    })
}

fn found(hits: Vec<HostThreadHit>) -> Result<HostThreadSearch, String> {
    page(hits, None)
}

fn active(gui: &HeadlessWorkbench<EmptySource>) -> Option<ThreadId> {
    gui.state().sidebar().active_thread.clone()
}

/// A key press as one raw event, so it lands in the same frame as other injected events.
fn press(key: Key) -> egui::Event {
    egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::NONE,
    }
}

fn type_text(gui: &mut HeadlessWorkbench<EmptySource>, text: &str) {
    gui.input_mut().events.push(egui::Event::Text(text.into()));
    gui.run();
}

fn start_chat(gui: &mut HeadlessWorkbench<EmptySource>, thread: &str) {
    let run_id = format!("run-{thread}");
    gui.state_mut().apply_events([
        Event::new(LifecycleEvent::AgentRunStarted {
            run_id: run_id.clone(),
            parent_run_id: None,
            agent_name: format!("chat:Worker:{thread}"),
            role: "worker".into(),
        }),
        Event::new(LifecycleEvent::AgentRunStateChanged {
            run_id,
            from: AgentRunPhase::Pending,
            to: AgentRunPhase::Running,
            reason: None,
        }),
    ]);
    gui.run();
}

#[test]
fn ctrl_k_search_selects_a_result_with_the_keyboard_and_opens_its_thread() {
    // Given: the modal opened with the shortcut, listing recent threads.
    let temp = tempfile::tempdir().unwrap();
    let (mut gui, requests) = fixture(temp.path());
    gui.run();
    gui.key_press(Modifiers::COMMAND, Key::K);
    gui.run();
    assert!(gui.state().thread_search().is_open());
    assert_eq!(requests.queries(), [""]);
    assert!(requests.answer(
        0,
        found(vec![
            hit("alpha", None),
            hit("beta", Some("needle in body"))
        ])
    ));
    gui.run();
    assert!(gui.has_label(&format!(
        "{} conversation: needle in body",
        gui::theme::icons::MAGNIFYING_GLASS
    )));

    // When: moving down to the second result and confirming.
    gui.key_press(Modifiers::NONE, Key::ArrowDown);
    gui.step();
    gui.key_press(Modifiers::NONE, Key::Enter);
    gui.run();

    // Then: its thread is active and the modal is gone.
    assert_eq!(active(&gui), Some(ThreadId::new("beta")));
    assert!(!gui.state().thread_search().is_open());
}

#[test]
fn sidebar_search_button_opens_the_modal_and_escape_closes_it() {
    let temp = tempfile::tempdir().unwrap();
    let (mut gui, requests) = fixture(temp.path());
    gui.run();
    gui.click_label("Search threads");
    gui.run();
    assert!(gui.state().thread_search().is_open());
    assert!(gui.has_label(SEARCHING_LABEL));

    // Closing drops the search: its late answer has nowhere to go.
    gui.key_press(Modifiers::NONE, Key::Escape);
    gui.run();
    assert!(!gui.state().thread_search().is_open());
    assert!(!requests.answer(0, found(vec![hit("beta", None)])));
    assert_eq!(active(&gui), Some(ThreadId::new("alpha")));
}

#[test]
fn escape_closes_the_search_without_stopping_the_running_chat_behind_it() {
    // Given: a chat that is running, with a Stop control in the composer.
    let temp = tempfile::tempdir().unwrap();
    let (mut gui, requests) = fixture(temp.path());
    gui.run();
    start_chat(&mut gui, "alpha");
    assert!(gui.has_label("Stop"));
    gui.key_press(Modifiers::COMMAND, Key::K);
    gui.run();
    assert!(gui.state().thread_search().is_open());

    // When: Esc is pressed while the search is open.
    gui.key_press(Modifiers::NONE, Key::Escape);
    gui.run();

    // Then: only the search closes; the chat keeps running.
    assert!(!gui.state().thread_search().is_open());
    assert_eq!(requests.stops(), 0);

    // And Esc still stops the chat once the search is gone.
    gui.key_press(Modifiers::NONE, Key::Escape);
    gui.run();
    assert_eq!(requests.stops(), 1);
}

#[test]
fn results_of_an_older_query_are_not_adopted_and_cannot_be_confirmed() {
    let temp = tempfile::tempdir().unwrap();
    let (mut gui, requests) = fixture(temp.path());
    gui.run();
    gui.state_mut().open_thread_search();
    gui.run();
    assert_eq!(requests.queries(), [""]);

    // When: the user types while the first search runs, then it answers.
    type_text(&mut gui, "needle");
    assert_eq!(requests.queries(), [""], "one request in flight at a time");
    assert!(requests.answer(0, found(vec![hit("alpha", None), hit("beta", None)])));
    gui.run();
    // Then: the answer to the old query is dropped, and the newest query is searched.
    assert!(gui.state().thread_search().outcome().is_none());
    assert_eq!(requests.queries(), ["", "needle"]);

    // When: the answer for the newest query arrives.
    assert!(requests.answer(1, found(vec![hit("beta", Some("needle"))])));
    gui.run();
    assert_eq!(
        gui.state().thread_search().outcome().unwrap().key.query,
        "needle"
    );

    // When: the query changes again, the old results stay visible but are not confirmable.
    type_text(&mut gui, "s");
    assert_eq!(requests.queries(), ["", "needle", "needles"]);
    gui.key_press(Modifiers::NONE, Key::Enter);
    gui.run();
    assert_eq!(active(&gui), Some(ThreadId::new("alpha")));
    assert!(gui.state().thread_search().is_open());

    // When: the matching answer arrives, Then Enter opens its result.
    assert!(requests.answer(2, found(vec![hit("beta", Some("needles"))])));
    gui.run();
    gui.key_press(Modifiers::NONE, Key::Enter);
    gui.run();
    assert_eq!(active(&gui), Some(ThreadId::new("beta")));
}

#[test]
fn a_partial_page_continues_in_the_background_instead_of_reporting_no_match() {
    let temp = tempfile::tempdir().unwrap();
    let (mut gui, requests) = fixture(temp.path());
    gui.run();
    gui.state_mut().open_thread_search();
    gui.run();

    // When: the first request examined only part of the project and found nothing.
    assert!(requests.answer(0, page(Vec::new(), Some("c1"))));
    gui.run();
    // Then: the search continues from its cursor and does not claim "no match".
    assert_eq!(requests.queries(), ["", ""]);
    assert_eq!(requests.cursor(1).as_deref(), Some("c1"));
    assert!(gui.has_label(SEARCHING_LABEL));
    assert!(!gui.has_label(NO_MATCHES_LABEL));

    // When: the continuation finds a match beyond the first page and exhausts the project.
    assert!(requests.answer(1, found(vec![hit("beta", Some("late match"))])));
    gui.run();
    // Then: it is listed and the search is finished.
    let rows = gui
        .state()
        .thread_search()
        .rows(gui.state().sidebar())
        .iter()
        .map(|row| row.thread.to_string())
        .collect::<Vec<_>>();
    assert_eq!(rows, ["beta"]);
    assert!(!gui.has_label(SEARCHING_LABEL));
    assert_eq!(requests.queries().len(), 2);
}

#[test]
fn an_exhausted_search_without_matches_says_so() {
    let temp = tempfile::tempdir().unwrap();
    let (mut gui, requests) = fixture(temp.path());
    gui.run();
    gui.state_mut().open_thread_search();
    gui.run();
    assert!(requests.answer(0, found(Vec::new())));
    gui.run();
    assert!(gui.has_label(NO_MATCHES_LABEL));
}

#[test]
fn a_search_with_enough_openable_matches_stops_asking_and_suggests_refining() {
    // Given: a sidebar with more threads than one search collects.
    let temp = tempfile::tempdir().unwrap();
    let requests = Requests::default();
    let mut state = state(temp.path()).with_command_sink(Box::new(ManualSink(requests.clone())));
    let mut sidebar = state.sidebar().clone();
    for index in 0..SEARCH_LIMIT {
        sidebar.threads.push(ThreadRecord::new(
            ThreadId::new(format!("t{index}")),
            ProjectId::new("project"),
            format!("Thread {index}"),
        ));
    }
    state = state.with_sidebar(sidebar);
    let mut gui = HeadlessWorkbench::new(state, [1200.0, 800.0]);
    gui.run();
    gui.state_mut().open_thread_search();
    gui.run();
    let many: Vec<_> = (0..SEARCH_LIMIT)
        .map(|index| hit(&format!("t{index}"), None))
        .collect();
    // When: a page fills the quota but the search could continue.
    assert!(requests.answer(0, page(many, Some("more"))));
    gui.run();
    // Then: it is not continued, only flagged.
    assert_eq!(requests.queries().len(), 1);
    assert!(gui.has_label(REFINE_LABEL));
}

#[test]
fn matches_the_sidebar_cannot_open_never_end_the_search_or_count_as_results() {
    let temp = tempfile::tempdir().unwrap();
    let (mut gui, requests) = fixture(temp.path());
    gui.run();
    gui.state_mut().open_thread_search();
    gui.run();

    // When: a full page of hits arrives that the sidebar does not list.
    let hidden: Vec<_> = (0..SEARCH_LIMIT)
        .map(|index| hit(&format!("gone{index}"), None))
        .collect();
    assert!(requests.answer(0, page(hidden, Some("c1"))));
    gui.run();
    // Then: the search continues rather than stopping or saying "no match".
    assert_eq!(requests.queries().len(), 2);
    assert_eq!(requests.cursor(1).as_deref(), Some("c1"));
    assert!(!gui.has_label(NO_MATCHES_LABEL));

    // When: the project is exhausted with an openable hit, Then it is the result.
    assert!(requests.answer(1, found(vec![hit("beta", None)])));
    gui.run();
    assert_eq!(
        gui.state()
            .thread_search()
            .rows(gui.state().sidebar())
            .len(),
        1
    );

    // And when everything found is unopenable, an exhausted search says "no match".
    gui.state_mut().open_thread_search();
    gui.run();
    assert!(requests.answer(2, found(vec![hit("gone", None)])));
    gui.run();
    assert!(gui.has_label(NO_MATCHES_LABEL));
}

#[test]
fn a_limited_title_scan_is_reported() {
    let temp = tempfile::tempdir().unwrap();
    let (mut gui, requests) = fixture(temp.path());
    gui.run();
    gui.state_mut().open_thread_search();
    gui.run();
    assert!(requests.answer(
        0,
        Ok(HostThreadSearch {
            hits: vec![hit("alpha", None)],
            next_cursor: None,
            index: HostSearchIndex::default(),
            title_scan_limited: true,
        })
    ));
    gui.run();
    assert!(gui.has_label(TITLE_SCAN_LIMITED_LABEL));
}

#[test]
fn enter_in_the_frame_of_an_edit_does_not_open_a_result_of_the_old_query() {
    // Given: results for the empty query, with beta first.
    let temp = tempfile::tempdir().unwrap();
    let (mut gui, requests) = fixture(temp.path());
    gui.run();
    gui.state_mut().open_thread_search();
    gui.run();
    assert!(requests.answer(0, found(vec![hit("beta", None), hit("alpha", None)])));
    gui.run();

    // When: a keystroke and Enter arrive in the same frame.
    gui.input_mut().events.push(egui::Event::Text("x".into()));
    gui.input_mut().events.push(press(Key::Enter));
    gui.run();

    // Then: the result for the old text is not opened; the search waits for the new text.
    assert_eq!(active(&gui), Some(ThreadId::new("alpha")));
    assert!(gui.state().thread_search().is_open());
    assert_eq!(requests.queries(), ["", "x"]);
}

#[test]
fn ime_composition_keys_are_isolated_but_do_not_drive_the_search() {
    // Given: a running chat behind the search, and results with beta first.
    let temp = tempfile::tempdir().unwrap();
    let (mut gui, requests) = fixture(temp.path());
    gui.run();
    start_chat(&mut gui, "alpha");
    gui.state_mut().open_thread_search();
    gui.run();
    assert!(requests.answer(0, found(vec![hit("beta", None), hit("alpha", None)])));
    gui.run();
    let ime = |event| egui::Event::Ime(event);

    // When: a conversion is open and the user navigates, confirms and cancels in it.
    gui.input_mut().events.push(ime(egui::ImeEvent::Preedit {
        text: "あ".into(),
        active_range_chars: None,
    }));
    gui.run();
    for key in [Key::ArrowDown, Key::Enter, Key::Escape] {
        gui.input_mut().events.push(press(key));
        gui.run();
    }
    // Then: the search neither moved, opened, closed nor let Esc stop the chat.
    assert!(gui.state().thread_search().is_open());
    assert_eq!(active(&gui), Some(ThreadId::new("alpha")));
    assert_eq!(requests.stops(), 0);

    // When: the conversion is committed in the same frame as its Enter and Esc.
    gui.input_mut().events.extend([
        ime(egui::ImeEvent::Commit("あ".into())),
        press(Key::Enter),
        press(Key::Escape),
    ]);
    gui.run();
    // Then: those keys were the IME's, not search commands.
    assert!(gui.state().thread_search().is_open());
    assert_eq!(active(&gui), Some(ThreadId::new("alpha")));
    assert_eq!(requests.stops(), 0);

    // When: Enter is pressed after the commit, Then it opens the untouched first result.
    // (The committed text changed the query, so wait for its results first.)
    gui.run();
    let last = requests.queries().len() - 1;
    assert!(requests.answer(last, found(vec![hit("beta", None), hit("alpha", None)])));
    gui.run();
    gui.key_press(Modifiers::NONE, Key::Enter);
    gui.run();
    assert_eq!(active(&gui), Some(ThreadId::new("beta")));
}

#[test]
fn search_leaves_enter_and_escape_to_a_modal_opened_above_it() {
    // Given: the search with results, then another modal opened over it.
    let temp = tempfile::tempdir().unwrap();
    let (mut gui, requests) = fixture(temp.path());
    gui.run();
    gui.state_mut().open_thread_search();
    gui.run();
    assert!(requests.answer(0, found(vec![hit("beta", None)])));
    gui.run();
    gui.state_mut().open_sandbox_settings();
    gui.run();

    // When: Enter and Esc are pressed.
    gui.key_press(Modifiers::NONE, Key::Enter);
    gui.run();
    gui.key_press(Modifiers::NONE, Key::Escape);
    gui.run();

    // Then: the search did not take them.
    assert!(gui.state().thread_search().is_open());
    assert_eq!(active(&gui), Some(ThreadId::new("alpha")));
}

#[test]
fn ctrl_k_opens_search_over_a_focused_composer_but_keeps_the_key_on_blocking_screens() {
    let temp = tempfile::tempdir().unwrap();
    let (mut gui, _requests) = fixture(temp.path());
    gui.run();
    gui.state_mut().composer_mut().focus_requested = true;
    gui.run();
    assert!(gui.focused_id().is_some());

    // When: Ctrl/Cmd+K is pressed while typing in the composer, Then search opens.
    gui.key_press(Modifiers::COMMAND, Key::K);
    gui.run();
    assert!(gui.state().thread_search().is_open());
    gui.key_press(Modifiers::COMMAND, Key::K);
    gui.run();
    assert!(!gui.state().thread_search().is_open());

    // The probe tells keys nothing consumed from the ones the search took.
    let unused_j = gui.watch_unconsumed(Key::J);
    let search_k = gui.watch_unconsumed(Key::K);
    gui.key_press(Modifiers::COMMAND, Key::J);
    gui.key_press(Modifiers::COMMAND, Key::K);
    gui.run();
    assert!(unused_j.load(Ordering::SeqCst));
    assert!(!search_k.load(Ordering::SeqCst));
    gui.key_press(Modifiers::COMMAND, Key::K);
    gui.run();
    assert!(!gui.state().thread_search().is_open());

    // When: a screen that cannot show the search is open, Then the key is left alone.
    gui.state_mut().open_add_project();
    gui.run();
    let blocked_k = gui.watch_unconsumed(Key::K);
    gui.key_press(Modifiers::COMMAND, Key::K);
    gui.run();
    assert!(!gui.state().thread_search().is_open());
    assert!(blocked_k.load(Ordering::SeqCst));
}

#[test]
fn search_reports_a_disabled_thread_index_instead_of_hiding_missing_body_hits() {
    let temp = tempfile::tempdir().unwrap();
    let (mut gui, requests) = fixture(temp.path());
    gui.run();
    gui.state_mut().open_thread_search();
    gui.run();
    let index = HostSearchIndex {
        enabled: Some(false),
        ..HostSearchIndex::default()
    };
    assert!(requests.answer(
        0,
        Ok(HostThreadSearch {
            hits: vec![hit("alpha", None)],
            next_cursor: None,
            index,
            title_scan_limited: false,
        })
    ));
    gui.run();
    assert!(gui.has_label(&index_notice(&index).unwrap()));
}

#[test]
fn search_without_a_runtime_says_it_is_unavailable() {
    let temp = tempfile::tempdir().unwrap();
    let mut gui = HeadlessWorkbench::new(state(temp.path()), [1200.0, 800.0]);
    gui.run();
    gui.state_mut().open_thread_search();
    gui.run();
    assert!(gui.has_label(UNAVAILABLE));
}
