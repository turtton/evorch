use super::*;
use std::sync::{Arc, Mutex};
use workbench_protocol::{
    handshake::{ActivityCounts, CoreState, CoreStatus},
    ids::RunId,
    namespaces::{ExecutionEvent, LifecycleEvent, LifecycleQuery, LifecycleReply},
    queries::QueryReply,
    views::{RunPhase, RunRole, RunSummary},
    wire::Revision,
};

type Log = Arc<Mutex<Vec<String>>>;

/// Records what the registry hands it, asks for one query per render and may
/// draw the selector.
pub(crate) struct Recorder {
    id: FeatureId,
    availability: Availability,
    selector: bool,
    selector_query: bool,
    note: Option<&'static str>,
    log: Log,
}

impl Recorder {
    pub(crate) fn new(id: FeatureId, availability: Availability, log: &Log) -> Box<Self> {
        Box::new(Self {
            id,
            availability,
            selector: false,
            selector_query: false,
            note: None,
            log: log.clone(),
        })
    }

    fn with_selector(mut self: Box<Self>) -> Box<Self> {
        self.selector = true;
        self
    }

    /// Asks for a query while being asked for the selector.
    fn with_selector_query(mut self: Box<Self>) -> Box<Self> {
        self.selector_query = true;
        self
    }

    fn with_note(mut self: Box<Self>, note: &'static str) -> Box<Self> {
        self.note = Some(note);
        self
    }

    fn note(&self, line: String) {
        self.log.lock().unwrap().push(line);
    }
}

impl Feature for Recorder {
    fn id(&self) -> FeatureId {
        self.id
    }
    fn title(&self) -> &'static str {
        "recorder"
    }
    fn availability(&self) -> Availability {
        self.availability
    }
    fn reset(&mut self) {
        self.note("reset".into());
    }
    fn event(&mut self, event: &UiEvent) {
        self.note(format!("event {}", event.namespace()));
    }
    fn reply(&mut self, tag: u64, reply: FeatureReply) {
        let outcome = match reply {
            FeatureReply::Command(Ok(_)) | FeatureReply::Query(Ok(_)) => "ok",
            FeatureReply::Command(Err(_)) | FeatureReply::Query(Err(_)) => "err",
        };
        self.note(format!("reply {tag} {outcome}"));
    }
    fn render(&mut self, _ui: &mut egui::Ui, ctx: &mut FeatureContext<'_>) {
        ctx.query(7, Query::Lifecycle(LifecycleQuery::GetCoreStatus));
    }
    fn unsupported(&self) -> Option<&'static str> {
        self.note
    }
    fn render_selector(&mut self, _ui: &mut egui::Ui, ctx: &mut FeatureContext<'_>) -> bool {
        if self.selector_query {
            ctx.query(11, Query::Lifecycle(LifecycleQuery::GetCoreStatus));
        }
        if self.selector {
            let project = ctx.selection().project.clone().unwrap();
            ctx.navigate(Navigation::NewConversation(project));
        }
        self.selector
    }
}

pub(crate) fn log() -> Log {
    Log::default()
}

pub(crate) fn lines(log: &Log) -> Vec<String> {
    log.lock().unwrap().clone()
}

fn run_ui(f: impl FnOnce(&mut egui::Ui)) {
    let mut f = Some(f);
    let mut output = egui::Context::default().run_ui(egui::RawInput::default(), |ui| {
        if let Some(f) = f.take() {
            f(ui);
        }
    });
    output.textures_delta.clear();
}

const NEEDS_QUERIES: Availability = Availability::Implemented {
    required: &[Capability::Queries],
};

pub(crate) fn status() -> CoreStatus {
    CoreStatus {
        state: CoreState::Ready,
        retention_reasons: vec![],
        running: ActivityCounts::default(),
    }
}

fn view<'a>(
    connected: bool,
    capabilities: &'a [Capability],
    cache: &'a Cache,
    selection: &'a Selection,
) -> FeatureView<'a> {
    FeatureView {
        connected,
        capabilities,
        cache,
        selection,
    }
}

#[test]
fn only_the_implemented_workspace_is_offered_and_the_other_placeholders_stay_silent() {
    let mut features = Features::default();
    let (cache, selection) = (Cache::default(), Selection::default());
    // Only implemented areas are offered: the workspace and MCP (in slot
    // order). Without a selected project MCP asks for nothing.
    assert_eq!(
        features.available(true, &Capability::IMPLEMENTED),
        [FeatureId::Workspace, FeatureId::Mcp]
    );
    assert!(
        features
            .available(false, &Capability::IMPLEMENTED)
            .is_empty()
    );
    run_ui(|ui| {
        let v = view(true, &Capability::IMPLEMENTED, &cache, &selection);
        let frame = features.render(ui, v);
        assert!(
            frame
                .actions
                .iter()
                .all(|a| a.feature == FeatureId::Workspace),
            "a placeholder never sends anything"
        );
        assert!(frame.navigation.is_empty());
        let outcome = features.render_selector(ui, v);
        assert!(outcome.replaced, "the workspace feature draws the selector");
        assert!(outcome.frame.navigation.is_empty());
    });
}

#[test]
fn an_implemented_feature_renders_only_while_connected_with_its_capabilities() {
    let log = log();
    let mut features = Features::default();
    features.install(Recorder::new(FeatureId::Mcp, NEEDS_QUERIES, &log));
    assert_eq!(
        features.available(true, &[Capability::Queries]),
        [FeatureId::Mcp]
    );
    assert!(features.available(false, &[Capability::Queries]).is_empty());
    assert!(features.available(true, &[Capability::Commands]).is_empty());

    let (cache, selection) = (Cache::default(), Selection::default());
    run_ui(|ui| {
        let frame = features.render(ui, view(true, &[Capability::Queries], &cache, &selection));
        // The request leaves typed, tagged for the feature that made it.
        assert_eq!(frame.actions.len(), 1);
        assert_eq!(
            (frame.actions[0].feature, frame.actions[0].tag),
            (FeatureId::Mcp, 7)
        );
        assert!(matches!(frame.actions[0].request, FeatureRequest::Query(_)));
        let none = features.render(ui, view(false, &[Capability::Queries], &cache, &selection));
        assert!(none.actions.is_empty());
    });
}

#[test]
fn only_an_available_feature_may_replace_the_selector_and_its_navigation_is_typed() {
    let log = log();
    let project = ProjectId::new("p").unwrap();
    let selection = Selection {
        project: Some(project.clone()),
        thread: None,
    };
    let cache = Cache::default();
    let mut features = Features::default();
    features.install(Recorder::new(FeatureId::Workspace, NEEDS_QUERIES, &log).with_selector());
    run_ui(|ui| {
        // Not connected: the feature is not available, the built-in selector stays.
        let unavailable =
            features.render_selector(ui, view(false, &[Capability::Queries], &cache, &selection));
        assert!(!unavailable.replaced);
        let outcome =
            features.render_selector(ui, view(true, &[Capability::Queries], &cache, &selection));
        assert!(outcome.replaced);
        assert_eq!(
            outcome.frame.navigation,
            [Navigation::NewConversation(project)]
        );
    });
}

#[test]
fn events_reach_only_the_feature_owning_their_namespace() {
    let (workspace_log, execution_log) = (log(), log());
    let mut features = Features::default();
    features.install(Recorder::new(
        FeatureId::Workspace,
        NEEDS_QUERIES,
        &workspace_log,
    ));
    features.install(Recorder::new(
        FeatureId::Execution,
        NEEDS_QUERIES,
        &execution_log,
    ));
    let run = RunSummary {
        id: RunId::new("run-1").unwrap(),
        thread: ThreadId::new("t").unwrap(),
        parent_run: None,
        role: RunRole::Worker,
        phase: RunPhase::Running,
        revision: Revision(1),
    };
    features.event(&UiEvent::Execution(ExecutionEvent::RunChanged { run }));
    features.event(&UiEvent::Lifecycle(LifecycleEvent::CoreStatusChanged {
        status: status(),
    }));
    assert!(lines(&workspace_log).is_empty());
    assert_eq!(lines(&execution_log), ["event execution"]);
}

#[test]
fn a_reply_returns_to_the_feature_that_asked_and_a_refusal_is_a_failed_reply() {
    let (workspace_log, lanes_log) = (log(), log());
    let mut features = Features::default();
    features.install(Recorder::new(
        FeatureId::Workspace,
        NEEDS_QUERIES,
        &workspace_log,
    ));
    features.install(Recorder::new(FeatureId::Lanes, NEEDS_QUERIES, &lanes_log));
    features.reply(
        FeatureId::Lanes,
        41,
        FeatureReply::Query(Ok(QueryReply::Lifecycle(LifecycleReply::CoreStatus(
            status(),
        )))),
    );
    assert!(lines(&workspace_log).is_empty());
    assert_eq!(lines(&lanes_log), ["reply 41 ok"]);

    let refused = FeatureAction {
        feature: FeatureId::Workspace,
        tag: 9,
        request: FeatureRequest::Query(Query::Lifecycle(LifecycleQuery::GetCoreStatus)),
    };
    features.refuse(&refused, "queue full");
    assert_eq!(lines(&workspace_log), ["reply 9 err"]);
}

#[test]
fn a_session_resets_first_then_delivers_events_and_the_held_replies() {
    let log = log();
    let mut features = Features::default();
    features.install(Recorder::new(FeatureId::Lanes, NEEDS_QUERIES, &log));
    let held = vec![HeldReply {
        feature: FeatureId::Lanes,
        tag: 3,
        reply: FeatureReply::Command(Err(FeatureError::new("rejected"))),
    }];
    features.begin_session(&[], held);
    // A reset never discards a held reply: it comes after it.
    assert_eq!(lines(&log), ["reset", "reply 3 err"]);
}

#[test]
fn what_a_feature_asks_for_while_declining_the_selector_is_not_lost() {
    let log = log();
    let mut features = Features::default();
    // Available, declines to draw the selector, but asked for a query.
    features
        .install(Recorder::new(FeatureId::Workspace, NEEDS_QUERIES, &log).with_selector_query());
    let (cache, selection) = (Cache::default(), Selection::default());
    run_ui(|ui| {
        let outcome =
            features.render_selector(ui, view(true, &[Capability::Queries], &cache, &selection));
        // The built-in selector stays, and the request is still delivered.
        assert!(!outcome.replaced);
        assert_eq!(outcome.frame.actions.len(), 1);
        assert_eq!(
            (
                outcome.frame.actions[0].feature,
                outcome.frame.actions[0].tag
            ),
            (FeatureId::Workspace, 11)
        );
    });
}

#[test]
fn only_placeholders_that_say_so_are_listed_as_unsupported() {
    let log = log();
    let mut features = Features::default();
    // The shipped placeholders say nothing: the view names no feature itself.
    assert!(features.unsupported_notes().is_empty());
    features.install(
        Recorder::new(FeatureId::Mcp, Availability::Placeholder, &log).with_note("not yet"),
    );
    features.install(
        Recorder::new(FeatureId::Teams, NEEDS_QUERIES, &log).with_note("implemented: not listed"),
    );
    assert_eq!(features.unsupported_notes(), [("recorder", "not yet")]);
}
