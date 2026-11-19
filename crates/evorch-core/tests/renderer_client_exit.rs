//! A desktop client that exits, or is killed, lets go of its own subscriptions and
//! preview tabs only (Stage 16b PR V). A run's tab, another client's tab and the helper
//! itself keep running. Two clients connect through the production `unix::connection`;
//! "killed" is a connection that ends without any command, which is what the death of
//! the client process looks like to the core. Every wait is an event of the core.
#![cfg(all(target_os = "linux", feature = "renderer-test-helper"))]

use std::sync::Arc;

use evorch_core::{backend::HostBackend, unix};
use runtime::artifacts::{ArtifactPages as _, CaptureRequest, store_with_refs};
use workbench_host::areas::browser::run_pages::RuntimePages;
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams};
use workbench_protocol::{
    ids::{ProjectId, ThreadId},
    namespaces::{artifacts::ArtifactRef, browser::*},
    queries::{Query, QueryReply},
};
use workspace_state::{
    ProjectId as StateProject, SidebarDocument, SidebarStore, ThreadId as StateThread, ThreadRecord,
};

#[allow(dead_code)]
#[path = "renderer_support/data_peer.rs"]
mod data_peer;
use data_peer::Peer;

struct Idle;
#[async_trait::async_trait]
impl runtime::AgentModel for Idle {
    async fn complete(
        &self,
        _: &runtime::AgentInvocationContext,
        _: runtime::Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        unreachable!("no turn runs in this fixture")
    }
    fn selected_model(&self, _: runtime::Role, _: Option<&str>) -> String {
        "renderer-client-exit".into()
    }
}
struct Direct;
impl runtime::SandboxFactory for Direct {
    fn build(
        &self,
        _: &runtime::ExecutionPolicy,
        _: &runtime::WorkspaceMounts,
    ) -> Result<Arc<dyn sandbox::Sandbox>, sandbox::SandboxError> {
        Ok(Arc::new(sandbox::DirectSandbox::new_unchecked()))
    }
}

#[test]
fn a_client_that_exits_or_is_killed_releases_only_what_it_holds() {
    let dir = test_socket_dir::SplitDir::new();
    let root = dir.path().canonicalize().unwrap();
    // SAFETY: single-threaded at this point; this binary holds one test.
    unsafe {
        std::env::set_var(
            "EVORCH_RENDERER_EXEC",
            std::env::var_os("CARGO_BIN_EXE_evorch-renderer-test-helper").unwrap(),
        )
    };
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(run(dir, root));
}

async fn session(peer: &mut Peer) -> (RendererSessionRef, RendererId) {
    let id = peer
        .browser(BrowserCommand::CreateRendererSession {})
        .await
        .unwrap();
    match peer.output(&id).await {
        RendererOutput::Session {
            session,
            resume_token,
        } => (session, resume_token),
        other => panic!("unexpected output: {other:?}"),
    }
}

async fn tabs(peer: &Peer) -> Vec<RendererTab> {
    match peer
        .client
        .query(Query::Browser(BrowserQuery::ListRendererTabs {
            offset: 0,
            limit: MAX_RENDERER_TABS as u32,
        }))
        .await
        .unwrap()
    {
        QueryReply::Browser(BrowserReply::Tabs { tabs, .. }) => tabs,
        other => panic!("unexpected reply: {other:?}"),
    }
}

async fn run(dir: test_socket_dir::SplitDir, root: std::path::PathBuf) {
    let project_root = root.join("one");
    std::fs::create_dir_all(&project_root).unwrap();
    let store = SidebarStore::new(root.join("sidebar.json"));
    let mut document = SidebarDocument::default();
    document
        .sidebar
        .add_project(StateProject::new("p1"), "p1", &project_root)
        .unwrap();
    document.sidebar.threads.push(ThreadRecord::new(
        StateThread::new("t1"),
        StateProject::new("p1"),
        "t1",
    ));
    document.sidebar.refresh_derived();
    {
        let lock = store.lock().unwrap();
        store.write(&lock, &document).unwrap();
    }
    let core = BootstrappedHost::open_with_test_seams(
        BootstrapOptions {
            config_dir: root.clone(),
            project_root: project_root.clone(),
            ownership_root: dir.sockets().into(),
        },
        TestSeams {
            model: Arc::new(Idle),
            sandbox: Arc::new(Direct),
        },
    )
    .await
    .unwrap();
    let writer = core.composition.storage_writer.clone();
    writer
        .upsert_threads_host(&[storage::threads::ThreadUpsert {
            thread_id: "t1".into(),
            project_id: Some("p1".into()),
            project_root: Some(project_root.to_string_lossy().into_owned()),
            title: Some("t1".into()),
            root_run_id: Some("r1".into()),
            ..Default::default()
        }])
        .unwrap();
    std::fs::write(project_root.join("page.html"), "<p>page</p>").unwrap();
    let artifacts = store_with_refs(root.join("artifacts"), writer).unwrap();
    let record = artifacts
        .capture(
            &project_root,
            None,
            &project_root.join("page.html"),
            CaptureRequest {
                title: "page.html",
                caption: None,
                run_id: "r1".into(),
                root_run_id: "r1".into(),
                thread_id: Some("t1".into()),
            },
        )
        .unwrap();
    let page = ArtifactRef {
        project: ProjectId::new("p1").unwrap(),
        thread: ThreadId::new("t1").unwrap(),
        artifact_id: record.artifact_id.clone(),
        digest: record.sha256.clone(),
    };

    // The run's own page: a tab no client owns, as `inspect_artifact` opens it.
    let pages = RuntimePages::new(core.host.lock().unwrap().renderer_owner());
    pages
        .open_artifact("r1", b"<p>the run's page</p>".to_vec(), &record)
        .await
        .unwrap();
    assert_eq!(
        core.host.lock().unwrap().renderer_run_tabs().unwrap().len(),
        1
    );

    let socket = dir.sockets().join("renderer-client-exit.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let backend = Arc::new(HostBackend::new(core.host.clone(), "client-exit".into()));
    // Hooks run in registration order, after the renderer service's own: when this fires,
    // the service has already dropped what the ended connection held.
    let left = Arc::new(tokio::sync::Notify::new());
    {
        let left = left.clone();
        core.host
            .lock()
            .unwrap()
            .connections()
            .register(Arc::new(move |_, _| left.notify_one()));
    }
    let (_stop, stopped) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (backend, stopped) = (backend.clone(), stopped.clone());
            tokio::spawn(async move {
                let _ = unix::connection(stream, backend, stopped).await;
            });
        }
    });

    // Another client with a preview of its own, the whole time.
    let mut other = Peer::connect(&socket, "client-other").await;
    let (other_session, _) = session(&mut other).await;
    let other_tab = other.open(&page, &other_session).await;

    // The desktop: a session, a preview tab and a frame subscription that is read.
    let mut desktop = Peer::connect(&socket, "client-desktop").await;
    let (desktop_session, token) = session(&mut desktop).await;
    let desktop_tab = desktop.open(&page, &desktop_session).await;
    let id = desktop
        .browser(BrowserCommand::SubscribeRendererFrames {
            tab: desktop_tab.reference.clone(),
        })
        .await
        .unwrap();
    let RendererOutput::Subscription {
        subscription,
        token: frames_token,
        ..
    } = desktop.output(&id).await
    else {
        panic!("not a subscription")
    };
    let mut frames = desktop
        .channel()
        .await
        .bind_frames(&desktop.client, subscription, frames_token)
        .await
        .unwrap();
    desktop
        .browser(BrowserCommand::SetRendererStreaming {
            tab: desktop_tab.reference.clone(),
            enabled: true,
        })
        .await
        .unwrap();
    assert!(frames.next_frame().await.unwrap().is_some());

    // -- The desktop is killed: nothing is sent, the connection just ends. --
    drop(frames);
    drop(desktop);
    left.notified().await;
    let run_tabs = core.host.lock().unwrap().renderer_run_tabs().unwrap();
    assert_eq!(
        run_tabs.len(),
        1,
        "the run's tab is untouched: {run_tabs:?}"
    );
    assert!(pages.page("r1").is_some(), "the run still holds its page");
    let seen = tabs(&other).await;
    assert!(
        seen.iter().any(|t| t.reference == other_tab.reference),
        "another client's tab is untouched"
    );
    let status = other
        .client
        .query(Query::Browser(BrowserQuery::GetRendererStatus {}))
        .await
        .unwrap();
    assert!(
        matches!(&status, QueryReply::Browser(BrowserReply::Status(s)) if s.phase == RendererPhase::Ready),
        "the helper keeps running: {status:?}"
    );
    // Its session waits out the grace: the same client finds it again by its token.
    let mut desktop = Peer::connect(&socket, "client-desktop").await;
    let id = desktop
        .browser(BrowserCommand::ResumeRendererSession {
            session: desktop_session.clone(),
            token,
        })
        .await
        .unwrap();
    let RendererOutput::Session { .. } = desktop.output(&id).await else {
        panic!("the session was not resumed")
    };
    assert!(
        tabs(&desktop)
            .await
            .iter()
            .any(|t| t.reference == desktop_tab.reference),
        "the resumed session still has its tab"
    );

    // -- A clean exit: the desktop closes its own tab and nothing else. --
    let id = desktop
        .browser(BrowserCommand::CloseRendererTab {
            tab: desktop_tab.reference.clone(),
        })
        .await
        .unwrap();
    desktop.output(&id).await;
    let after = tabs(&other).await;
    assert!(after.iter().all(|t| t.reference != desktop_tab.reference));
    assert!(after.iter().any(|t| t.reference == other_tab.reference));
    drop(desktop);
    left.notified().await;
    assert_eq!(
        core.host.lock().unwrap().renderer_run_tabs().unwrap().len(),
        1,
        "the run's tab survives both ways out"
    );
    assert!(pages.page("r1").is_some());
    assert!(pages.close("r1"));
}
