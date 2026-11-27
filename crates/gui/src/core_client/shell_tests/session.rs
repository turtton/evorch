use std::path::Path;

use egui::{Key, Modifiers};
use workbench_protocol::namespaces::ExecutionQuery;

use super::*;
use crate::core_client::persistence::{ClientStateStore, SavedSelection, SavedState};

fn selection(project: &str, thread: Option<&str>) -> SavedSelection {
    SavedSelection {
        project: ProjectId::new(project).unwrap(),
        thread: thread.map(|thread| ThreadId::new(thread).unwrap()),
    }
}

fn save_selection(dir: &Path, saved: SavedSelection) -> ClientStateStore {
    let store = ClientStateStore::in_config_dir(dir);
    store
        .save(&SavedState {
            selection: Some(saved),
            ..SavedState::default()
        })
        .unwrap();
    store
}

fn restored(store: ClientStateStore, connected: bool) -> Screen {
    let (tx, actions) = tokio::sync::mpsc::channel(256);
    let shared = Arc::new(Mutex::new(Shared {
        connected,
        ..populated()
    }));
    let screen = CoreClientScreen::new(shared.clone(), tx, None, None)
        .with_local_panes(super::super::local::LocalPanes::new(
            Arc::new(crate::diff::FixtureDiffSource::empty()),
            None,
        ))
        .with_state_store(store);
    let harness = Harness::builder()
        .with_size(egui::vec2(1440.0, 900.0))
        .build_ui_state(|ui, screen: &mut CoreClientScreen| screen.ui(ui), screen);
    crate::theme::style::install_preset(&harness.ctx, crate::theme::style::ThemePreset::Graphite);
    Screen {
        harness,
        shared,
        actions,
    }
}

fn first_pages(actions: &[Action]) -> Vec<&ThreadId> {
    actions
        .iter()
        .filter_map(|action| match action {
            Action::History(thread, None, false) => Some(thread),
            _ => None,
        })
        .collect()
}

fn connect(screen: &mut Screen) -> Vec<Action> {
    screen.shared.lock().unwrap().connected = true;
    screen.settle()
}

// Given: a session file that kept thr-2 of proj-1, and a window not yet connected
// When: the window draws, then the first snapshot is in
// Then: nothing is selected or read before; after, thr-2 is selected and its history is
//       asked for once
#[test]
fn the_kept_thread_is_selected_once_the_core_is_connected() {
    let dir = tempfile::tempdir().unwrap();
    let store = save_selection(dir.path(), selection("proj-1", Some("thr-2")));
    let mut screen = restored(store, false);

    screen.harness.run_steps(2);
    let mut before = Vec::new();
    while let Ok(action) = screen.actions.try_recv() {
        before.push(action);
    }
    assert!(first_pages(&before).is_empty());
    assert_eq!(screen.harness.state().selected, None);

    let actions = connect(&mut screen);

    let thr_2 = ThreadId::new("thr-2").unwrap();
    assert_eq!(first_pages(&actions), [&thr_2]);
    let state = screen.harness.state();
    assert_eq!(state.selected, Some(thr_2));
    assert_eq!(state.project, Some(ProjectId::new("proj-1").unwrap()));
}

// Given: a session file that kept a thread the core no longer has
// When: the window connects
// Then: only its project is selected, and no history is asked for
#[test]
fn a_kept_thread_the_core_lost_selects_only_its_project() {
    let dir = tempfile::tempdir().unwrap();
    let store = save_selection(dir.path(), selection("proj-1", Some("thr-gone")));
    let mut screen = restored(store, false);
    screen.harness.run_steps(1);

    let actions = connect(&mut screen);

    assert!(first_pages(&actions).is_empty());
    let state = screen.harness.state();
    assert_eq!(state.selected, None);
    assert_eq!(state.project, Some(ProjectId::new("proj-1").unwrap()));
}

// Given: a window with a session file, Lanes closed
// When: Ctrl+S is pressed, then Ctrl+Shift+R
// Then: the file keeps the layout without Lanes, and the reset brings Lanes back
#[test]
fn ctrl_s_saves_the_layout_and_ctrl_shift_r_resets_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = ClientStateStore::in_config_dir(dir.path());
    let mut screen = restored(store.clone(), true);
    screen.settle();
    let lanes = CoreTab::Feature(FeatureId::Lanes);
    let dock = &mut screen.harness.state_mut().dock;
    let path = dock.find_tab(&lanes).unwrap();
    dock.remove_tab(path);

    screen
        .harness
        .key_press_modifiers(Modifiers::COMMAND, Key::S);
    screen.harness.run_steps(1);

    let saved = store.load().expect("a saved file");
    let mut files = super::super::local::files::FileTabs::default();
    let dock = crate::core_client::persistence::restore_dock(saved.layout.as_ref(), &mut files);
    assert!(dock.find_tab(&lanes).is_none());
    assert!(dock.find_tab(&CoreTab::Conversation).is_some());

    screen
        .harness
        .key_press_modifiers(Modifiers::COMMAND | Modifiers::SHIFT, Key::R);
    screen.harness.run_steps(1);
    assert!(screen.harness.state().dock.find_tab(&lanes).is_some());
}

// Given: Browser in front of the conversation's leaf, the Terminal folded
// When: Ctrl+1 is pressed, then Ctrl+2
// Then: the Conversation comes to the front of its leaf, then the Terminal opens in front
#[test]
fn ctrl_1_and_ctrl_2_bring_the_conversation_and_the_terminal_forward() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    shell::focus(
        &mut screen.harness.state_mut().dock,
        CoreTab::Feature(FeatureId::Browser),
    );
    assert_ne!(
        screen.front_of_leaf(CoreTab::Conversation),
        CoreTab::Conversation
    );
    assert!(screen.terminal_collapsed());

    screen
        .harness
        .key_press_modifiers(Modifiers::COMMAND, Key::Num1);
    screen.harness.run_steps(1);
    assert_eq!(
        screen.front_of_leaf(CoreTab::Conversation),
        CoreTab::Conversation
    );

    screen
        .harness
        .key_press_modifiers(Modifiers::COMMAND, Key::Num2);
    screen.harness.run_steps(1);
    assert!(!screen.terminal_collapsed());
    assert_eq!(screen.front_of_leaf(CoreTab::Terminal), CoreTab::Terminal);
}

// Given: a restored window that never connected
// When: its state is saved
// Then: the kept selection is saved again, not lost
#[test]
fn saving_before_connecting_keeps_the_kept_selection() {
    let dir = tempfile::tempdir().unwrap();
    let kept = selection("proj-1", Some("thr-2"));
    let store = save_selection(dir.path(), kept.clone());
    let mut screen = restored(store.clone(), false);
    screen.harness.run_steps(1);

    screen.harness.state().save_state();

    assert_eq!(store.load().unwrap().selection, Some(kept));
}

// Given: a session file that kept a layout without Lanes and Memory, a wider sidebar, a
//        local file in front of the right-hand area, and thr-1 selected
// When: the window opens and connects
// Then: the dock is that layout, the file is shown in its tab, and thr-1's conversation
//       is back
#[test]
fn a_kept_layout_reopens_with_its_file_tab_and_thread() {
    let dir = tempfile::tempdir().unwrap();
    let notes = dir.path().join("NOTES.md");
    std::fs::write(
        &notes,
        "# Redirect fix\n\n- honour `next` after login\n- fall back to /home\n",
    )
    .unwrap();
    let mut dock = shell::default_dock();
    for feature in [FeatureId::Lanes, FeatureId::Memory] {
        let path = dock.find_tab(&CoreTab::Feature(feature)).unwrap();
        dock.remove_tab(path);
    }
    if let Ok(egui_dock::Node::Horizontal(split)) = dock.node_mut(egui_dock::NodePath::new(
        egui_dock::SurfaceIndex::main(),
        egui_dock::NodeIndex::root(),
    )) {
        split.fraction = 0.36;
    }
    let mut files = super::super::local::files::FileTabs::default();
    let file = CoreTab::File(files.open(notes));
    shell::place_beside(&mut dock, file, CoreTab::Feature(FeatureId::Execution));
    shell::focus(&mut dock, file);
    let store = ClientStateStore::in_config_dir(&dir.path().join("config"));
    store
        .save(&SavedState {
            layout: crate::core_client::persistence::extract(
                &dock,
                &files,
                &crate::core_client::persistence::WindowRects::default(),
            ),
            selection: Some(selection("proj-1", Some("thr-1"))),
            ..SavedState::default()
        })
        .unwrap();
    let mut screen = restored(store, false);
    screen.harness.run_steps(1);

    connect(&mut screen);
    screen.capture("restored");

    let state = screen.harness.state();
    assert_eq!(state.selected, Some(ThreadId::new("thr-1").unwrap()));
    for feature in [FeatureId::Lanes, FeatureId::Memory] {
        assert!(state.dock.find_tab(&CoreTab::Feature(feature)).is_none());
    }
    let front = screen.front_of_leaf(CoreTab::Feature(FeatureId::Execution));
    assert!(matches!(front, CoreTab::File(_)), "{front:?}");
    assert!(screen.terminal_collapsed());
    screen.harness.get_by_label("Redirect fix");
}

// Given: a session file that kept run tabs of thr-2 and of a thread the core no longer has
// When: the window connects and its state is saved
// Then: only thr-2's tab is kept; before connecting both were kept
#[test]
fn run_tabs_of_threads_the_core_lost_are_dropped_once_connected() {
    let dir = tempfile::tempdir().unwrap();
    let pane = |thread: &str, run: &str| crate::core_client::run_tabs::RunPane {
        thread: ThreadId::new(thread).unwrap(),
        run: RunId::new(run).unwrap(),
        name: String::new(),
        child: true,
    };
    let kept = pane("thr-2", "run-kept");
    let store = ClientStateStore::in_config_dir(dir.path());
    store
        .save(&SavedState {
            run_tabs: vec![pane("thr-gone", "run-gone"), kept.clone()],
            ..SavedState::default()
        })
        .unwrap();
    let mut screen = restored(store.clone(), false);
    screen.harness.run_steps(1);
    screen.harness.state().save_state();
    assert_eq!(store.load().unwrap().run_tabs.len(), 2);

    connect(&mut screen);
    screen.harness.state().save_state();

    assert_eq!(store.load().unwrap().run_tabs, [kept]);
}

// Given: a session file another build wrote at version 2
// When: the window draws and its state is saved as it closes
// Then: the file's bytes are left as they were
#[test]
fn closing_keeps_a_file_this_build_cannot_read() {
    let dir = tempfile::tempdir().unwrap();
    let store = ClientStateStore::in_config_dir(dir.path());
    let bytes = br#"{"version": 2, "layout": null}"#;
    std::fs::write(store.path(), bytes).unwrap();
    let mut screen = restored(store.clone(), true);
    screen.harness.run_steps(1);

    screen.harness.state().save_state();

    assert_eq!(std::fs::read(store.path()).unwrap(), bytes);
}

// Given: a session file with Plan in a window at 400x300 and Diff in one saved too small
// When: the window draws, Plan's window is folded, and the state is saved
// Then: Plan's window keeps the rect it had open, and Diff's is saved at the usable size
//       egui opened it at
#[test]
fn a_floating_window_keeps_its_open_rect() {
    let dir = tempfile::tempdir().unwrap();
    let store = ClientStateStore::in_config_dir(dir.path());
    let window = |kind: &str, width: f32, height: f32| {
        serde_json::json!({
            "rect": {"x": 200.0, "y": 150.0, "width": width, "height": height},
            "root": {"type": "tabs", "tabs": [{"kind": kind}]},
        })
    };
    let main = serde_json::json!({
        "type": "split", "direction": "horizontal", "fraction": 0.3,
        "first": {"type": "tabs", "tabs": [{"kind": "projects"}]},
        "second": {"type": "tabs", "tabs": [{"kind": "conversation"}, {"kind": "notifications"}]},
    });
    let file = serde_json::json!({"version": 1, "layout": {
        "main": main,
        "windows": [window("plan", 400.0, 300.0), window("diff", 1.0, 1.0)],
    }});
    std::fs::write(store.path(), serde_json::to_vec(&file).unwrap()).unwrap();
    let mut screen = restored(store.clone(), true);
    screen.settle();
    let dock = &mut screen.harness.state_mut().dock;
    let plan = dock.find_tab(&CoreTab::Plan).unwrap();
    dock.leaf_mut(plan.node_path()).unwrap().collapsed = true;
    screen.harness.run_steps(1);

    screen.harness.state().save_state();

    let windows = store.load().unwrap().layout.unwrap().windows;
    let rect = |kind: &str| {
        windows
            .iter()
            .find(|window| serde_json::to_value(&window.root).unwrap()["tabs"][0]["kind"] == kind)
            .and_then(|window| window.rect)
            .unwrap_or_else(|| panic!("{kind} has a rect"))
    };
    let plan = rect("plan");
    assert_eq!((plan.x, plan.y), (200.0, 150.0));
    assert!((plan.width - 400.0).abs() < 1.0, "{plan:?}");
    assert!((plan.height - 300.0).abs() < 1.0, "{plan:?}");
    let diff = rect("diff");
    assert!(diff.width >= 64.0 && diff.height >= 64.0, "{diff:?}");
}

const LANE_ROW: &str = "↳ Auth regression tests";

// Given: a session file that kept thr-1, the lane's lead, folded
// When: the window opens and connects
// Then: the lane stays hidden under it, while without the fold it is shown
#[test]
fn a_thread_folded_before_the_restart_stays_folded() {
    let dir = tempfile::tempdir().unwrap();
    let store = ClientStateStore::in_config_dir(dir.path());
    store
        .save(&SavedState {
            folded_threads: vec![ThreadId::new("thr-1").unwrap()],
            ..SavedState::default()
        })
        .unwrap();
    let mut folded = restored(store, true);
    folded.settle();
    folded.harness.get_by_label("Expand children of thr-1");
    assert!(folded.harness.query_by_label(LANE_ROW).is_none());

    let dir = tempfile::tempdir().unwrap();
    let mut open = restored(ClientStateStore::in_config_dir(dir.path()), true);
    open.settle();
    open.harness.get_by_label(LANE_ROW);
}

// Given: a window whose lead thread is shown unfolded
// When: the person folds it and the state is saved, then a new window reads the file
// Then: the new window shows the thread folded, and unfolding it there saves it unfolded
#[test]
fn folding_and_unfolding_a_thread_is_kept_across_windows() {
    let dir = tempfile::tempdir().unwrap();
    let store = ClientStateStore::in_config_dir(dir.path());
    store.save(&SavedState::default()).unwrap();
    let mut first = restored(store.clone(), true);
    first.settle();
    first
        .harness
        .get_by_label("Collapse children of thr-1")
        .click();
    first.settle();
    first.harness.state().save_state();
    assert_eq!(
        store.load().unwrap().folded_threads,
        [ThreadId::new("thr-1").unwrap()]
    );

    let mut second = restored(store.clone(), true);
    second.settle();
    second
        .harness
        .get_by_label("Expand children of thr-1")
        .click();
    second.settle();
    second.harness.state().save_state();
    assert!(store.load().unwrap().folded_threads.is_empty());
}

// Given: a session file that kept a named subagent tab of thr-1 while a file tab hid the
//        Subagents tab, and the core lists that run as live with a model
// When: the window connects
// Then: the run is read once although the tab has its name, and the answer's model is the
//       tab's, shown under its title
#[test]
fn a_restored_named_subagent_tab_reads_its_model_while_subagents_stays_hidden() {
    let dir = tempfile::tempdir().unwrap();
    let notes = dir.path().join("NOTES.md");
    std::fs::write(&notes, "# Notes\n").unwrap();
    let mut dock = shell::default_dock();
    let mut files = super::super::local::files::FileTabs::default();
    let file = CoreTab::File(files.open(notes));
    shell::place_beside(&mut dock, file, CoreTab::Feature(FeatureId::Execution));
    shell::focus(&mut dock, file);
    let pane = crate::core_client::run_tabs::RunPane {
        thread: ThreadId::new("thr-1").unwrap(),
        run: RunId::new("run-9").unwrap(),
        name: "tester".into(),
        child: true,
    };
    let store = ClientStateStore::in_config_dir(&dir.path().join("config"));
    store
        .save(&SavedState {
            layout: crate::core_client::persistence::extract(
                &dock,
                &files,
                &crate::core_client::persistence::WindowRects::default(),
            ),
            run_tabs: vec![pane],
            selection: Some(selection("proj-1", Some("thr-1"))),
            ..SavedState::default()
        })
        .unwrap();
    let mut screen = restored(store, false);
    let run: RunSummary = view(json!({
        "id": "run-9", "thread": "thr-1", "parent_run": "run-1",
        "role": "worker", "phase": "running", "revision": "4",
    }));
    screen
        .shared
        .lock()
        .unwrap()
        .cache
        .runs
        .insert("run-9".into(), run.clone());
    screen.harness.run_steps(1);
    let mut asked = connect(&mut screen);

    let mut reads = Vec::new();
    for _ in 0..4 {
        screen.harness.run_steps(2);
        asked.extend(screen.settle());
        for action in std::mem::take(&mut asked) {
            if let Action::Feature(FeatureAction {
                feature: FeatureId::Execution,
                tag,
                request: FeatureRequest::Query(Query::Execution(ExecutionQuery::GetRun(target))),
            }) = action
            {
                let detail = json!({"summary": serde_json::to_value(&run).unwrap(),
                    "name": "tester", "lifecycle": "active", "source": "live",
                    "model": "work/gpt-5", "child_count": 0, "revision": "4"});
                let reply = FeatureReply::Query(Ok(view(
                    json!({"type": "execution", "payload": {"type": "run", "payload": detail}}),
                )));
                screen
                    .shared
                    .lock()
                    .unwrap()
                    .features
                    .reply(FeatureId::Execution, tag, reply);
                reads.push(target.run.as_str().to_owned());
            }
        }
    }
    screen.harness.run_steps(2);

    assert_eq!(reads, ["run-9"]);
    let state = screen.harness.state();
    let tab = state
        .run_tabs
        .iter()
        .find(|(_, tab)| tab.run.as_str() == "run-9")
        .map(|(_, tab)| tab.model.clone());
    assert_eq!(tab, Some(Some("work/gpt-5".to_owned())));
}
