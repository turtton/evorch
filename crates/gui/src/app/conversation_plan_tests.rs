use std::sync::Mutex;

use event_bus::PlanChangeCause;
use workbench_protocol::namespaces::{ExecutionQuery, ExecutionReply};

use super::*;
use crate::fixture::{DemoSource, FixturePlanReader, PlanScenario};
use crate::model::conversation_plan::{PlanReader, ReadFailure, inline_spawner};

/// Answers with the mixed fixture plan and records each query's kind and thread.
#[derive(Default)]
struct Recorder(Mutex<Vec<(&'static str, String)>>);

struct RecordingReader(Arc<Recorder>);

impl PlanReader for RecordingReader {
    fn read(&self, query: &ExecutionQuery) -> Result<ExecutionReply, ReadFailure> {
        let entry = match query {
            ExecutionQuery::GetConversationPlan(target) => ("summary", target.thread.to_string()),
            ExecutionQuery::ReadConversationPlanTree(query) => ("tree", query.thread.to_string()),
            ExecutionQuery::GetConversationPlanTask(query) => ("task", query.thread.to_string()),
            ExecutionQuery::ReadConversationPlanHistory(query) => {
                ("history", query.thread.to_string())
            }
            _ => ("other", String::new()),
        };
        self.0.0.lock().unwrap().push(entry);
        FixturePlanReader::new(PlanScenario::Mixed).read(query)
    }
}

impl Recorder {
    fn take(&self) -> Vec<(&'static str, String)> {
        std::mem::take(&mut self.0.lock().unwrap())
    }
}

fn workbench() -> (WorkbenchState<DemoSource>, Arc<Recorder>) {
    let recorder = Arc::new(Recorder::default());
    let model = ConversationPlanModel::default()
        .with_reader(Arc::new(RecordingReader(Arc::clone(&recorder))))
        .with_spawner(inline_spawner());
    let mut state =
        WorkbenchState::new(DemoSource(Vec::new()), &workspace_ui::UiSettings::default())
            .unwrap()
            .with_conversation_plan(model);
    state.sidebar.active_thread = Some(workspace_ui::ThreadId::new("first"));
    (state, recorder)
}

fn tab_count(state: &WorkbenchState<DemoSource>) -> usize {
    state
        .dock
        .iter_all_tabs()
        .filter(|(_, tab)| tab.as_str() == PANEL)
        .count()
}

fn changed(thread: &str, revision: u64) -> Event {
    Event::new(EventKind::ConversationPlan(
        ConversationPlanEvent::Changed {
            thread_id: thread.into(),
            plan_id: "plan-demo".into(),
            revision,
            cause: PlanChangeCause::ModelUpdate,
            source_run_id: None,
            changes: Vec::new(),
            progress: event_bus::PlanProgress::default(),
        },
    ))
}

#[test]
fn opening_adds_one_tab_to_the_subagent_region_and_focuses_it() {
    // Given: a workbench whose layout has the Subagents leaf.
    let (mut state, _) = workbench();
    assert_eq!(tab_count(&state), 0, "the default layout has no plan tab");

    // When: the band's open button is used twice.
    state.open_conversation_plan_tab();
    state.open_conversation_plan_tab();

    // Then: one tab sits beside the subagents, in front.
    assert_eq!(tab_count(&state), 1);
    let path = state.dock.find_tab(&PanelId::new(PANEL)).unwrap();
    let region = state
        .dock
        .find_tab(&PanelId::new("subagents-home"))
        .unwrap();
    assert_eq!(path.node_path(), region.node_path());
    assert_eq!(state.dock.leaf(path.node_path()).unwrap().active, path.tab);
    assert_eq!(
        state.panels[&PanelId::new(PANEL)].kind,
        PanelKind::ConversationPlan
    );
}

#[test]
fn the_opened_tab_sits_right_after_subagents_with_the_bar_scrolled_back() {
    // Given: a subagent region with transcripts after the Subagents tab, its
    // tab bar scrolled toward the end as an overflowing bar can be.
    let (mut state, _) = workbench();
    let region = state
        .dock
        .find_tab(&PanelId::new("subagents-home"))
        .unwrap();
    {
        let leaf = state.dock.leaf_mut(region.node_path()).unwrap();
        leaf.tabs.extend(["agent-a", "agent-b"].map(PanelId::new));
        leaf.scroll = -240.0;
    }

    // When: the plan is opened.
    state.open_conversation_plan_tab();

    // Then: the tab follows Subagents, ahead of the transcripts, and the bar
    // starts at its beginning, so its title and close button are in view.
    let path = state.dock.find_tab(&PanelId::new(PANEL)).unwrap();
    assert_eq!(path.tab.0, region.tab.0 + 1);
    let leaf = state.dock.leaf(path.node_path()).unwrap();
    assert_eq!(leaf.active, path.tab);
    assert_eq!(leaf.scroll, 0.0);

    // Opening again after the bar scrolled away shows it again.
    state.dock.leaf_mut(path.node_path()).unwrap().scroll = -240.0;
    state.open_conversation_plan_tab();
    assert_eq!(state.dock.leaf(path.node_path()).unwrap().scroll, 0.0);
}

#[test]
fn plan_updates_neither_open_the_tab_nor_bring_a_closed_one_back() {
    let (mut state, recorder) = workbench();
    let ctx = egui::Context::default();
    state.poll_conversation_plan(&ctx);

    // A change while the tab was never opened: only the band's summary is read.
    recorder.take();
    state.fold_conversation_plan_event(&changed("first", 20));
    state.poll_conversation_plan(&ctx);
    assert_eq!(tab_count(&state), 0);
    assert_eq!(recorder.take(), vec![("summary", "first".to_owned())]);

    // The user opens the tab, then closes it.
    state.open_conversation_plan_tab();
    let path = state.dock.find_tab(&PanelId::new(PANEL)).unwrap();
    state.dock.remove_tab(path);

    // A later change leaves it closed.
    state.fold_conversation_plan_event(&changed("first", 21));
    state.poll_conversation_plan(&ctx);
    assert_eq!(tab_count(&state), 0);
}

#[test]
fn a_saved_layout_does_not_bring_back_the_tab() {
    // Given: a layout saved while the plan tab was open.
    let (mut state, _) = workbench();
    state.open_conversation_plan_tab();
    let workspace = crate::dock::from_dock_state(&state.dock, &state.panels).unwrap();
    let settings = workspace_ui::UiSettings {
        layout: workspace_ui::LayoutSettings {
            workspace: Some(workspace),
        },
        ..Default::default()
    };

    // When: the GUI starts from it.
    let restored = WorkbenchState::new(DemoSource(Vec::new()), &settings).unwrap();

    // Then: the tab opens again only on request.
    assert!(restored.dock.find_tab(&PanelId::new(PANEL)).is_none());
    assert!(!restored.panels.contains_key(&PanelId::new(PANEL)));
}

#[test]
fn the_band_follows_the_shown_conversation() {
    let (mut state, recorder) = workbench();
    let ctx = egui::Context::default();
    state.poll_conversation_plan(&ctx);
    let band = state.conversation_plan.band().unwrap();
    assert_eq!(band.summary.unwrap().thread.to_string(), "first");

    // When: the user switches to another conversation.
    recorder.take();
    state.sidebar.active_thread = Some(workspace_ui::ThreadId::new("second"));
    state.poll_conversation_plan(&ctx);

    // Then: its plan is read and shown, and the first one's is gone.
    assert_eq!(recorder.take(), vec![("summary", "second".to_owned())]);
    let band = state.conversation_plan.band().unwrap();
    assert_eq!(band.summary.unwrap().thread.to_string(), "second");

    // A change of the conversation no longer shown is not read.
    state.fold_conversation_plan_event(&changed("first", 30));
    state.poll_conversation_plan(&ctx);
    assert!(recorder.take().is_empty());
}

#[test]
fn a_late_notification_does_not_read_again() {
    let (mut state, recorder) = workbench();
    let ctx = egui::Context::default();
    state.poll_conversation_plan(&ctx);
    let revision = state
        .conversation_plan()
        .state()
        .summary()
        .unwrap()
        .revision;
    recorder.take();

    // When: a notification older than (or equal to) what is shown arrives.
    state.fold_conversation_plan_event(&changed("first", revision.0));
    state.fold_conversation_plan_event(&changed("first", revision.0 - 1));
    state.poll_conversation_plan(&ctx);

    // Then: nothing is read and the shown revision stays.
    assert!(recorder.take().is_empty());
    assert_eq!(
        state
            .conversation_plan()
            .state()
            .summary()
            .unwrap()
            .revision,
        revision
    );
}

#[test]
fn without_a_reader_there_is_no_band() {
    let mut state =
        WorkbenchState::new(DemoSource(Vec::new()), &workspace_ui::UiSettings::default()).unwrap();
    state.sidebar.active_thread = Some(workspace_ui::ThreadId::new("first"));
    state.poll_conversation_plan(&egui::Context::default());
    assert!(state.conversation_plan.band().is_none());
}

fn render_panel(state: &mut WorkbenchState<DemoSource>, ctx: &egui::Context) {
    // The theme takes effect from the next pass; the panel needs its text styles.
    crate::theme::install(ctx);
    ctx.run_ui(egui::RawInput::default(), |_| {})
        .textures_delta
        .clear();
    ctx.run_ui(egui::RawInput::default(), |ui| {
        state.conversation_plan.render(ui);
    })
    .textures_delta
    .clear();
}

#[test]
fn a_lagged_event_subscription_reads_the_shown_plan_again() {
    // Given: a pump whose subscription overflowed before it could read, so
    // change notifications may be lost for good.
    let (state, recorder) = workbench();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let bus = event_bus::EventBus::new(2);
    let receiver = bus.subscribe();
    for id in 0..8 {
        bus.emit(Event::new(event_bus::LifecycleEvent::Started {
            session_id: id.to_string(),
        }));
    }
    let (wake, woken) = std::sync::mpsc::channel();
    let pump = crate::events::EventPump::spawn(
        runtime.handle(),
        receiver,
        Some(Arc::new(move || {
            let _ = wake.send(());
        })),
    );
    let mut state = state.with_pump(pump);
    let ctx = egui::Context::default();
    state.poll_conversation_plan(&ctx);
    recorder.take();

    // When: the frame learns of the lag.
    woken.recv().expect("the lag wakes the frame");
    state.drain_pump(&ctx, false);
    state.poll_conversation_plan(&ctx);

    // Then: the plan is read from its source again, with no notification.
    assert_eq!(recorder.take(), vec![("summary", "first".to_owned())]);
}

#[test]
fn showing_the_panel_again_reads_the_plan_again() {
    // Given: the panel was shown and read in full, then hidden for a frame.
    let (mut state, recorder) = workbench();
    let ctx = egui::Context::default();
    state.poll_conversation_plan(&ctx);
    recorder.take();
    render_panel(&mut state, &ctx);
    state.poll_conversation_plan(&ctx);
    let first: Vec<_> = recorder.take().into_iter().map(|(kind, _)| kind).collect();
    assert_eq!(first, ["summary", "tree", "history"]);
    render_panel(&mut state, &ctx);
    state.poll_conversation_plan(&ctx);
    assert!(
        recorder.take().is_empty(),
        "a panel kept on screen is not read"
    );
    state.poll_conversation_plan(&ctx);

    // When: it is shown again, with no notification in between.
    render_panel(&mut state, &ctx);
    state.poll_conversation_plan(&ctx);

    // Then: every view is read from the source again.
    let again: Vec<_> = recorder.take().into_iter().map(|(kind, _)| kind).collect();
    assert_eq!(again, ["summary", "tree", "history"]);
}
