//! Real host, wire subscription, shared desktop actor and egui footer. The
//! fetch seam is the only substitute: no quota endpoint or credential is used.
use async_trait::async_trait;
use egui_kittest::{Harness, kittest::Queryable};
use gui::core_client::{
    Shared,
    connection::{Action, serve},
    enqueue,
    features::{
        FeatureAction, FeatureId, FeatureRequest, FeatureView, Selection,
        diagnostics::quota::footer,
    },
};
use std::sync::{Arc, Mutex};
use workbench_host::{
    areas::diagnostics::quota::test_support::with_fetch,
    bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams},
};
use workbench_protocol::{
    commands::Command,
    ids::OperationId,
    namespaces::{
        diagnostics::*,
        settings::{RefreshSettings, SettingsCommand},
    },
};

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
        unreachable!("quota never calls a model")
    }
    fn selected_model(&self, _: runtime::Role, _: Option<&str>) -> String {
        "unused".into()
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
}
fn draw(ui: &mut egui::Ui, screen: &mut Screen) {
    let mut shared = screen.shared.lock().unwrap();
    let Shared {
        features,
        cache,
        capabilities,
        connected,
        ..
    } = &mut *shared;
    let frame = features.render(
        ui,
        FeatureView {
            connected: *connected,
            capabilities,
            cache,
            selection: &Selection::default(),
        },
    );
    let quotas = features.diagnostics().unwrap().quotas.profiles();
    footer(ui, quotas);
    for action in frame.actions {
        enqueue(&screen.actions, &mut shared, Action::Feature(action));
    }
}
type Window = Harness<'static, Screen>;
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
fn profiles(window: &Window) -> Vec<ProviderQuota> {
    window
        .state()
        .shared
        .lock()
        .unwrap()
        .features
        .diagnostics()
        .unwrap()
        .quotas
        .profiles()
        .to_vec()
}
const CONFIG: &str = r#"version = 3
[providers.work]
provider_type = "openai-codex"
base_url = "https://unused.invalid"
credential = { type = "keyring", service = "evorch", account = "never-read" }
"#;
#[tokio::test(flavor = "multi_thread")]
async fn quota_updates_and_profile_removal_reach_footer_without_manual_reload() {
    let dir = test_socket_dir::SplitDir::new();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(root.join("config.toml"), CONFIG).unwrap();
    let (calls, mut received) = tokio::sync::mpsc::unbounded_channel();
    let mut boot = with_fetch(
        Arc::new(move |profile, _| {
            let calls = calls.clone();
            Box::pin(async move {
                let (answer, result) = tokio::sync::oneshot::channel();
                calls.send((profile, answer)).unwrap();
                result.await.unwrap()
            })
        }),
        BootstrappedHost::open_with_test_seams(
            BootstrapOptions {
                config_dir: root.clone(),
                project_root: root.clone(),
                ownership_root: dir.sockets().to_path_buf(),
            },
            TestSeams {
                model: Arc::new(Idle),
                sandbox: Arc::new(Unisolated),
            },
        ),
    )
    .await
    .unwrap();
    // Host construction, including subscription profiles, does not fetch.
    assert!(received.try_recv().is_err());
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
    let desktop = tokio::spawn(serve(
        launcher,
        rx,
        shared.clone(),
        egui::Context::default(),
    ));
    let mut window: Window = Harness::builder()
        .with_size(egui::vec2(1000.0, 700.0))
        .build_ui_state(
            draw,
            Screen {
                shared: shared.clone(),
                actions: actions.clone(),
            },
        );
    until(&mut window, |w| !profiles(w).is_empty()).await;
    window.get_by_label("Quota work");
    let (profile, answer) = received.recv().await.unwrap();
    assert_eq!(profile, "work");
    let second = workbench_client::WorkbenchClient::connect(
        workbench_client::UnixTransport::connect(&instance.socket)
            .await
            .unwrap(),
        workbench_client::ClientConfig::new(
            workbench_protocol::ids::ClientId::new("quota-second").unwrap(),
            workbench_protocol::handshake::BuildInfo {
                name: "quota-test".into(),
                version: "1".into(),
                revision: None,
            },
        ),
    )
    .await
    .unwrap();
    second
        .query(workbench_protocol::queries::Query::Diagnostics(
            DiagnosticsQuery::GetProviderQuotas(GetProviderQuotas {}),
        ))
        .await
        .unwrap();
    assert!(
        received.try_recv().is_err(),
        "second client must not start another fetch"
    );
    drop(second);
    answer
        .send(Ok(vec![QuotaWindow {
            id: "primary".into(),
            label: "5h".into(),
            remaining_basis_points: Some(7500),
            resets_at_ms: None,
            duration_seconds: Some(18000),
            usage_label: None,
        }]))
        .unwrap();
    until(&mut window, |w| {
        profiles(w)
            .first()
            .is_some_and(|q| q.state == QuotaState::Ready)
    })
    .await;
    assert_eq!(
        profiles(&window)[0].windows[0].remaining_basis_points,
        Some(7500)
    );
    assert!(received.try_recv().is_err());
    // #852 is not in the base: disappearance uses profile removal, the same
    // eligible-profile reconciliation path as off once show_quota lands.
    std::fs::write(root.join("config.toml"), "version = 3\n").unwrap();
    actions
        .send(Action::Feature(FeatureAction {
            feature: FeatureId::Settings,
            tag: 999,
            request: FeatureRequest::Command(Command::Settings(SettingsCommand::RefreshSettings(
                RefreshSettings {
                    operation_id: OperationId::new("quota-off").unwrap(),
                },
            ))),
        }))
        .await
        .unwrap();
    until(&mut window, |w| profiles(w).is_empty()).await;
    window.run_steps(2); // Draw the snapshot just observed in shared state.
    assert!(window.query_by_label("Quota work").is_none());
    actions.send(Action::Shutdown).await.unwrap();
    desktop.await.unwrap();
    let _ = stop.send(true);
    serving.abort();
    let _ = serving.await;
    boot.shutdown().await.unwrap();
}
