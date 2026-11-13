//! The MCP screen of the desktop client, from a click to the keychain and a
//! real MCP server process: add -> approve -> test -> revoke -> delete.
//!
//! The screen is the real `McpFeature` drawn by egui and driven through its
//! accessibility tree. It talks over the shared actor (`serve`) and a Unix
//! socket to the production core composition (real `McpArea`, approval ledger,
//! process limiter and a file keychain). The MCP server is a mock process that
//! reports over a control socket. Every wait observes an explicit state of the
//! shared client or a screen element, never a duration.
use async_trait::async_trait;
use egui::accesskit::Role;
use egui_kittest::{Harness, kittest::Queryable};
use gui::core_client::{
    Shared,
    connection::{Action, serve},
    enqueue,
    features::{FeatureView, Selection},
};
use sandbox::{CredentialStore, FileCredentialStore, Secret};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams};
use workbench_protocol::ids::ProjectId;

const SECRET: &str = "screen-e2e-typed-secret-value";

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
        "mcp-screen".into()
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

/// What the screen test draws: the features of the shared client, for the
/// selected project, and the requests they make go to the shared actor.
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

fn click(window: &mut Window, label: &str) {
    window.get_by_label(label).click();
    window.run_steps(4);
}

fn type_into(window: &mut Window, role: Role, nth: usize, text: &str) {
    window.get_all_by_role(role).nth(nth).unwrap().click();
    window.run_steps(4);
    window
        .get_all_by_role(role)
        .nth(nth)
        .unwrap()
        .type_text(text);
    window.run_steps(4);
}

/// Draw until `shown` holds; each wait ends on a change of the shared client.
async fn until(window: &mut Window, _waiting_for: &str, shown: impl Fn(&Window) -> bool) {
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

#[tokio::test(flavor = "multi_thread")]
async fn the_screen_adds_approves_tests_revokes_and_deletes_a_server_through_the_core() {
    let dir = tempfile::Builder::new()
        .prefix("e2e-mcp-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir(root.join(".evorch")).unwrap();
    std::fs::write(
        root.join("sidebar.json"),
        serde_json::json!({"version":workspace_state::SIDEBAR_SCHEMA_VERSION,"projects":[{"id":"p","name":"p","repo_root":root,"allowed_directories":[]}],"threads":[]}).to_string(),
    )
    .unwrap();
    // The mock MCP server connects here when it starts.
    let control = tokio::net::UnixListener::bind(root.join("control")).unwrap();
    let started = Arc::new(AtomicUsize::new(0));
    let counter = started.clone();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = control.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);
            held.push(stream);
        }
    });
    let keychain = Arc::new(FileCredentialStore::open(root.join("secrets")).unwrap());
    let mcp = runtime::mcp::McpHostServices::at_config_dir(
        runtime::mcp::McpCredentialBackend::Available(keychain.clone()),
        root.clone(),
    );
    let boot = BootstrappedHost::open_with_mcp_test_seams(
        BootstrapOptions {
            config_dir: root.clone(),
            project_root: root.clone(),
            ownership_root: root.join("owners"),
        },
        TestSeams {
            model: Arc::new(Idle),
            sandbox: Arc::new(Unisolated),
        },
        mcp,
    )
    .await
    .unwrap();

    // The production core listens on a Unix socket; the desktop's shared actor
    // connects to it as it does to a core it launched.
    let identity = core_instance::ConfigIdentity::new(&root.join("config")).unwrap();
    let instance = evorch_core::instance::Instance::acquire(
        &core_instance::LaunchGuard::acquire(&identity).unwrap(),
        &root,
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
        .with_size(egui::vec2(1000.0, 3000.0))
        .build_ui_state(
            draw,
            Screen {
                shared: shared.clone(),
                actions: actions.clone(),
                selection: Selection {
                    project: Some(ProjectId::new("p").unwrap()),
                    thread: None,
                },
            },
        );

    // The screen loads the project's servers through the core.
    until(&mut window, "the list is loaded", |w| {
        shows(w, "Add MCP server")
    })
    .await;

    // Add: fill the form of a stdio server whose environment holds a keychain
    // reference, then type its secret.
    click(&mut window, "Add MCP server");
    let peer = std::env::var("CARGO_BIN_EXE_e2e-mcp-peer").unwrap();
    click(&mut window, "Add to Environment");
    click(&mut window, "Keychain reference");
    // Text fields in form order: name, command, working directory, environment
    // name, environment value, startup timeout, request timeout.
    type_into(&mut window, Role::TextInput, 0, "mock");
    type_into(&mut window, Role::TextInput, 1, &peer);
    type_into(&mut window, Role::TextInput, 3, "TOKEN");
    type_into(&mut window, Role::TextInput, 4, "token");
    type_into(
        &mut window,
        Role::MultilineTextInput,
        0,
        root.join("control").to_str().unwrap(),
    );
    click(&mut window, "Set value…");
    let secret_role = if window.get_all_by_role(Role::PasswordInput).next().is_some() {
        Role::PasswordInput
    } else {
        Role::TextInput
    };
    let last = window.get_all_by_role(secret_role).count() - 1;
    type_into(&mut window, secret_role, last, SECRET);
    click(&mut window, "Store secret");
    // The secret reached the keychain and the server is listed (not approved yet).
    until(&mut window, "the server is listed", |w| {
        w.query_by_label("mock").is_some()
    })
    .await;
    assert!(
        !shows(&window, SECRET),
        "the typed secret is shown nowhere on the screen"
    );
    let config = std::fs::read_to_string(root.join(".evorch/mcp.toml")).unwrap();
    assert!(
        !config.contains(SECRET),
        "the secret is not in the config file"
    );
    let stored = keychain
        .get(&tools::mcp::secrets::credential_key(
            &tools::mcp::types::ApprovalScope {
                canonical_root: root.clone(),
                server_id: "mock".into(),
            },
            "token",
        ))
        .unwrap()
        .expect("the secret was stored under its scoped name");
    assert_eq!(stored.expose(), SECRET);
    until(&mut window, "the keychain entry is shown as stored", |w| {
        shows(w, "Keychain entry token: a value is stored")
    })
    .await;
    // The entry changes behind the screen's back while the files stay as they
    // are (the revision does not change): a reload shows the entry as it is.
    let key = tools::mcp::secrets::credential_key(
        &tools::mcp::types::ApprovalScope {
            canonical_root: root.clone(),
            server_id: "mock".into(),
        },
        "token",
    );
    keychain.delete(&key).unwrap();
    click(&mut window, "Reload");
    until(&mut window, "the missing entry is shown", |w| {
        shows(w, "Keychain entry token: no value is stored yet")
    })
    .await;
    keychain
        .set(&key, &Secret::from(SECRET.to_owned()))
        .unwrap();
    click(&mut window, "Reload");
    until(&mut window, "the stored entry is shown again", |w| {
        shows(w, "Keychain entry token: a value is stored")
    })
    .await;

    // Approve: preview what approving allows, then confirm.
    click(&mut window, "Review approval");
    until(&mut window, "the preview is shown", |w| {
        w.query_by_label("Approve").is_some()
    })
    .await;
    assert!(shows(&window, "Approve mock?"));
    click(&mut window, "Approve");
    until(&mut window, "the approval is confirmed", |w| {
        shows(w, "Approved. It is used from the next new run.")
    })
    .await;
    assert_eq!(
        started.load(Ordering::SeqCst),
        0,
        "approving starts nothing"
    );

    // Test: the core starts the mock server and the handshake succeeds.
    click(&mut window, "Test connection");
    until(&mut window, "the test result is shown", |w| {
        shows(w, "Test result: Ready")
    })
    .await;
    assert_eq!(started.load(Ordering::SeqCst), 1, "one server process ran");

    // Revoke: the next test is refused and starts no process.
    click(&mut window, "Revoke");
    until(&mut window, "the approval is revoked", |w| {
        shows(w, "Approval revoked.")
    })
    .await;
    click(&mut window, "Test connection");
    until(&mut window, "the refused test is shown", |w| {
        shows(w, "not approved") || shows(w, "ApprovalRevoked")
    })
    .await;
    assert_eq!(
        started.load(Ordering::SeqCst),
        1,
        "a revoked server starts nothing"
    );

    // Delete: the row goes away and the override leaves the file.
    click(&mut window, "Delete");
    until(&mut window, "the server is gone", |w| {
        w.query_by_label("mock").is_none()
    })
    .await;
    let config = std::fs::read_to_string(root.join(".evorch/mcp.toml")).unwrap_or_default();
    assert!(!config.contains("mock"), "{config}");

    // Shut the client and the core down.
    actions.send(Action::Shutdown).await.unwrap();
    desktop.await.unwrap();
    let _ = stop.send(true);
    serving.abort();
}
