//! A real desktop (shared connection and features, drawn into egui) and a
//! Web client against one core with a live lead and a managed lane: both show
//! the same runs and lanes, and a lane operation made from the screen reaches
//! the ledger and is shown again.
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use gui::core_client::{
    Shared,
    connection::{Action, serve},
    features::{FeatureView, Selection},
};
use workbench_protocol::namespaces::{
    ExecutionQuery, ExecutionReply, LanesQuery, LanesReply,
    execution::{RunDetailsQuery, RunScope},
    lanes::ListLanes,
};
use workbench_protocol::{envelope::*, ids::*, queries::*};

struct HeldModel(tokio::sync::Notify);

#[async_trait]
impl runtime::AgentModel for HeldModel {
    async fn complete(
        &self,
        _: &runtime::AgentInvocationContext,
        _: runtime::Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        self.0.notify_one();
        std::future::pending().await
    }
    fn selected_model(&self, _: runtime::Role, _: Option<&str>) -> String {
        "held-model".into()
    }
}

fn texts(output: &egui::FullOutput) -> String {
    fn walk(shape: &egui::Shape, out: &mut String) {
        match shape {
            egui::Shape::Text(text) => {
                out.push_str(text.galley.text());
                out.push('\n');
            }
            egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| walk(s, out)),
            _ => {}
        }
    }
    let mut out = String::new();
    for clipped in &output.shapes {
        walk(&clipped.shape, &mut out);
    }
    out
}

/// Draw the features once for `thread`, send what they asked for, return the text drawn.
async fn frame(
    shared: &Arc<Mutex<Shared>>,
    actions: &tokio::sync::mpsc::Sender<Action>,
    ctx: &egui::Context,
    thread: &str,
) -> String {
    let (asked, text) = {
        let mut state = shared.lock().unwrap();
        let selection = Selection {
            project: Some(ProjectId::new("project-1").unwrap()),
            thread: Some(ThreadId::new(thread).unwrap()),
        };
        let mut asked = None;
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            let state = &mut *state;
            let view = FeatureView {
                connected: state.connected,
                capabilities: &state.capabilities,
                cache: &state.cache,
                selection: &selection,
            };
            asked = Some(state.features.render(ui, view));
        });
        output.textures_delta.clear();
        (asked.unwrap(), texts(&output))
    };
    for action in asked.actions {
        actions.send(Action::Feature(action)).await.unwrap();
    }
    text
}

/// Draw until the screen shows everything in `wanted`; every wait is on a
/// change the connection reported.
async fn screen_shows(
    shared: &Arc<Mutex<Shared>>,
    actions: &tokio::sync::mpsc::Sender<Action>,
    ctx: &egui::Context,
    thread: &str,
    wanted: &[String],
) -> String {
    let changed = shared.lock().unwrap().changed.clone();
    loop {
        let notification = changed.notified();
        tokio::pin!(notification);
        notification.as_mut().enable();
        let text = frame(shared, actions, ctx, thread).await;
        if wanted.iter().all(|w| text.contains(w.as_str())) {
            return text;
        }
        notification.await;
    }
}

async fn web(http: &reqwest::Client, root: &str, id: &str, query: Query) -> QueryReply {
    let reply: ReplyEnvelope = http
        .post(format!("{root}/api/v1/request"))
        .header("Origin", root)
        .json(&RequestEnvelope {
            request_id: RequestId::new(id).unwrap(),
            session_id: SessionId::new("web").unwrap(),
            body: RequestBody::Query(query),
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    match reply.result {
        ReplyResult::Success(ReplyPayload::Query(reply)) => reply,
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_desktop_and_the_web_client_show_the_same_runs_and_lanes() {
    let dir = tempfile::Builder::new()
        .prefix("p12-")
        .tempdir_in("/tmp")
        .unwrap();
    let bus = Arc::new(event_bus::EventBus::new(1024));
    let model = Arc::new(HeldModel(tokio::sync::Notify::new()));
    let config = storage::StorageConfig {
        db_path: dir.path().join("events.db"),
        ..Default::default()
    };
    let store = storage::Storage::open(config.clone()).unwrap();
    let runtime = runtime::AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus.clone())),
        model.clone(),
    )
    .with_run_store(runtime::RunStore::open(&config, store.handle()).unwrap());
    let owner = Arc::new(
        runtime::ownership::OwnerHost::open(
            &dir.path().join("owners"),
            Default::default(),
            bus.clone(),
        )
        .unwrap(),
    );
    let lead = runtime
        .delegate_chat(
            "lead",
            runtime::Role::Orchestrator,
            "manage lanes".into(),
            runtime::RunConfig {
                conversation: true,
                interactive: true,
                keep_alive: true,
                ownership: Some(owner.start("lead").unwrap()),
                project_root: Some(dir.path().into()),
                ..Default::default()
            },
        )
        .unwrap();
    model.0.notified().await;
    let brief: event_bus::LaneBrief = serde_json::from_value(serde_json::json!({
        "objective": "e2e objective", "inputs": [], "deliverables": ["patch"],
        "acceptance_criteria": ["review"], "constraints": [], "workspace": {},
        "report_policy": "attached"
    }))
    .unwrap();
    let lane = runtime.open_lane(lead, "e2e-open", brief).unwrap();
    model.0.notified().await;
    store
        .handle()
        .upsert_threads_host(&["lead", lane.thread_id.as_str()].map(|id| {
            storage::threads::ThreadUpsert {
                thread_id: id.into(),
                project_id: Some("project-1".into()),
                project_root: Some(dir.path().to_string_lossy().into_owned()),
                ..Default::default()
            }
        }))
        .unwrap();
    std::fs::write(
        dir.path().join("sidebar.json"),
        r#"{"version":2,"families":[]}"#,
    )
    .unwrap();

    let service = workbench_host::commands::CommandService::new(
        runtime.clone(),
        tokio::runtime::Handle::current(),
    )
    .with_event_bus(bus.clone())
    .with_ownership(owner.clone());
    service.bind_lane_delivery();
    let areas = workbench_host::areas::Areas {
        execution: workbench_host::areas::execution::ExecutionArea::with_runtime(runtime.clone()),
        teams: workbench_host::areas::teams::TeamsArea::with_runtime(runtime.clone()),
        lanes: workbench_host::areas::lanes::LanesArea::with_runtime(runtime.clone()),
        ..Default::default()
    };
    let host = Arc::new(Mutex::new(
        workbench_host::CommandHost::new(service)
            .with_lane_sidebar_path(dir.path().join("sidebar.json"))
            .with_areas(areas),
    ));
    let identity = core_instance::ConfigIdentity::new(&dir.path().join("config")).unwrap();
    let instance = evorch_core::instance::Instance::acquire(
        &core_instance::LaunchGuard::acquire(&identity).unwrap(),
        dir.path(),
    )
    .unwrap();
    let listener = instance.bind().unwrap();
    listener.set_nonblocking(true).unwrap();
    let listener = tokio::net::UnixListener::from_std(listener).unwrap();
    let backend = Arc::new(evorch_core::backend::HostBackend::new(
        host,
        instance.identity.clone(),
    ));
    let (_stop, stopped) = tokio::sync::watch::channel(false);
    let socket_backend = backend.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let backend = socket_backend.clone();
            let stopped = stopped.clone();
            tokio::spawn(async move {
                let _ = evorch_core::unix::connection(stream, backend, stopped).await;
            });
        }
    });
    let launcher = core_launcher::Launcher::with_socket(
        identity,
        core_launcher::CoreBinary::explicit(&std::env::current_exe().unwrap()).unwrap(),
        gui::core_client::client_config(),
        instance.socket.clone(),
    )
    .unwrap();
    let (actions, rx) = tokio::sync::mpsc::channel(64);
    let shared = Arc::new(Mutex::new(Shared::default()));
    tokio::spawn(serve(
        launcher,
        rx,
        shared.clone(),
        egui::Context::default(),
    ));
    let changed = shared.lock().unwrap().changed.clone();
    loop {
        let notification = changed.notified();
        tokio::pin!(notification);
        notification.as_mut().enable();
        if shared.lock().unwrap().connected {
            break;
        }
        notification.await;
    }

    let web_server = workbench_web::WebServer::start(
        workbench_web::WebServerConfig {
            enabled: true,
            bind: ([127, 0, 0, 1], 0).into(),
            ..Default::default()
        },
        backend.clone(),
    )
    .await
    .unwrap()
    .unwrap();
    let root = format!("http://{}", web_server.local_addr());
    let http = reqwest::Client::new();

    // What the Web client reads for the lead's thread.
    let QueryReply::Execution(ExecutionReply::RunDetails(web_runs)) = web(
        &http,
        &root,
        "web-runs",
        Query::Execution(ExecutionQuery::ListRunDetails(RunDetailsQuery {
            thread: ThreadId::new("lead").unwrap(),
            scope: RunScope::Roots,
            page: Default::default(),
        })),
    )
    .await
    else {
        panic!("run details expected");
    };
    let QueryReply::Lanes(LanesReply::Lanes(web_lanes)) = web(
        &http,
        &root,
        "web-lanes",
        Query::Lanes(LanesQuery::ListLanes(ListLanes {
            lead_thread: ThreadId::new("lead").unwrap(),
            policy: None,
            page: Default::default(),
        })),
    )
    .await
    else {
        panic!("lanes expected");
    };
    assert_eq!(web_runs.items.len(), 1);
    assert_eq!(web_lanes.items.len(), 1);
    assert_eq!(web_lanes.items[0].lane_id.as_str(), lane.lane_id);

    // The desktop shows exactly those runs and lanes.
    let wanted: Vec<String> = web_runs
        .items
        .iter()
        .map(|run| run.name.clone())
        .chain(
            web_lanes
                .items
                .iter()
                .map(|lane| lane.objective_preview.clone()),
        )
        .collect();
    let ctx = egui::Context::default();
    let text = screen_shows(&shared, &actions, &ctx, "lead", &wanted).await;
    assert!(text.contains("実行中"), "the lead's run is live and active");
    assert!(text.contains("添付"), "the lane is attached");

    // A lane operation from the desktop reaches the ledger; both clients then
    // agree that the lane left the lead's list.
    let wanted_after = ["この会話が管理している lane はありません。".to_owned()];
    let changed = shared.lock().unwrap().changed.clone();
    let mut clicked = false;
    loop {
        let notification = changed.notified();
        tokio::pin!(notification);
        notification.as_mut().enable();
        if !clicked {
            // The same typed command the screen's "切り離す" button sends.
            use workbench_protocol::lane_management::*;
            let command = workbench_protocol::commands::Command::Lanes(
                workbench_protocol::namespaces::LanesCommand::ManageLane(ManageLane {
                    request: LaneManagementRequest {
                        operation_id: LaneManagementOperationId::new("e2e-detach").unwrap(),
                        lane_id: workbench_protocol::lane_attention::LaneId::new(&lane.lane_id)
                            .unwrap(),
                        expected_management_revision: web_lanes.items[0].management_revision,
                        action: LaneManagementAction::Detach,
                    },
                    expected_family_revision: None,
                }),
            );
            actions
                .send(Action::Command {
                    command,
                    draft: None,
                })
                .await
                .unwrap();
            // The area here has no event task, so the screen is refreshed the way
            // its "再接続 / 再同期" button does.
            actions.send(Action::Reconnect).await.unwrap();
            clicked = true;
        }
        let text = frame(&shared, &actions, &ctx, "lead").await;
        if wanted_after.iter().all(|w| text.contains(w.as_str())) {
            break;
        }
        notification.await;
    }
    let QueryReply::Lanes(LanesReply::Lanes(after)) = web(
        &http,
        &root,
        "web-lanes-after",
        Query::Lanes(LanesQuery::ListLanes(ListLanes {
            lead_thread: ThreadId::new("lead").unwrap(),
            policy: None,
            page: Default::default(),
        })),
    )
    .await
    else {
        panic!("lanes expected");
    };
    assert!(
        after.items.is_empty(),
        "the Web client sees the same ledger"
    );
    for run in [lane.root_run_id, lead] {
        runtime.cancel(run).unwrap();
        runtime.wait(run).await.unwrap();
    }
}
