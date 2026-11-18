//! F2's authorization matrix (plan §8.1) through the production composition:
//! a bootstrapped core, the executable fake helper, and clients A and B that
//! really open tabs. Every wait is an event of the shared stream or a
//! lifecycle event, never a duration.
//!
//! Covered: the owner's list and pagination, Capture and frame-token retrieval,
//! another owner, the Web surface and observers (metadata only, then frames),
//! and the shared delivery (the first All-scope subscription, its replay after
//! a reconnect, and a snapshot) leaking no body, image, retrieval reference,
//! token or helper free text.
#![cfg(all(target_os = "linux", feature = "renderer-test-helper"))]
/// What the fake helper (`tests/support/renderer_test_helper.rs`) returns for a capture.
const FAKE_PIXELS: &[u8] = b"\x89PNG\r\n\x1a\nFAKE-PIXELS";

use std::sync::Arc;

use runtime::artifacts::{ArtifactRecord, CaptureRequest, store_with_refs};
use workbench_host::{
    bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams},
    projection::Projection,
    subscriptions::Subscription,
};
use workbench_protocol::{
    commands::{Command, CommandResult},
    envelope::{Provenance, Surface},
    errors::ErrorCode,
    events::{EventCursor, EventEnvelope, Subscribe, SubscriptionScope, UiEvent},
    ids::{ClientId, ProjectId, SessionId, ThreadId},
    namespaces::{
        artifacts::{ArtifactRef, ArtifactsCommand, ArtifactsResult},
        browser::*,
    },
    queries::{Query, QueryReply},
    wire::{InstanceEpoch, Sequence},
};
use workspace_state::{
    ProjectId as StateProject, SidebarDocument, SidebarStore, ThreadId as StateThread, ThreadRecord,
};

const BODY: &str = "<p>日本語の本文 é</p><img src=\"data:image/png;base64,QUJD\">";

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
        "renderer-authorization".into()
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

fn who(surface: Surface, client: &str) -> Provenance {
    Provenance::human(surface, ClientId::new(client).unwrap())
}
fn reference(project: &str, thread: &str, record: &ArtifactRecord) -> ArtifactRef {
    ArtifactRef {
        project: ProjectId::new(project).unwrap(),
        thread: ThreadId::new(thread).unwrap(),
        artifact_id: record.artifact_id.clone(),
        digest: record.sha256.clone(),
    }
}

struct Fixture {
    core: BootstrappedHost,
    events: Subscription,
    /// Every event the first subscription delivered, in order.
    seen: Vec<EventEnvelope>,
    secrets: Vec<String>,
    /// Operations that ran as a task: they publish `OperationChanged` when accepted and
    /// again when they end. The others (a session, a grant, a subscription) are
    /// answered at once and publish it once.
    tasks: std::sync::Mutex<std::collections::HashSet<RendererId>>,
}
impl Fixture {
    fn command(&self, command: BrowserCommand, p: &Provenance) -> Result<RendererId, ErrorCode> {
        let immediate = matches!(
            command,
            BrowserCommand::CreateRendererSession {}
                | BrowserCommand::ResumeRendererSession { .. }
                | BrowserCommand::UnsubscribeRendererFrames { .. }
                | BrowserCommand::GrantRendererObserver { .. }
                | BrowserCommand::SubscribeRendererFrames { .. }
        );
        let reply = self
            .core
            .host
            .lock()
            .unwrap()
            .handle_command(Command::Browser(command), p.clone())
            .map_err(|error| error.code)?;
        match reply.result.as_deref() {
            Some(CommandResult::Browser(BrowserResult::Accepted { operation_id })) => {
                if !immediate {
                    self.tasks.lock().unwrap().insert(operation_id.clone());
                }
                Ok(operation_id.clone())
            }
            other => panic!("unexpected receipt: {other:?}"),
        }
    }
    fn preview(
        &self,
        artifact: &ArtifactRef,
        session: &RendererSessionRef,
        p: &Provenance,
    ) -> Result<RendererId, ErrorCode> {
        let reply = self
            .core
            .host
            .lock()
            .unwrap()
            .handle_command(
                Command::Artifacts(ArtifactsCommand::OpenPreview {
                    artifact: artifact.clone(),
                    session: session.clone(),
                    viewport: RendererViewport {
                        width: 1024,
                        height: 768,
                    },
                }),
                p.clone(),
            )
            .map_err(|error| error.code)?;
        match reply.result.as_deref() {
            Some(CommandResult::Artifacts(ArtifactsResult::Accepted { operation_id })) => {
                self.tasks.lock().unwrap().insert(operation_id.clone());
                Ok(operation_id.clone())
            }
            other => panic!("unexpected receipt: {other:?}"),
        }
    }
    fn query(&self, query: BrowserQuery, p: &Provenance) -> Result<BrowserReply, ErrorCode> {
        match self
            .core
            .host
            .lock()
            .unwrap()
            .handle_query(
                &SessionId::new("session").unwrap(),
                &Query::Browser(query),
                p,
            )
            .map_err(|error| error.code)?
        {
            QueryReply::Browser(reply) => Ok(reply),
            other => panic!("unexpected reply: {other:?}"),
        }
    }
    fn tabs(
        &self,
        offset: u32,
        limit: u32,
        p: &Provenance,
    ) -> Result<(Vec<RendererTab>, bool), ErrorCode> {
        match self.query(BrowserQuery::ListRendererTabs { offset, limit }, p)? {
            BrowserReply::Tabs { tabs, more } => Ok((tabs, more)),
            other => panic!("unexpected reply: {other:?}"),
        }
    }
    fn tab(&self, tab: &RendererTabRef, p: &Provenance) -> Result<RendererTab, ErrorCode> {
        match self.query(BrowserQuery::GetRendererTab { tab: tab.clone() }, p)? {
            BrowserReply::Tab(tab) => Ok(*tab),
            other => panic!("unexpected reply: {other:?}"),
        }
    }
    fn operation(
        &self,
        id: &RendererId,
        p: &Provenance,
    ) -> Result<RendererOperationState, ErrorCode> {
        match self.query(
            BrowserQuery::GetRendererOperation {
                operation_id: id.clone(),
            },
            p,
        )? {
            BrowserReply::Operation(operation) => Ok(operation.state),
            other => panic!("unexpected reply: {other:?}"),
        }
    }
    /// The terminal state of an operation. An operation publishes
    /// `OperationChanged` once when a task is accepted and once when it ends
    /// (an immediate operation once), so the stream is read until they are here: whatever the operation put on the
    /// shared stream is then in `seen` before anything is checked.
    async fn finished(&mut self, id: &RendererId, p: &Provenance) -> RendererOperationState {
        let state = loop {
            let state = self.operation(id, p).unwrap();
            if state != RendererOperationState::Running {
                break state;
            }
            let event = self.events.recv().await.expect("the stream stays open");
            self.seen.push(event);
        };
        let expected = if self.tasks.lock().unwrap().contains(id) {
            2
        } else {
            1
        };
        while self
            .seen
            .iter()
            .filter(|e| {
                matches!(&e.event,
                    UiEvent::Browser(BrowserEvent::OperationChanged { id: changed })
                        if changed == id)
            })
            .count()
            < expected
        {
            let event = self.events.recv().await.expect("the stream stays open");
            self.seen.push(event);
        }
        state
    }
    async fn output(&mut self, id: &RendererId, p: &Provenance) -> RendererOutput {
        match self.finished(id, p).await {
            RendererOperationState::Completed { output } => {
                self.note(&output);
                output
            }
            other => panic!("the operation did not complete: {other:?}"),
        }
    }
    /// What must never appear in a shared delivery.
    fn note(&mut self, output: &RendererOutput) {
        match output {
            RendererOutput::Session { resume_token, .. } => {
                self.secrets.push(resume_token.as_str().to_owned())
            }
            RendererOutput::Capture { capture_id, .. } => {
                self.secrets.push(capture_id.as_str().to_owned())
            }
            RendererOutput::Subscription {
                subscription,
                token,
                ..
            } => {
                self.secrets.push(subscription.as_str().to_owned());
                self.secrets.push(token.as_str().to_owned());
            }
            _ => {}
        }
    }
    async fn session(&mut self, p: &Provenance) -> RendererSessionRef {
        let id = self
            .command(BrowserCommand::CreateRendererSession {}, p)
            .unwrap();
        match self.output(&id, p).await {
            RendererOutput::Session { session, .. } => session,
            other => panic!("unexpected output: {other:?}"),
        }
    }
    async fn open(
        &mut self,
        artifact: &ArtifactRef,
        session: &RendererSessionRef,
        p: &Provenance,
    ) -> RendererTab {
        let id = self.preview(artifact, session, p).unwrap();
        match self.output(&id, p).await {
            RendererOutput::Tab(tab) => *tab,
            other => panic!("unexpected output: {other:?}"),
        }
    }
    fn assert_no_leak(&self, label: &str, text: &str) {
        for needle in [
            "FAKE-PIXELS",
            "HELPER-FREE-TEXT",
            "HELPER-CDP-SECRET",
            "日本語の本文",
            "QUJD",
            "data:image",
            "page.html",
            "note.html",
            "one/",
            "two/",
        ]
        .into_iter()
        .map(str::to_owned)
        .chain(self.secrets.iter().cloned())
        {
            assert!(!text.contains(&needle), "{label} leaks {needle:?}: {text}");
        }
    }
}

fn projection(f: &Fixture) -> Arc<Projection> {
    f.core.host.lock().unwrap().projection().clone()
}

#[test]
fn owners_observers_and_the_web_are_authorized_and_shared_delivery_leaks_nothing() {
    let dir = test_socket_dir::SplitDir::new();
    let root = dir.path().canonicalize().unwrap();
    let (one, two) = (root.join("one"), root.join("two"));
    for path in [&one, &two] {
        std::fs::create_dir_all(path).unwrap();
    }
    // The core reads the helper from this variable when it bootstraps; this
    // binary holds one test and sets it before any thread exists.
    // SAFETY: single-threaded at this point.
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
        .block_on(run(dir, root, one, two));
}

async fn run(
    dir: test_socket_dir::SplitDir,
    root: std::path::PathBuf,
    one: std::path::PathBuf,
    two: std::path::PathBuf,
) {
    let store = SidebarStore::new(root.join("sidebar.json"));
    let mut document = SidebarDocument::default();
    for (project, path, thread) in [("p1", &one, "t1"), ("p2", &two, "t2")] {
        document
            .sidebar
            .add_project(StateProject::new(project), project, path)
            .unwrap();
        document.sidebar.threads.push(ThreadRecord::new(
            StateThread::new(thread),
            StateProject::new(project),
            thread,
        ));
    }
    document.sidebar.refresh_derived();
    {
        let lock = store.lock().unwrap();
        store.write(&lock, &document).unwrap();
    }
    let core = BootstrappedHost::open_with_test_seams(
        BootstrapOptions {
            config_dir: root.clone(),
            project_root: one.clone(),
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
    for (project, path, thread, run) in [("p1", &one, "t1", "r1"), ("p2", &two, "t2", "r2")] {
        writer
            .upsert_threads_host(&[storage::threads::ThreadUpsert {
                thread_id: thread.into(),
                project_id: Some(project.into()),
                project_root: Some(path.to_string_lossy().into_owned()),
                title: Some(thread.into()),
                root_run_id: Some(run.into()),
                ..Default::default()
            }])
            .unwrap();
    }
    std::fs::write(one.join("page.html"), BODY).unwrap();
    std::fs::write(one.join("note.html"), "<p>second</p>").unwrap();
    std::fs::write(two.join("other.html"), "<p>other</p>").unwrap();
    let artifacts = store_with_refs(root.join("artifacts"), writer).unwrap();
    let capture = |base: &std::path::Path, file: &str, run: &str, thread: &str| {
        artifacts
            .capture(
                base,
                None,
                &base.join(file),
                CaptureRequest {
                    title: file,
                    caption: None,
                    run_id: run.into(),
                    root_run_id: run.into(),
                    thread_id: Some(thread.into()),
                },
            )
            .unwrap()
    };
    let page_record = capture(&one, "page.html", "r1", "t1");
    let page = reference("p1", "t1", &page_record);
    let note = reference("p1", "t1", &capture(&one, "note.html", "r1", "t1"));
    let other = reference("p2", "t2", &capture(&two, "other.html", "r2", "t2"));

    let (a, b) = (
        who(Surface::Desktop, "client-a"),
        who(Surface::Desktop, "client-b"),
    );
    let web = who(Surface::Web, "client-a");
    let connections = core.host.lock().unwrap().connections();
    let _connected = [
        connections.open(ClientId::new("client-a").unwrap()),
        connections.open(ClientId::new("client-b").unwrap()),
    ];
    let events = core
        .host
        .lock()
        .unwrap()
        .projection()
        .subscribe_with_snapshot(&Subscribe {
            scope: SubscriptionScope::All,
            after: None,
        })
        .unwrap();
    let first_started = serde_json::to_string(&events.started).unwrap();
    assert!(
        events.started.snapshot_id.is_some(),
        "a first subscription takes a snapshot"
    );
    let mut f = Fixture {
        core,
        events,
        seen: Vec::new(),
        secrets: Vec::new(),
        tasks: Default::default(),
    };
    f.assert_no_leak("the first snapshot", &first_started);

    // Each client opens tabs on its own session.
    let (session_a, session_b) = (f.session(&a).await, f.session(&b).await);
    let a1 = f.open(&page, &session_a, &a).await;
    let a2 = f.open(&note, &session_a, &a).await;
    let b1 = f.open(&other, &session_b, &b).await;
    // The artifact claim is verified against the stored thread, not the client.
    let mut claimed = page.clone();
    claimed.project = ProjectId::new("p2").unwrap();
    claimed.thread = ThreadId::new("t2").unwrap();
    assert!(f.preview(&claimed, &session_a, &a).is_err());

    // Listing is per owner and pages.
    let names = |tabs: &[RendererTab]| {
        let mut ids: Vec<_> = tabs
            .iter()
            .map(|t| t.reference.id.as_str().to_owned())
            .collect();
        ids.sort();
        ids
    };
    let mut expected = vec![
        a1.reference.id.as_str().to_owned(),
        a2.reference.id.as_str().to_owned(),
    ];
    expected.sort();
    let (listed, more) = f.tabs(0, 4, &a).unwrap();
    assert!(!more);
    assert_eq!(names(&listed), expected);
    let (first, more_first) = f.tabs(0, 1, &a).unwrap();
    let (second, more_second) = f.tabs(1, 1, &a).unwrap();
    assert_eq!(
        (first.len(), more_first, second.len(), more_second),
        (1, true, 1, false)
    );
    assert_eq!(names(&[first, second].concat()), expected);
    let (listed_b, _) = f.tabs(0, 4, &b).unwrap();
    assert_eq!(names(&listed_b), vec![b1.reference.id.as_str().to_owned()]);
    assert_eq!(f.tabs(0, 4, &web).unwrap_err(), ErrorCode::PermissionDenied);
    // The same client id on another surface owns nothing.
    if let Ok((tabs, _)) = f.tabs(0, 4, &who(Surface::Cli, "client-a")) {
        assert!(tabs.is_empty());
    }

    // The owner captures; the retrieval reference is private to the owner.
    let capture_op = f
        .command(
            BrowserCommand::CaptureRendererTab {
                tab: a1.reference.clone(),
            },
            &a,
        )
        .unwrap();
    match f.output(&capture_op, &a).await {
        RendererOutput::Capture { byte_len, .. } => {
            assert_eq!(byte_len as usize, FAKE_PIXELS.len())
        }
        other => panic!("unexpected output: {other:?}"),
    }
    for denied in [&b, &web] {
        assert_eq!(
            f.command(
                BrowserCommand::CaptureRendererTab {
                    tab: a1.reference.clone()
                },
                denied
            )
            .unwrap_err(),
            ErrorCode::PermissionDenied
        );
        assert_eq!(
            f.operation(&capture_op, denied).unwrap_err(),
            ErrorCode::PermissionDenied
        );
        assert_eq!(
            f.tab(&a1.reference, denied).unwrap_err(),
            ErrorCode::PermissionDenied
        );
    }
    assert_eq!(
        f.operation(&capture_op, &web).unwrap_err(),
        ErrorCode::PermissionDenied
    );

    // An observer with metadata only reads the tab and can do nothing else.
    let grant = |metadata, frames| BrowserCommand::GrantRendererObserver {
        tab: a1.reference.clone(),
        client: ClientId::new("client-b").unwrap(),
        metadata,
        frames,
    };
    let granted = f.command(grant(true, false), &a).unwrap();
    f.output(&granted, &a).await;
    assert_eq!(f.tab(&a1.reference, &b).unwrap(), a1);
    for command in [
        BrowserCommand::CaptureRendererTab {
            tab: a1.reference.clone(),
        },
        BrowserCommand::RendererCdp {
            tab: a1.reference.clone(),
            method: "Runtime.evaluate".into(),
            params: serde_json::json!({}),
        },
        BrowserCommand::CloseRendererTab {
            tab: a1.reference.clone(),
        },
        BrowserCommand::SubscribeRendererFrames {
            tab: a1.reference.clone(),
        },
        grant(true, true),
    ] {
        assert_eq!(
            f.command(command, &b).unwrap_err(),
            ErrorCode::PermissionDenied,
            "a metadata observer is read-only"
        );
    }
    assert_eq!(
        f.tab(&a1.reference, &web).unwrap_err(),
        ErrorCode::PermissionDenied
    );
    // A frames grant yields a subscription and a token for B alone.
    let granted = f.command(grant(true, true), &a).unwrap();
    f.output(&granted, &a).await;
    let lease = f
        .command(
            BrowserCommand::SubscribeRendererFrames {
                tab: a1.reference.clone(),
            },
            &b,
        )
        .unwrap();
    assert!(matches!(
        f.output(&lease, &b).await,
        RendererOutput::Subscription { .. }
    ));
    assert_eq!(
        f.operation(&lease, &a).unwrap_err(),
        ErrorCode::PermissionDenied
    );
    // Revoking removes the observer entirely.
    let revoked = f.command(grant(false, false), &a).unwrap();
    f.output(&revoked, &a).await;
    assert_eq!(
        f.tab(&a1.reference, &b).unwrap_err(),
        ErrorCode::PermissionDenied
    );

    // A helper failure reaches the client as a structured reason only. The
    // probe is an allowed CDP method, so it passes the real validation and
    // reaches the helper, which answers with free text.
    let cdp = |probe: &str| BrowserCommand::RendererCdp {
        tab: a1.reference.clone(),
        method: "DOM.getDocument".into(),
        params: serde_json::json!({ "probe": probe }),
    };
    let failing = f.command(cdp("error"), &a).unwrap();
    assert_eq!(
        f.finished(&failing, &a).await,
        RendererOperationState::Failed {
            reason: RendererFailure::Unavailable
        },
        "the helper's error becomes a structured reason"
    );
    // The same helper still serves the owner's next request, and the result
    // (which carries a secret) reaches the owner alone. `served == 2` shows the
    // failing probe really reached the helper.
    let reading = f.command(cdp("secret"), &a).unwrap();
    match f.output(&reading, &a).await {
        RendererOutput::Cdp { result } => {
            assert_eq!(result["secret"], "HELPER-CDP-SECRET");
            assert_eq!(result["served"], 2, "both probes reached the helper");
        }
        other => panic!("unexpected output: {other:?}"),
    }
    assert_eq!(
        f.operation(&reading, &b).unwrap_err(),
        ErrorCode::PermissionDenied
    );
    assert_eq!(
        f.operation(&reading, &web).unwrap_err(),
        ErrorCode::PermissionDenied
    );
    assert_eq!(
        f.query(BrowserQuery::GetRendererStatus {}, &a).map(
            |reply| matches!(reply, BrowserReply::Status(s) if s.phase == RendererPhase::Ready)
        ),
        Ok(true),
        "an expected failure does not fail the renderer"
    );

    // A run that did not create the artifact but belongs to the same
    // conversation shows it (`inspect_artifact` allows that): its tab is kept.
    f.core
        .composition
        .storage_writer
        .upsert_threads_host(&[storage::threads::ThreadUpsert {
            thread_id: "t1".into(),
            root_run_id: Some("r1b".into()),
            ..Default::default()
        }])
        .unwrap();
    let pages = workbench_host::areas::browser::run_pages::RuntimePages::new(
        f.core.host.lock().unwrap().renderer_owner(),
    );
    use runtime::artifacts::ArtifactPages as _;
    assert_ne!(page_record.run_id, "r1b", "another run than the creator");
    pages
        .open_artifact("r1b", b"<p>shared page</p>".to_vec(), &page_record)
        .await
        .unwrap();
    let run_tabs = f.core.host.lock().unwrap().renderer_run_tabs().unwrap();
    assert_eq!(run_tabs.len(), 1, "{run_tabs:?}");
    assert_eq!(run_tabs[0].owner, RendererOwnerKind::Run);
    assert_eq!(run_tabs[0].artifact, page);
    // A granted observer reads the run tab's metadata; the Web does not.
    let run = run_tabs[0].reference.clone();
    let granted = f
        .command(
            BrowserCommand::GrantRendererObserver {
                tab: run.clone(),
                client: ClientId::new("client-b").unwrap(),
                metadata: true,
                frames: false,
            },
            &a,
        )
        .unwrap();
    f.output(&granted, &a).await;
    assert_eq!(f.tab(&run, &b).unwrap().artifact, page);
    assert_eq!(f.tab(&run, &web).unwrap_err(), ErrorCode::PermissionDenied);
    assert!(pages.close("r1b"));

    // Closing a tab ends its listing.
    let closed = f
        .command(
            BrowserCommand::CloseRendererTab {
                tab: a2.reference.clone(),
            },
            &a,
        )
        .unwrap();
    f.output(&closed, &a).await;
    assert_eq!(f.tabs(0, 4, &a).unwrap().0.len(), 1);

    // The shared delivery: what the first subscription saw, its replay after a
    // reconnect, and a new snapshot (another epoch's cursor, or none).
    let browser_events = f
        .seen
        .iter()
        .filter(|e| matches!(e.event, UiEvent::Browser(_)))
        .count();
    assert!(
        browser_events > 0,
        "the shared stream carries the opaque renderer events"
    );
    f.assert_no_leak(
        "the first subscription",
        &serde_json::to_string(&f.seen).unwrap(),
    );
    let anchor = f
        .seen
        .iter()
        .position(|e| matches!(e.event, UiEvent::Browser(_)))
        .unwrap();
    let cursor = f.seen[anchor].cursor;
    let mut replay = projection(&f)
        .subscribe_with_snapshot(&Subscribe {
            scope: SubscriptionScope::All,
            after: Some(cursor),
        })
        .unwrap();
    assert!(
        replay.started.snapshot_id.is_none(),
        "the cursor is replayable"
    );
    assert_eq!(replay.started.high_water, cursor);
    let later = &f.seen[anchor + 1..];
    let mut replayed = Vec::new();
    for _ in later {
        replayed.push(
            replay
                .recv()
                .await
                .expect("the replay delivers the later events"),
        );
    }
    assert_eq!(
        replayed
            .iter()
            .map(|e| (e.cursor, &e.event))
            .collect::<Vec<_>>(),
        later
            .iter()
            .map(|e| (e.cursor, &e.event))
            .collect::<Vec<_>>(),
        "a reconnect replays exactly the events after its cursor"
    );
    f.assert_no_leak("the replay", &serde_json::to_string(&replayed).unwrap());
    let foreign = EventCursor {
        core_epoch: InstanceEpoch::from(cursor.core_epoch.get() + 1),
        sequence: Sequence::from(1),
    };
    for after in [Some(foreign), None] {
        let snapshot = projection(&f)
            .subscribe_with_snapshot(&Subscribe {
                scope: SubscriptionScope::All,
                after,
            })
            .unwrap();
        assert!(
            snapshot.started.snapshot_id.is_some(),
            "{after:?} needs a snapshot"
        );
        f.assert_no_leak(
            "a snapshot",
            &serde_json::to_string(&snapshot.started).unwrap(),
        );
    }

    for (session, p) in [(session_a, &a), (session_b, &b)] {
        let id = f
            .command(BrowserCommand::CloseRendererSession { session }, p)
            .unwrap();
        f.output(&id, p).await;
    }
    drop(f.events);
    drop(_connected);
    f.core.shutdown().await.unwrap();
}
