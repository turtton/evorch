//! The restart targets (`core.restart.targets`) cannot be changed by a settings
//! save from any client: not from the desktop screen, drawn and clicked with
//! egui_kittest over the shared actor and a Unix socket, and not from a Web
//! client over HTTP. Both reach the production core composition
//! (`BootstrappedHost`, real `SettingsArea` and config files); only the model
//! and the per-run sandbox are replaced. Every wait observes an explicit state
//! of the shared client or an event of the core, never a duration.
//!
//! The targets decide which programs the core may start on a planned restart,
//! so a client reachable over the network (a Web client through Tailscale, say)
//! must not be able to point them somewhere else: that would turn a settings
//! save into the start of an arbitrary program.
use async_trait::async_trait;
use egui::{Key, Modifiers, accesskit::Role};
use egui_kittest::{Harness, kittest::Queryable};
use gui::core_client::{
    Shared,
    connection::{Action, serve},
    enqueue,
    features::{FeatureView, Selection},
};
use std::sync::{Arc, Mutex};
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams};
use workbench_protocol::{
    commands::{Command, CommandResult},
    envelope::{ReplyEnvelope, ReplyPayload, ReplyResult, RequestBody, RequestEnvelope},
    errors::ErrorCode,
    events::{Subscribe, SubscriptionScope, UiEvent},
    ids::{OperationId, RequestId, SessionId},
    namespaces::settings::*,
    operation::OperationState,
    queries::{Query, QueryReply},
};

const TARGET: &str = "[core.restart.targets.web]\npath = '/usr/bin/true'\n";
const PROVIDER: &str = "[providers.local]\n\
type = 'openai-compatible'\n\
base_url = 'https://llm.example.invalid/v1'\n\
api_key_env = 'EVORCH_TEST_ABSENT_KEY'\n\
models = ['m']\n\
default_model = 'm'\n";

struct Idle;
#[async_trait]
impl runtime::AgentModel for Idle {
    async fn complete(
        &self,
        _: &runtime::AgentInvocationContext,
        _: runtime::Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        std::future::pending().await
    }
    fn selected_model(&self, _: runtime::Role, _: Option<&str>) -> String {
        "settings-screen".into()
    }
}

struct Unisolated;
impl runtime::SandboxFactory for Unisolated {
    fn build(
        &self,
        _: &runtime::ExecutionPolicy,
        _: &runtime::WorkspaceMounts,
    ) -> Result<Arc<dyn sandbox::Sandbox>, sandbox::SandboxError> {
        Ok(Arc::new(sandbox::DirectSandbox::new_unchecked()))
    }
}

struct Screen {
    shared: Arc<Mutex<Shared>>,
    actions: tokio::sync::mpsc::Sender<Action>,
    selection: Selection,
}

fn draw(ui: &mut egui::Ui, screen: &mut Screen) {
    let frame = {
        let mut shared = screen.shared.lock().unwrap();
        let Shared {
            features,
            cache,
            capabilities,
            connected,
            ..
        } = &mut *shared;
        features.render(
            ui,
            FeatureView {
                connected: *connected,
                capabilities,
                cache,
                selection: &screen.selection,
            },
        )
    };
    let mut shared = screen.shared.lock().unwrap();
    for action in frame.actions {
        enqueue(&screen.actions, &mut shared, Action::Feature(action));
    }
}

type Window = Harness<'static, Screen>;

/// Draw until `shown` holds; each wait ends on a change of the shared client.
async fn until(window: &mut Window, shown: impl Fn(&Window) -> bool) {
    let changed = window.state().shared.lock().unwrap().changed.clone();
    loop {
        let notification = changed.notified();
        tokio::pin!(notification);
        notification.as_mut().enable();
        window.run_steps(2);
        if shown(window) {
            return;
        }
        notification.await;
    }
}

fn shows(window: &Window, text: &str) -> bool {
    window.query_by_label_contains(text).is_some()
}

async fn web(http: &reqwest::Client, root: &str, id: &str, body: RequestBody) -> ReplyEnvelope {
    http.post(format!("{root}/api/v1/request"))
        .header("Origin", root)
        .json(&RequestEnvelope {
            request_id: RequestId::new(id).unwrap(),
            session_id: SessionId::new("web").unwrap(),
            body,
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

fn settings(command: SettingsCommand) -> RequestBody {
    RequestBody::Command(Command::Settings(command))
}

/// The pending operation a command was accepted as.
fn accepted(reply: ReplyEnvelope) -> OperationId {
    match reply.result {
        ReplyResult::Success(ReplyPayload::Command(reply)) => match reply.result.map(|b| *b) {
            Some(CommandResult::Settings(SettingsResult::Operation(view))) => {
                assert_eq!(view.operation.state, OperationState::Pending);
                view.operation.operation_id
            }
            other => panic!("{other:?}"),
        },
        other => panic!("the command was not accepted: {other:?}"),
    }
}

/// The end of operation `id`, as the core announces it.
async fn ended(
    events: &mut workbench_host::subscriptions::Subscription,
    id: &OperationId,
) -> OperationState {
    loop {
        let envelope = events.recv().await.expect("the subscription is open");
        if let UiEvent::Settings(SettingsEvent::SettingsOperationCompleted { operation }) =
            envelope.event
            && &operation.operation.operation_id == id
        {
            return operation.operation.state;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn no_client_can_change_the_restart_targets_by_saving_settings() {
    let dir = test_socket_dir::SplitDir::new();
    let root = dir.path().canonicalize().unwrap();
    let base = format!("version = 3\n{TARGET}{PROVIDER}");
    std::fs::write(root.join("config.toml"), &base).unwrap();
    std::fs::write(
        root.join("sidebar.json"),
        serde_json::json!({"version":workspace_state::SIDEBAR_SCHEMA_VERSION,"projects":[],"threads":[]}).to_string(),
    )
    .unwrap();
    let boot = BootstrappedHost::open_with_test_seams(
        BootstrapOptions {
            config_dir: root.clone(),
            project_root: root.clone(),
            ownership_root: dir.sockets().to_path_buf(),
        },
        TestSeams {
            model: Arc::new(Idle),
            sandbox: Arc::new(Unisolated),
        },
    )
    .await
    .unwrap();
    let mut events = boot
        .host
        .lock()
        .unwrap()
        .projection()
        .subscribe_with_snapshot(&Subscribe {
            scope: SubscriptionScope::All,
            after: None,
        })
        .unwrap();

    // The desktop's shared actor connects over the core's Unix socket.
    let identity = core_instance::ConfigIdentity::new(&root.join("gui-identity")).unwrap();
    let instance = evorch_core::instance::Instance::acquire(
        &core_instance::LaunchGuard::acquire(&identity).unwrap(),
        dir.sockets(),
    )
    .unwrap();
    let listener = instance.bind().unwrap();
    listener.set_nonblocking(true).unwrap();
    let listener = tokio::net::UnixListener::from_std(listener).unwrap();
    let backend = Arc::new(evorch_core::backend::HostBackend::new(
        boot.host.clone(),
        instance.identity.clone(),
    ));
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let socket_backend = backend.clone();
    let serving = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        while let Ok((stream, _)) = listener.accept().await {
            connections.spawn(evorch_core::unix::connection(
                stream,
                socket_backend.clone(),
                stopped.clone(),
            ));
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
    let desktop = tokio::spawn(serve(
        launcher,
        rx,
        shared.clone(),
        egui::Context::default(),
    ));
    let mut window: Window = Harness::builder()
        .with_size(egui::vec2(1000.0, 3000.0))
        .build_ui_state(
            draw,
            Screen {
                shared: shared.clone(),
                actions: actions.clone(),
                selection: Selection::default(),
            },
        );

    // The desktop: the screen shows the file; the person replaces it with a
    // document that has no restart targets at all and saves.
    until(&mut window, |w| shows(w, "Saved settings are applied.")).await;
    window
        .get_all_by_role(Role::MultilineTextInput)
        .next()
        .unwrap()
        .click();
    window.run_steps(4);
    window.key_press_modifiers(Modifiers::COMMAND, Key::A);
    window.run_steps(4);
    window
        .get_all_by_role(Role::MultilineTextInput)
        .next()
        .unwrap()
        .type_text("version = 3");
    window.run_steps(4);
    window.get_by_label("Save file").click();
    window.run_steps(4);
    until(&mut window, |w| {
        shows(w, "cannot be changed from this screen")
    })
    .await;
    assert_eq!(
        std::fs::read_to_string(root.join("config.toml")).unwrap(),
        base,
        "the desktop's save did not reach the file"
    );

    // A Web client over HTTP: whole-document saves that remove, rewrite, rename
    // and add a target, each quoting the current revision.
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
    let url = format!("http://{}", web_server.local_addr());
    let http = reqwest::Client::new();
    let snapshot = {
        let reply = web(
            &http,
            &url,
            "snapshot",
            RequestBody::Query(Query::Settings(SettingsQuery::GetSettingsSnapshot {})),
        )
        .await;
        match reply.result {
            ReplyResult::Success(ReplyPayload::Query(QueryReply::Settings(
                SettingsReply::Settings(snapshot),
            ))) => *snapshot,
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(snapshot.document, base);
    let cases = [
        ("omitted", format!("version = 3\n{PROVIDER}")),
        (
            "removed",
            format!("version = 3\n[core]\nidle_grace_secs = 9\n{PROVIDER}"),
        ),
        (
            "path-changed",
            format!("version = 3\n[core.restart.targets.web]\npath = '/bin/sh'\n{PROVIDER}"),
        ),
        (
            "renamed",
            format!(
                "version = 3\n[core.restart.targets.other]\npath = '/usr/bin/true'\n{PROVIDER}"
            ),
        ),
        (
            "added",
            format!(
                "version = 3\n{TARGET}[core.restart.targets.extra]\npath = '/bin/ls'\n{PROVIDER}"
            ),
        ),
    ];
    for (name, text) in cases {
        let id = OperationId::new(format!("web-{name}")).unwrap();
        let reply = web(
            &http,
            &url,
            &format!("req-{name}"),
            settings(SettingsCommand::SaveDocument(SaveDocument {
                operation_id: id.clone(),
                expected_revision: snapshot.revision.clone(),
                text,
            })),
        )
        .await;
        assert_eq!(accepted(reply), id);
        assert_eq!(
            ended(&mut events, &id).await,
            OperationState::Failed {
                code: ErrorCode::PermissionDenied
            },
            "{name}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("config.toml")).unwrap(),
            base,
            "{name} reached the file"
        );
    }

    // A partial save from the Web changes what it names and leaves the targets
    // as they are.
    let id = OperationId::new("web-provider").unwrap();
    let reply = web(
        &http,
        &url,
        "req-provider",
        settings(SettingsCommand::SaveProvider(Box::new(SaveProvider {
            operation_id: id.clone(),
            expected_revision: snapshot.revision.clone(),
            original_name: None,
            provider: ProviderInput::OpenaiCompatible {
                name: "added".into(),
                kind: OpenaiCompatibleKind::OpenaiCompatible,
                base_url: "https://llm.example.invalid/v1".into(),
                credential: CredentialRef::Env {
                    var: "EVORCH_TEST_ABSENT_KEY".into(),
                },
                models: vec![ModelInput {
                    id: "m".into(),
                    enabled: true,
                    kind: None,
                    context_override: None,
                }],
                excluded_models: vec![],
                default_model: "m".into(),
                show_quota: None,
            },
        }))),
    )
    .await;
    assert_eq!(accepted(reply), id);
    assert_eq!(ended(&mut events, &id).await, OperationState::Ready);
    let file = std::fs::read_to_string(root.join("config.toml")).unwrap();
    assert!(file.contains("[providers.added]"), "{file}");
    assert!(file.contains("[core.restart.targets.web]"), "{file}");
    assert!(file.contains("path = '/usr/bin/true'"), "{file}");

    actions.send(Action::Shutdown).await.unwrap();
    desktop.await.unwrap();
    let _ = stop.send(true);
    serving.abort();
}
