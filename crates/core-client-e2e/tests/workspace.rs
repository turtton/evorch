//! The sidebar of the desktop client, from a click to the core's sidebar file:
//! two windows, each a real `WorkspaceFeature` drawn by egui and driven through
//! its accessibility tree, share one production core composition over a Unix
//! socket. Rename, pin, archive and trust edits made in one window show up in
//! the other, and both windows converge again after the core is stopped and
//! started over the same files. Every wait observes an explicit state of the
//! shared client or a screen element, never a duration.
use async_trait::async_trait;
use egui::accesskit::Role;
use egui_kittest::{Harness, kittest::Queryable};
use gui::core_client::{
    Shared,
    connection::{Action, serve},
    enqueue,
    features::{FeatureView, Navigation, Selection},
};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams};
use workbench_protocol::ids::ProjectId;
use workspace_state::{SidebarDocument, SidebarStore, ThreadId, ThreadRecord, TrustState};

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
        "sidebar-screen".into()
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
    let outcome = {
        let mut shared = screen.shared.lock().unwrap();
        let Shared {
            features,
            cache,
            capabilities,
            connected,
            ..
        } = &mut *shared;
        features.render_selector(
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
    for action in outcome.frame.actions {
        enqueue(&screen.actions, &mut shared, Action::Feature(action));
    }
    for navigation in outcome.frame.navigation {
        if let Navigation::SelectProject(project) = navigation {
            screen.selection = Selection {
                project: Some(project),
                thread: None,
            };
        }
    }
}

type Window = Harness<'static, Screen>;

/// The sidebar has read everything it shows: no request is outstanding, the
/// threads' details and the project's access are known. A click made before
/// that would act on a model the core is about to replace.
fn ready(window: &Window) -> bool {
    let shared = window.state().shared.lock().unwrap();
    let feature = shared.features.workspace().unwrap();
    feature.settled() && feature.model().details.len() == 2 && feature.model().access.len() == 1
}

fn click_nth(window: &mut Window, label: &str, nth: usize) {
    window.get_all_by_label(label).nth(nth).unwrap().click();
    window.run_steps(4);
}

/// Clicks the `nth` element labelled `label` once the sidebar has read what
/// it shows and the element is on the screen.
async fn click(window: &mut Window, label: &str, nth: usize) {
    until(window, |w| {
        ready(w) && w.query_all_by_label(label).nth(nth).is_some()
    })
    .await;
    click_nth(window, label, nth);
}

fn shows(window: &Window, text: &str) -> bool {
    window.query_by_label_contains(text).is_some()
}

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

async fn both(a: &mut Window, b: &mut Window, shown: impl Fn(&Window) -> bool + Copy) {
    until(a, shown).await;
    until(b, shown).await;
}

async fn open_core(config: &Path, repo: &Path, owners: &Path) -> BootstrappedHost {
    BootstrappedHost::open_with_test_seams(
        BootstrapOptions {
            config_dir: config.to_path_buf(),
            project_root: repo.to_path_buf(),
            ownership_root: owners.to_path_buf(),
        },
        TestSeams {
            model: Arc::new(Idle),
            sandbox: Arc::new(Unisolated),
        },
    )
    .await
    .unwrap()
}

fn seed(config: &Path, repo: &Path) {
    let titles = [("t1", "first", 2_000), ("t2", "second", 1_000)];
    let mut document = SidebarDocument::default();
    document
        .sidebar
        .add_project(workspace_state::ProjectId::new("p1"), "repo", repo)
        .unwrap();
    for (id, title, at) in titles {
        let mut thread = ThreadRecord::new(
            ThreadId::new(id),
            workspace_state::ProjectId::new("p1"),
            title,
        );
        thread.created_at = at;
        thread.updated_at = at;
        document.sidebar.threads.push(thread);
    }
    document.sidebar.refresh_derived();
    let store = SidebarStore::new(config.join("sidebar.json"));
    let lock = store.lock().unwrap();
    store.write(&lock, &document).unwrap();
    drop(lock);
    let storage = storage::Storage::open(storage::StorageConfig {
        db_path: config.join("evorch-events.db"),
        ..Default::default()
    })
    .unwrap();
    let upserts: Vec<_> = titles
        .iter()
        .map(|(id, title, at)| storage::threads::ThreadUpsert {
            thread_id: (*id).to_owned(),
            project_id: Some("p1".into()),
            project_root: Some(repo.to_string_lossy().into_owned()),
            title: Some((*title).to_owned()),
            archived: Some(false),
            pinned: Some(false),
            created_at_ns: at * 1_000_000_000,
            updated_at_ns: at * 1_000_000_000,
            ..storage::threads::ThreadUpsert::default()
        })
        .collect();
    storage.handle().upsert_threads_host(&upserts).unwrap();
    storage.close();
}

#[tokio::test(flavor = "multi_thread")]
async fn two_windows_edit_the_sidebar_through_the_core_and_converge_across_a_core_restart() {
    let dir = test_socket_dir::SplitDir::new();
    let root = dir.path().canonicalize().unwrap();
    let (config, repo, owners) = (
        root.join("config"),
        root.join("repo"),
        dir.sockets().to_path_buf(),
    );
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    let repo = repo.canonicalize().unwrap();
    seed(&config, &repo);
    let mut boot = open_core(&config, &repo, &owners).await;

    let identity = core_instance::ConfigIdentity::new(&root.join("identity")).unwrap();
    let instance = evorch_core::instance::Instance::acquire(
        &core_instance::LaunchGuard::acquire(&identity).unwrap(),
        dir.sockets(),
    )
    .unwrap();
    let listener = instance.bind().unwrap();
    listener.set_nonblocking(true).unwrap();
    let listener = tokio::net::UnixListener::from_std(listener).unwrap();
    let backend_for = |boot: &BootstrappedHost| {
        Arc::new(evorch_core::backend::HostBackend::new(
            boot.host.clone(),
            instance.identity.clone(),
        ))
    };
    let current = Arc::new(Mutex::new(backend_for(&boot)));
    let cuts = Arc::new(Mutex::new(Vec::<tokio::sync::watch::Sender<bool>>::new()));
    let (accepted, mut acceptances) = tokio::sync::watch::channel(0usize);
    let serving = tokio::spawn({
        let (current, cuts, accepted) = (current.clone(), cuts.clone(), accepted.clone());
        async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (cut, stopped) = tokio::sync::watch::channel(false);
                cuts.lock().unwrap().push(cut);
                let backend = current.lock().unwrap().clone();
                accepted.send_modify(|count| *count += 1);
                tokio::spawn(evorch_core::unix::connection(stream, backend, stopped));
            }
        }
    });

    let open_window = || {
        let launcher = core_launcher::Launcher::with_socket(
            identity.clone(),
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
        let window: Window = Harness::builder()
            .with_size(egui::vec2(1000.0, 800.0))
            .build_ui_state(
                draw,
                Screen {
                    shared,
                    actions: actions.clone(),
                    selection: Selection {
                        project: Some(ProjectId::new("p1").unwrap()),
                        thread: None,
                    },
                },
            );
        (window, actions, desktop)
    };
    let (mut a, actions_a, desktop_a) = open_window();
    let (mut b, actions_b, desktop_b) = open_window();

    eprintln!("STAGE initial");
    both(&mut a, &mut b, |w| {
        shows(w, "first") && shows(w, "second") && shows(w, "Trust: Unapproved")
    })
    .await;

    eprintln!("STAGE Rename in A");
    // Rename in A: the title the core holds shows in both windows.
    click(&mut a, "Rename", 0).await;
    a.get_all_by_role(Role::TextInput).next().unwrap().click();
    a.run_steps(4);
    a.get_all_by_role(Role::TextInput)
        .next()
        .unwrap()
        .type_text(" v2");
    a.run_steps(4);
    a.key_press(egui::Key::Enter);
    a.run_steps(4);
    both(&mut a, &mut b, |w| shows(w, "first v2")).await;

    eprintln!("STAGE Pin in A");
    // Pin in A.
    click(&mut a, "Pin", 0).await;
    both(&mut a, &mut b, |w| shows(w, "📌 first v2")).await;

    eprintln!("STAGE Trust in A");
    // Trust in A.
    click(&mut a, "Approve project", 0).await;
    both(&mut a, &mut b, |w| shows(w, "Trust: Approved")).await;

    eprintln!("STAGE Unpin in B");
    // Unpin in B.
    click(&mut b, "Unpin", 0).await;
    both(&mut a, &mut b, |w| shows(w, "first v2") && !shows(w, "📌")).await;

    eprintln!("STAGE Trust revoked in B");
    // Trust revoked in B, approved again in A.
    click(&mut b, "Revoke trust", 0).await;
    both(&mut a, &mut b, |w| shows(w, "Trust: Unapproved")).await;
    click(&mut a, "Approve project", 0).await;
    both(&mut a, &mut b, |w| shows(w, "Trust: Approved")).await;

    eprintln!("STAGE Archive in A");
    // Archive in A: the thread leaves both windows, the other one stays.
    click(&mut a, "Archive", 0).await;
    both(&mut a, &mut b, |w| {
        !shows(w, "first v2") && shows(w, "second")
    })
    .await;

    eprintln!("STAGE The core stops");
    // The core stops and starts over the same files; both windows are cut and
    // reconnect, and show what the files hold.
    let before = *accepted.borrow();
    let previous_epoch = a
        .state()
        .shared
        .lock()
        .unwrap()
        .cache
        .cursor()
        .unwrap()
        .core_epoch;
    boot.shutdown().await.unwrap();
    drop(boot);
    let mut boot = open_core(&config, &repo, &owners).await;
    *current.lock().unwrap() = backend_for(&boot);
    for cut in cuts.lock().unwrap().drain(..) {
        cut.send(true).unwrap();
    }
    // The windows reconnect when asked to (a user's "reconnect" or the
    // supervisor's retry), not on their own.
    for actions in [&actions_a, &actions_b] {
        actions.send(Action::Reconnect).await.unwrap();
    }
    acceptances
        .wait_for(|count| *count >= before + 2)
        .await
        .unwrap();
    both(&mut a, &mut b, |w| {
        let shared = w.state().shared.lock().unwrap();
        let fresh = shared.connected
            && shared
                .cache
                .cursor()
                .is_some_and(|cursor| cursor.core_epoch != previous_epoch);
        drop(shared);
        fresh
            && ready(w)
            && shows(w, "second")
            && shows(w, "Trust: Approved")
            && !shows(w, "first v2")
    })
    .await;

    eprintln!("STAGE Restore after the restart");
    // The archived conversation is listed on request and restored from there.
    until(&mut a, |w| shows(w, "Show archived")).await;
    a.get_by_label("Show archived").click();
    a.run_steps(4);
    until(&mut a, |w| shows(w, "(archived) first v2")).await;
    click(&mut a, "Restore", 0).await;
    both(&mut a, &mut b, |w| shows(w, "first v2")).await;
    let document = SidebarStore::new(config.join("sidebar.json"))
        .read_unlocked()
        .unwrap();
    let states = document.sidebar.resolve_states();
    assert!(!states[&ThreadId::new("t1")].archived);
    assert!(!states[&ThreadId::new("t1")].pinned);
    assert!(!states[&ThreadId::new("t2")].archived);
    assert_eq!(document.sidebar.projects[0].trust, TrustState::Approved);
    let renamed = document
        .sidebar
        .threads
        .iter()
        .find(|t| t.id == ThreadId::new("t1"))
        .unwrap();
    assert_eq!(renamed.title, "first v2");

    for actions in [&actions_a, &actions_b] {
        actions.send(Action::Shutdown).await.unwrap();
    }
    desktop_a.await.unwrap();
    desktop_b.await.unwrap();
    serving.abort();
    boot.shutdown().await.unwrap();
}
