use super::*;
use workbench_protocol::{
    handshake::{ActivityCounts, CoreState, CoreStatus},
    ids::{RunId, ThreadId},
    namespaces::{ExecutionEvent, LifecycleEvent, LifecycleQuery, LifecycleReply},
    queries::QueryReply,
    views::{RunPhase, RunRole, RunSummary},
    wire::Revision,
};

/// Records what the registry hands it, and asks for one query per render.
struct Recorder {
    id: FeatureId,
    availability: Availability,
    events: Vec<String>,
    replies: Vec<u64>,
}

impl Recorder {
    fn new(id: FeatureId, availability: Availability) -> Self {
        Self {
            id,
            availability,
            events: vec![],
            replies: vec![],
        }
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
    fn event(&mut self, event: &UiEvent) {
        self.events.push(event.namespace().to_owned());
    }
    fn reply(&mut self, tag: u64, _reply: FeatureReply) {
        self.replies.push(tag);
    }
    fn render(&mut self, _ui: &mut egui::Ui, ctx: &mut FeatureContext<'_>) {
        ctx.query(7, Query::Lifecycle(LifecycleQuery::GetCoreStatus));
    }
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

fn status() -> CoreStatus {
    CoreStatus {
        state: CoreState::Ready,
        retention_reasons: vec![],
        running: ActivityCounts::default(),
    }
}

#[test]
fn the_shipped_placeholders_are_never_offered_or_rendered() {
    let mut features = Features::default();
    assert!(
        features
            .available(true, &Capability::IMPLEMENTED)
            .is_empty()
    );
    run_ui(|ui| {
        let sent = features.render(ui, true, &Capability::IMPLEMENTED);
        assert!(sent.is_empty());
    });
}

#[test]
fn an_implemented_feature_renders_only_while_connected_with_its_capabilities() {
    let mut feature = Recorder::new(FeatureId::Mcp, NEEDS_QUERIES);
    let mut all: [&mut dyn Feature; 1] = [&mut feature];
    assert_eq!(
        available_ids(&mut all, true, &[Capability::Queries]),
        [FeatureId::Mcp]
    );
    assert!(available_ids(&mut all, false, &[Capability::Queries]).is_empty());
    assert!(available_ids(&mut all, true, &[Capability::Commands]).is_empty());

    run_ui(|ui| {
        let sent = render_all(&mut all, ui, true, &[Capability::Queries]);
        // The request leaves typed, tagged for the feature that made it.
        assert_eq!(sent.len(), 1);
        assert_eq!((sent[0].feature, sent[0].tag), (FeatureId::Mcp, 7));
        assert!(matches!(sent[0].request, FeatureRequest::Query(_)));
        assert!(render_all(&mut all, ui, false, &[Capability::Queries]).is_empty());
    });
}

#[test]
fn events_reach_only_the_feature_owning_their_namespace() {
    let mut workspace = Recorder::new(FeatureId::Workspace, NEEDS_QUERIES);
    let mut execution = Recorder::new(FeatureId::Execution, NEEDS_QUERIES);
    let mut all: [&mut dyn Feature; 2] = [&mut workspace, &mut execution];
    let run = RunSummary {
        id: RunId::new("run-1").unwrap(),
        thread: ThreadId::new("t").unwrap(),
        parent_run: None,
        role: RunRole::Worker,
        phase: RunPhase::Running,
        revision: Revision(1),
    };
    route_event(
        &mut all,
        &UiEvent::Execution(ExecutionEvent::RunChanged { run }),
    );
    route_event(
        &mut all,
        &UiEvent::Lifecycle(LifecycleEvent::CoreStatusChanged { status: status() }),
    );
    assert!(workspace.events.is_empty());
    assert_eq!(execution.events, ["execution"]);
}

#[test]
fn a_reply_returns_to_the_feature_that_asked_and_a_refusal_is_a_failed_reply() {
    let mut workspace = Recorder::new(FeatureId::Workspace, NEEDS_QUERIES);
    let mut lanes = Recorder::new(FeatureId::Lanes, NEEDS_QUERIES);
    {
        let mut all: [&mut dyn Feature; 2] = [&mut workspace, &mut lanes];
        route_reply(
            &mut all,
            FeatureId::Lanes,
            41,
            FeatureReply::Query(Ok(QueryReply::Lifecycle(LifecycleReply::CoreStatus(
                status(),
            )))),
        );
    }
    assert!(workspace.replies.is_empty());
    assert_eq!(lanes.replies, [41]);

    let query = FeatureAction {
        feature: FeatureId::Workspace,
        tag: 9,
        request: FeatureRequest::Query(Query::Lifecycle(LifecycleQuery::GetCoreStatus)),
    };
    assert!(matches!(
        query.refused("queue full"),
        FeatureReply::Query(Err(FeatureError {
            outcome_unknown: false,
            ..
        }))
    ));
}
