//! The agent browser tab opens beside the conversation whose agent opened a
//! page, and the View button on a browser tool card brings it back.
#![cfg(feature = "browser")]

use std::sync::{Arc, Mutex};

use event_bus::{Event, LifecycleEvent, ToolEvent};
use gui::browser::{Activity, AgentBrowserPane, AgentView};
use gui::fixture::DemoSource;
use serde_json::json;
use workspace_ui::{ProjectId, SidebarState, ThreadId};

/// Pages the test opens and closes; records what the pane watched.
#[derive(Clone, Default)]
struct Pages {
    open: Arc<Mutex<Vec<String>>>,
    observed: Arc<Mutex<Vec<Option<String>>>>,
}

struct ScriptedView(Pages);

impl AgentView for ScriptedView {
    fn pages(&mut self, _ctx: &egui::Context) -> Vec<String> {
        self.0.open.lock().unwrap().clone()
    }

    fn observe(&mut self, key: Option<&str>, _ctx: &egui::Context) {
        self.0.observed.lock().unwrap().push(key.map(str::to_owned));
    }

    fn poll_frame(&mut self) -> Option<egui::ColorImage> {
        None
    }

    fn tab(&self) -> Option<String> {
        Some("t1".into())
    }

    fn activity(&self) -> Vec<Activity> {
        vec![Activity::Action {
            label: "navigate https://example.test/".into(),
            error: None,
        }]
    }

    fn error(&self) -> Option<String> {
        None
    }
}

fn workbench(pages: &Pages) -> gui::headless::HeadlessWorkbench<DemoSource> {
    let dir = tempfile::tempdir().unwrap();
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("project");
    sidebar
        .add_project(project.clone(), "project", dir.path())
        .unwrap();
    sidebar.select_project(&project).unwrap();
    for thread in ["one", "two"] {
        sidebar
            .create_thread(ThreadId::new(thread), project.clone(), thread)
            .unwrap();
    }
    sidebar.switch_thread(&ThreadId::new("one")).unwrap();
    let mut state =
        gui::app::WorkbenchState::new(DemoSource(Vec::new()), &workspace_ui::UiSettings::default())
            .unwrap()
            .with_sidebar(sidebar);
    state.set_agent_browser(Box::new(AgentBrowserPane::new(ScriptedView(pages.clone()))));
    state.apply_events([Event::new(LifecycleEvent::AgentRunStarted {
        run_id: "run-1".into(),
        parent_run_id: None,
        agent_name: "chat:Worker:one".into(),
        role: "Worker".into(),
    })]);
    let mut gui = gui::headless::HeadlessWorkbench::new(state, [1600.0, 1000.0]);
    gui.run();
    std::mem::forget(dir);
    gui
}

fn navigate(run: &str) -> [Event; 2] {
    [
        Event::new(ToolEvent::ToolStarted {
            tool_name: "browser_navigate".into(),
            call_id: format!("{run}-navigate"),
            input: Some(json!({"url": "https://example.test/"})),
            run_id: Some(run.into()),
        }),
        Event::new(ToolEvent::ToolCompleted {
            tool_name: "browser_navigate".into(),
            call_id: format!("{run}-navigate"),
            is_error: false,
            output: Some("- heading \"Example\"".into()),
            detail: None,
            run_id: Some(run.into()),
        }),
    ]
}

#[test]
fn a_page_opens_the_browser_tab_and_view_reopens_it_after_closing() {
    // Given: the conversation's run navigates, which opens its page.
    let pages = Pages::default();
    let mut gui = workbench(&pages);
    assert!(!gui.has_label("Showing the active tab t1"));
    gui.state_mut().apply_events(navigate("run-1"));
    pages.open.lock().unwrap().push("run-1".into());
    gui.run();
    gui.run();

    // Then: the Browser tab shows the page beside the conversation.
    assert!(gui.has_label("Showing the active tab t1"));
    assert!(gui.has_label("navigate https://example.test/"));
    assert_eq!(
        pages.observed.lock().unwrap().as_slice(),
        [Some("run-1".to_owned())]
    );
    assert!(gui.has_label("View"), "browser cards offer View");

    // When: another conversation is opened.
    gui.state_mut().switch_thread(ThreadId::new("two")).unwrap();
    gui.run();
    gui.run();
    // Then: the tab leaves with its conversation and the screencast stops.
    assert!(!gui.has_label("Showing the active tab t1"));
    assert_eq!(pages.observed.lock().unwrap().last(), Some(&None));

    // When: the page closes while away, and the conversation is opened again.
    pages.open.lock().unwrap().clear();
    gui.state_mut().switch_thread(ThreadId::new("one")).unwrap();
    gui.run();
    gui.run();
    // Then: the tab is back and says the page closed.
    assert!(gui.has_label("The page was closed."));

    // When: View on the card is clicked after the page reopens.
    pages.open.lock().unwrap().push("run-1".into());
    gui.click_label("View");
    gui.run();
    gui.run();
    // Then: the page is watched again.
    assert!(gui.has_label("Showing the active tab t1"));
    assert_eq!(
        pages.observed.lock().unwrap().last(),
        Some(&Some("run-1".to_owned()))
    );
    if let Some(directory) = std::env::var_os("EVORCH_AGENT_BROWSER_CAPTURE_DIR") {
        gui.capture()
            .expect("render workbench")
            .save_png(&std::path::Path::new(&directory).join("agent-browser-tab.png"))
            .expect("save capture");
    }
}
