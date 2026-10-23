use std::cell::RefCell;
use std::rc::Rc;

use super::*;
use crate::panes::agent_browser::{available, request_open};

/// Scripted pages; records what the workbench asked the surface to do.
#[derive(Default)]
struct Script {
    pages: Vec<String>,
    shown: Vec<(Vec<String>, Option<String>)>,
    pick: Option<String>,
    stops: usize,
}

struct FakeSurface(Rc<RefCell<Script>>);

impl AgentBrowserSurface for FakeSurface {
    fn pages(&mut self, _ctx: &egui::Context) -> Vec<String> {
        self.0.borrow().pages.clone()
    }

    fn render(&mut self, _ui: &mut egui::Ui, view: BrowserTabView<'_>) -> Option<String> {
        let mut script = self.0.borrow_mut();
        script.shown.push((
            view.runs.iter().map(|run| run.label.clone()).collect(),
            view.selected.map(str::to_owned),
        ));
        script.pick.take()
    }

    fn stop(&mut self, _ctx: &egui::Context) {
        self.0.borrow_mut().stops += 1;
    }
}

fn thread(id: &str, runs: &[&str]) -> workspace_ui::ThreadRecord {
    let mut thread = workspace_ui::ThreadRecord::new(
        workspace_ui::ThreadId::new(id),
        workspace_ui::ProjectId::new("project"),
        id,
    );
    thread.run_ids = runs.iter().map(|run| (*run).to_owned()).collect();
    thread
}

fn workbench() -> (
    WorkbenchState<crate::fixture::DemoSource>,
    Rc<RefCell<Script>>,
) {
    let mut state = WorkbenchState::new(
        crate::fixture::DemoSource(Vec::new()),
        &workspace_ui::UiSettings::default(),
    )
    .unwrap();
    state.sidebar.threads = vec![
        thread("first", &["root-1", "child-1", "child-2"]),
        thread("second", &["root-2"]),
    ];
    state.sidebar.active_thread = Some(workspace_ui::ThreadId::new("first"));
    let script = Rc::new(RefCell::new(Script::default()));
    state.set_agent_browser(Box::new(FakeSurface(Rc::clone(&script))));
    (state, script)
}

fn tab(state: &WorkbenchState<crate::fixture::DemoSource>) -> Option<egui_dock::TabPath> {
    state.dock.find_tab(&PanelId::new(PANEL))
}

fn open_page(script: &Rc<RefCell<Script>>, run: &str) {
    script.borrow_mut().pages.push(run.to_owned());
}

fn switch(state: &mut WorkbenchState<crate::fixture::DemoSource>, thread: &str) {
    state.sidebar.active_thread = Some(workspace_ui::ThreadId::new(thread));
}

fn browser_call(run: &str) -> event_bus::Event {
    event_bus::Event::new(event_bus::ToolEvent::ToolStarted {
        tool_name: "browser_click".into(),
        call_id: format!("call-{run}"),
        input: None,
        run_id: Some(run.into()),
    })
}

fn selected(state: &WorkbenchState<crate::fixture::DemoSource>, thread: &str) -> Option<String> {
    state.agent_browser.threads[thread].selected.clone()
}

#[test]
fn a_page_opened_in_the_active_thread_adds_the_tab_to_the_subagent_region() {
    // Given: a workbench whose keyboard focus is in the conversation.
    let (mut state, script) = workbench();
    let ctx = egui::Context::default();
    let conversation = state.dock.find_tab(&PanelId::new("agent-main")).unwrap();
    state
        .dock
        .set_focused_node_and_surface(conversation.node_path());

    // When: a run of the active thread opens its first page.
    open_page(&script, "child-1");
    state.sync_agent_browser(&ctx);

    // Then: the tab joins the Subagents leaf in front, focus stays put,
    // and the run is shown.
    let path = tab(&state).expect("browser tab");
    let region = state
        .dock
        .find_tab(&PanelId::new("subagents-home"))
        .unwrap();
    assert_eq!(path.node_path(), region.node_path());
    let leaf = state.dock.leaf(path.node_path()).unwrap();
    assert_eq!(leaf.active, path.tab);
    assert_eq!(state.dock.focused_leaf(), Some(conversation.node_path()));
    assert_eq!(
        state.panels[&PanelId::new(PANEL)].kind,
        PanelKind::AgentBrowser
    );
    assert_eq!(selected(&state, "first").as_deref(), Some("child-1"));
    assert!(available(&ctx));
}

#[test]
fn the_tab_shows_only_beside_threads_that_opened_a_page() {
    // Given: the second thread opens a page while the first is active.
    let (mut state, script) = workbench();
    let ctx = egui::Context::default();
    open_page(&script, "root-2");
    state.sync_agent_browser(&ctx);
    assert!(tab(&state).is_none());

    // When / Then: the tab follows the thread that has a page.
    switch(&mut state, "second");
    state.sync_agent_browser(&ctx);
    assert!(tab(&state).is_some());
    switch(&mut state, "first");
    state.sync_agent_browser(&ctx);
    assert!(tab(&state).is_none());
}

#[test]
fn a_closed_tab_returns_for_a_new_page_or_a_view_request() {
    // Given: the user closed the tab of a thread with a page.
    let (mut state, script) = workbench();
    let ctx = egui::Context::default();
    open_page(&script, "child-1");
    state.sync_agent_browser(&ctx);
    let path = tab(&state).unwrap();
    state.dock.remove_tab(path);
    state.agent_browser.closed_by_user();
    state.finish_agent_browser_frame(&ctx);

    // When: frames pass with no new page.
    state.sync_agent_browser(&ctx);
    // Then: it stays closed.
    assert!(tab(&state).is_none());

    // When: View is picked on a browser card of child-1.
    request_open(&ctx, "child-1");
    state.sync_agent_browser(&ctx);
    // Then: the tab is back.
    assert!(tab(&state).is_some());

    // When: it is closed again and another run opens a page.
    state.dock.remove_tab(tab(&state).unwrap());
    state.agent_browser.closed_by_user();
    state.finish_agent_browser_frame(&ctx);
    open_page(&script, "child-2");
    state.sync_agent_browser(&ctx);
    // Then: the tab returns on that run.
    assert!(tab(&state).is_some());
    assert_eq!(selected(&state, "first").as_deref(), Some("child-2"));
}

#[test]
fn the_tab_follows_the_run_that_last_used_the_browser() {
    // Given: two runs of the thread with pages; the later one is shown.
    let (mut state, script) = workbench();
    let ctx = egui::Context::default();
    open_page(&script, "child-1");
    state.sync_agent_browser(&ctx);
    open_page(&script, "child-2");
    state.sync_agent_browser(&ctx);
    assert_eq!(selected(&state, "first").as_deref(), Some("child-2"));

    // When: the first run calls a browser tool again.
    state.apply_events([browser_call("child-1")]);
    // Then: the tab moves back to it.
    assert_eq!(selected(&state, "first").as_deref(), Some("child-1"));

    // When: a run without a page calls one, or View names a run.
    state.apply_events([browser_call("root-1")]);
    assert_eq!(selected(&state, "first").as_deref(), Some("child-1"));
    request_open(&ctx, "child-2");
    state.sync_agent_browser(&ctx);
    // Then: only View moves it, since a run without a page has nothing to show.
    assert_eq!(selected(&state, "first").as_deref(), Some("child-2"));
}

#[test]
fn rendering_lists_the_threads_runs_and_applies_the_users_pick() {
    // Given: two runs with pages in the active thread.
    let (mut state, script) = workbench();
    let ctx = egui::Context::default();
    open_page(&script, "child-1");
    open_page(&script, "child-2");
    state.sync_agent_browser(&ctx);
    script.borrow_mut().pick = Some("child-1".into());

    // When: the tab draws.
    let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
        state
            .agent_browser
            .render(ui, Some("first"), |run| format!("agent {run}"));
    });
    output.textures_delta.clear();
    state.finish_agent_browser_frame(&ctx);

    // Then: the surface saw both runs, the pick took effect, and nothing stopped.
    let shown = script.borrow().shown.clone();
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0].0, ["agent child-1", "agent child-2"]);
    assert_eq!(selected(&state, "first").as_deref(), Some("child-1"));
    assert_eq!(script.borrow().stops, 0);
}

#[test]
fn an_undrawn_tab_stops_its_screencast() {
    let (mut state, script) = workbench();
    let ctx = egui::Context::default();
    open_page(&script, "child-1");
    state.sync_agent_browser(&ctx);
    // The dock did not draw the tab (another tab of its leaf was in front).
    state.finish_agent_browser_frame(&ctx);
    assert_eq!(script.borrow().stops, 1);
}

#[test]
fn without_an_agent_browser_there_is_no_tab_or_view_button() {
    let mut state = WorkbenchState::new(
        crate::fixture::DemoSource(Vec::new()),
        &workspace_ui::UiSettings::default(),
    )
    .unwrap();
    state.sidebar.threads = vec![thread("first", &["root-1"])];
    state.sidebar.active_thread = Some(workspace_ui::ThreadId::new("first"));
    let ctx = egui::Context::default();
    request_open(&ctx, "root-1");
    state.sync_agent_browser(&ctx);
    assert!(tab(&state).is_none());
    assert!(!available(&ctx));
}

#[test]
fn a_saved_layout_does_not_bring_back_the_tab() {
    // Given: a layout saved while the browser tab was open.
    let (mut state, script) = workbench();
    let ctx = egui::Context::default();
    open_page(&script, "child-1");
    state.sync_agent_browser(&ctx);
    let workspace = crate::dock::from_dock_state(&state.dock, &state.panels).unwrap();
    let settings = workspace_ui::UiSettings {
        layout: workspace_ui::LayoutSettings {
            workspace: Some(workspace),
        },
        ..Default::default()
    };

    // When: the GUI starts from it.
    let restored = WorkbenchState::new(crate::fixture::DemoSource(Vec::new()), &settings).unwrap();

    // Then: no page survives a restart, so neither does its tab.
    assert!(restored.dock.find_tab(&PanelId::new(PANEL)).is_none());
    assert!(!restored.panels.contains_key(&PanelId::new(PANEL)));
}

#[test]
fn coming_back_to_a_thread_shows_the_tab_as_it_was_left() {
    let in_front = |state: &WorkbenchState<crate::fixture::DemoSource>| {
        let path = tab(state).unwrap();
        state.dock.leaf(path.node_path()).unwrap().active == path.tab
    };
    // Given: the first thread's tab in front of the Subagents tab.
    let (mut state, script) = workbench();
    let ctx = egui::Context::default();
    open_page(&script, "child-1");
    state.sync_agent_browser(&ctx);
    assert!(in_front(&state));

    // When: the user leaves and comes back.
    switch(&mut state, "second");
    state.sync_agent_browser(&ctx);
    switch(&mut state, "first");
    state.sync_agent_browser(&ctx);
    // Then: it is still in front.
    assert!(in_front(&state));

    // When: the user brings the Subagents tab forward, leaves and comes back.
    state.focus_panel("subagents-home");
    switch(&mut state, "second");
    state.sync_agent_browser(&ctx);
    switch(&mut state, "first");
    state.sync_agent_browser(&ctx);
    // Then: the browser tab stays behind it.
    assert!(!in_front(&state));
}
