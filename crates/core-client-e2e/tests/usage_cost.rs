//! The cost of a thread in the desktop client follows the ledger of the production core:
//! a provider request that completes is committed to the usage ledger, the core announces
//! it (`usage_changed`) over its Unix socket, and the client reads the thread's usage
//! again by itself.
//!
//! The model is a stand-in that never answers, so the thread's run stays open; the
//! provider's completion is published on the core's event bus as the routing layer does,
//! and everything after it is the production path: the storage bridge's ledger, the
//! diagnostics area, the projection's event stream, the socket and the client's feature.
//! Every wait ends on a change of the shared client, never on a duration.
use async_trait::async_trait;
use egui_kittest::Harness;
use gui::core_client::{
    Shared,
    connection::{Action, serve},
    enqueue,
    features::{FeatureView, Selection},
};
use std::sync::{Arc, Mutex};
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams};
use workbench_protocol::commands::{Command, ComposerRole, SendChat};
use workbench_protocol::ids::{ProjectId, SubmissionId, ThreadId};
use workbench_protocol::namespaces::ConversationCommand;

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
        "usage-cost".into()
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

/// Draws the features of the shared client for the selected thread, as the dock does.
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
async fn until(window: &mut Window, shown: impl Fn(&Shared) -> bool) {
    let changed = window.state().shared.lock().unwrap().changed.clone();
    loop {
        let notification = changed.notified();
        tokio::pin!(notification);
        notification.as_mut().enable();
        window.run_steps(2);
        if shown(&window.state().shared.lock().unwrap()) {
            return;
        }
        notification.await;
    }
}

fn completed(request_id: &str, run: &str, input_tokens: u64) -> event_bus::Event {
    event_bus::Event::new(event_bus::ProviderEvent::RequestCompleted {
        usage_reported: true,
        request_id: request_id.into(),
        provider: "e2e".into(),
        profile: None,
        protocol: "openai-chat-completions".into(),
        model: "unpriced".into(),
        streaming: true,
        duration_ms: 10,
        input_tokens,
        output_tokens: 5,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        finish_reason: "stop".into(),
        service_tier: None,
        run_id: Some(run.into()),
        reasoning_tokens: None,
        purpose: None,
    })
}

fn requests_of(shared: &Shared, thread: &ThreadId) -> Option<u64> {
    shared
        .features
        .diagnostics()
        .and_then(|diagnostics| diagnostics.thread_usage(thread))
        .map(|usage| usage.total.totals.requests)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_completed_provider_request_updates_the_thread_cost_through_the_core_notice() {
    let dir = test_socket_dir::SplitDir::new();
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir(root.join(".evorch")).unwrap();
    std::fs::write(
        root.join("sidebar.json"),
        serde_json::json!({"version":workspace_state::SIDEBAR_SCHEMA_VERSION,"projects":[{"id":"p","name":"p","repo_root":root,"allowed_directories":[]}],"threads":[]}).to_string(),
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

    let identity = core_instance::ConfigIdentity::new(&root.join("config")).unwrap();
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
    let serving = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        while let Ok((stream, _)) = listener.accept().await {
            connections.spawn(evorch_core::unix::connection(
                stream,
                backend.clone(),
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
    shared
        .lock()
        .unwrap()
        .features
        .diagnostics_mut()
        .expect("the diagnostics feature")
        .set_thread_reask_gap(std::time::Duration::ZERO);
    let desktop = tokio::spawn(serve(
        launcher,
        rx,
        shared.clone(),
        egui::Context::default(),
    ));
    let thread = ThreadId::new("t").unwrap();
    let mut window: Window = Harness::builder()
        .with_size(egui::vec2(1000.0, 1200.0))
        .build_ui_state(
            draw,
            Screen {
                shared: shared.clone(),
                actions: actions.clone(),
                selection: Selection {
                    project: Some(ProjectId::new("p").unwrap()),
                    thread: Some(thread.clone()),
                },
            },
        );
    until(&mut window, |shared| shared.connected).await;

    // A conversation whose run stays open, so the thread and its run exist in the core.
    actions
        .send(Action::Command {
            command: Command::Conversation(ConversationCommand::CreateExecution(SendChat {
                project: ProjectId::new("p").unwrap(),
                thread: thread.clone(),
                submission_id: SubmissionId::new("one").unwrap(),
                composer_role: ComposerRole::Worker,
                text: "hello".into(),
                skills: vec![],
                attachments: vec![],
                model_preference: None,
                fork_seed: None,
            })),
            draft: None,
        })
        .await
        .unwrap();
    let thread_id = thread.clone();
    until(&mut window, move |shared| {
        shared
            .cache
            .runs
            .values()
            .any(|run| run.thread == thread_id && run.parent_run.is_none())
    })
    .await;
    let run = {
        let shared = shared.lock().unwrap();
        shared
            .cache
            .runs
            .values()
            .find(|run| run.thread == thread && run.parent_run.is_none())
            .map(|run| run.id.as_str().to_owned())
            .unwrap()
    };

    // Nothing was requested yet: the thread is read and has no request.
    let asked = thread.clone();
    until(&mut window, move |shared| {
        requests_of(shared, &asked) == Some(0)
    })
    .await;

    // The provider completes a request: the core commits it and announces it, and the
    // client reads the thread again without anyone asking.
    let bus = boot.runtime.event_bus();
    bus.emit(completed("req-1", &run, 1_000));
    let asked = thread.clone();
    until(&mut window, move |shared| {
        requests_of(shared, &asked) == Some(1)
    })
    .await;
    bus.emit(completed("req-2", &run, 2_000));
    let asked = thread.clone();
    until(&mut window, move |shared| {
        requests_of(shared, &asked) == Some(2)
    })
    .await;
    let usage = {
        let shared = shared.lock().unwrap();
        shared
            .features
            .diagnostics()
            .and_then(|diagnostics| diagnostics.thread_usage(&thread))
            .cloned()
            .unwrap()
    };
    assert_eq!(usage.total.totals.input_tokens, 3_000);
    assert_eq!(
        usage.conversation.totals.requests, 2,
        "the thread's root run is the main agent"
    );

    actions.send(Action::Shutdown).await.unwrap();
    desktop.await.unwrap();
    let _ = stop.send(true);
    serving.abort();
}
