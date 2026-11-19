//! Real CEF through the production core owner, with test-only subreaper checks.
#![cfg(all(target_os = "linux", feature = "renderer-test-helper"))]
#[path = "renderer_support/data_peer.rs"]
mod data_peer;
#[path = "renderer_support/mod.rs"]
mod support;
use renderer_ipc::{OpenArtifact, SessionId, TabId, Viewport};
use workbench_host::areas::{
    AreaTasks,
    browser::renderer::{OwnerConfig, RendererOwner},
};
fn executable() -> std::path::PathBuf {
    let exe = std::env::var_os("EVORCH_RENDERER_EXE")
        .map(std::path::PathBuf::from)
        .expect("cef-e2e must supply EVORCH_RENDERER_EXE");
    assert!(exe.is_absolute() && exe.is_file());
    assert!(exe.with_file_name("evorch-cef-helper").is_file());
    exe
}
#[tokio::test]
#[ignore = "requires actual CEF; mandatory cef-e2e supplies EVORCH_RENDERER_EXE"]
async fn real_owner_reclaims_graceful_and_forced_groups_without_touching_unrelated_child() {
    // Baseline child stays alive across both renderer generations.
    let mut unrelated = tokio::process::Command::new("sh")
        .args(["-c", "read line"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut tree = support::Tree::new();
    let root = tempfile::tempdir().unwrap();
    let tasks = AreaTasks::new(tokio::runtime::Handle::current());
    let ledger = sandbox::SpawnLedger::new();
    let owner = RendererOwner::new(
        &tasks,
        ledger.clone(),
        OwnerConfig::new(executable(), root.path().join("profiles")),
    )
    .unwrap();
    for graceful in [true, false] {
        let (open, body) = OpenArtifact::with_files(
            SessionId(1),
            TabId(1),
            Viewport {
                width: 1024,
                height: 768,
            },
            30,
            30_000,
            vec![(
                "index.html".into(),
                "text/html".into(),
                b"<canvas>renderer owner</canvas>".to_vec(),
            )],
        );
        let tab = owner.open(open, body).await.unwrap();
        tree.track(&ledger);
        owner.set_frame_rate(tab, 10).await.unwrap();
        owner.set_streaming(tab, false).await.unwrap();
        owner.set_streaming(tab, true).await.unwrap();
        if graceful {
            owner.close(tab).await.unwrap();
            owner.stop().await.unwrap();
        } else {
            ledger.stop_all_children();
            let mut view = owner.subscribe();
            view.wait_for(|s| s.phase == workbench_host::areas::browser::renderer::Phase::Failed)
                .await
                .unwrap();
        }
        assert!(ledger.children().is_empty());
        assert!(unrelated.try_wait().unwrap().is_none());
    }
    ledger.close_fence();
    tasks.request_stop();
    tasks.shutdown().await.unwrap();
    ledger.reap_all_children().await.unwrap();
    ledger.join_workers().await.unwrap();
    tree.assert_clean();
    drop(unrelated.stdin.take());
    unrelated.wait().await.unwrap();
    assert_eq!(
        std::fs::read_dir(root.path().join("profiles"))
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test]
#[ignore = "requires actual CEF; mandatory cef-e2e supplies EVORCH_RENDERER_EXE"]
async fn abrupt_real_core_exit_ends_helper_by_eof_and_collects_the_old_profile() {
    let dir = tempfile::tempdir().unwrap();
    support::core_eof(executable(), &dir.path().join("profiles")).await;
}

#[tokio::test]
#[ignore = "requires actual CEF; mandatory cef-e2e supplies EVORCH_RENDERER_EXE"]
async fn inspect_artifact_pages_open_through_the_core_owner_with_the_owned_record() {
    use runtime::artifacts::{ArtifactPages as _, ArtifactRecord};
    use workbench_host::areas::browser::run_pages::RuntimePages;
    let root = tempfile::tempdir().unwrap();
    let tasks = AreaTasks::new(tokio::runtime::Handle::current());
    let ledger = sandbox::SpawnLedger::new();
    let owner = RendererOwner::new(
        &tasks,
        ledger,
        OwnerConfig::new(executable(), root.path().join("profiles")),
    )
    .unwrap();
    let pages = RuntimePages::new(Some(owner.clone()));
    let record = ArtifactRecord {
        artifact_id: "artifact-1".into(),
        sha256: "0".repeat(64),
        media_type: "text/html".into(),
        extension: "html".into(),
        title: "page".into(),
        caption: None,
        source_path: "/unused".into(),
        byte_len: 0,
        run_id: "run-1".into(),
        root_run_id: "run-1".into(),
        thread_id: Some("thread-1".into()),
        inlined_assets: Vec::new(),
    };
    let page = pages
        .open_artifact("run-1", b"<button>inspect me</button>".to_vec(), &record)
        .await
        .unwrap();
    assert!(pages.page("run-1").is_some());
    let png = page.agent().screenshot().await.unwrap();
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    // IHDR: the screenshot is the 1024x768 viewport.
    assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), 1024);
    assert_eq!(u32::from_be_bytes(png[20..24].try_into().unwrap()), 768);
    assert!(pages.close("run-1"));
    owner.stop().await.unwrap();
}

/// Drives the model through the real `inspect_artifact` tool of a bootstrapped
/// core, so the provider the core injects is what reaches real CEF.
struct InspectScript {
    step: std::sync::Mutex<usize>,
    results: std::sync::Arc<std::sync::Mutex<Vec<(bool, String)>>>,
}
fn tool_texts(messages: &[providers::Message]) -> Vec<(bool, String)> {
    use providers::{ContentBlock, ToolResultContent};
    messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                is_error, content, ..
            } => Some((
                *is_error,
                content
                    .iter()
                    .map(|part| match part {
                        ToolResultContent::Text { text } => text.clone(),
                        ToolResultContent::Image { media_type, .. } => {
                            format!("[image {media_type}]")
                        }
                    })
                    .collect(),
            )),
            _ => None,
        })
        .collect()
}
#[async_trait::async_trait]
impl runtime::AgentModel for InspectScript {
    fn selected_model(&self, _: runtime::Role, _: Option<&str>) -> String {
        "inspect-script".into()
    }
    async fn complete(
        &self,
        _: &runtime::AgentInvocationContext,
        _: runtime::Role,
        messages: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        use serde_json::json;
        let results = tool_texts(messages);
        *self.results.lock().unwrap() = results.clone();
        let latest = results.last().map(|(_, text)| text.clone());
        let mut step = self.step.lock().unwrap();
        let call = match *step {
            0 => Some((
                "render_artifact",
                json!({"path": "page.html", "title": "page"}),
            )),
            1 => {
                let id = serde_json::from_str::<serde_json::Value>(&latest.unwrap()).unwrap()
                    ["artifact_id"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                Some((
                    "inspect_artifact",
                    json!({"action": "open", "artifact_id": id}),
                ))
            }
            2 => Some(("inspect_artifact", json!({"action": "snapshot"}))),
            3 => {
                let outline = latest.unwrap();
                let at = outline.find("[ref=").expect("an outline ref") + "[ref=".len();
                let reference = outline[at..].split(']').next().unwrap().to_owned();
                Some((
                    "inspect_artifact",
                    json!({"action": "click", "ref": reference}),
                ))
            }
            4 => Some(("inspect_artifact", json!({"action": "screenshot"}))),
            5 => Some(("inspect_artifact", json!({"action": "close"}))),
            _ => None,
        };
        *step += 1;
        let (content, finish_reason) = match call {
            Some((name, input)) => (
                vec![providers::ContentBlock::ToolUse {
                    id: format!("call-{}", *step),
                    name: name.into(),
                    input,
                }],
                providers::FinishReason::ToolUse,
            ),
            None => (
                vec![providers::ContentBlock::Text {
                    text: "inspected".into(),
                }],
                providers::FinishReason::Stop,
            ),
        };
        Ok(providers::ChatResponse {
            usage_reported: true,
            message: providers::Message {
                role: providers::Role::Assistant,
                content,
            },
            finish_reason,
            usage: providers::Usage::default(),
        })
    }
}
struct Unisolated;
impl runtime::SandboxFactory for Unisolated {
    fn build(
        &self,
        _: &runtime::ExecutionPolicy,
        _: &runtime::WorkspaceMounts,
    ) -> Result<std::sync::Arc<dyn sandbox::Sandbox>, sandbox::SandboxError> {
        Ok(std::sync::Arc::new(sandbox::DirectSandbox::new_unchecked()))
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires actual CEF; mandatory cef-e2e supplies EVORCH_RENDERER_EXE"]
async fn the_core_inspect_artifact_tool_runs_open_snapshot_click_screenshot_close_in_real_cef() {
    use event_bus::{AgentRunPhase, EventKind, LifecycleEvent};
    use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams};
    use workbench_protocol::commands::{Command, ComposerRole, SendChat};
    use workbench_protocol::ids::{ProjectId, SubmissionId, ThreadId};
    use workspace_state::{
        ProjectId as StateProject, SidebarDocument, SidebarStore, ThreadId as StateThread,
        ThreadRecord,
    };

    let dir = test_socket_dir::SplitDir::new();
    let root = dir.path().canonicalize().unwrap();
    let project = root.join("repo");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("page.html"),
        "<!doctype html><button>Add</button>",
    )
    .unwrap();
    let store = SidebarStore::new(root.join("sidebar.json"));
    let mut document = SidebarDocument::default();
    document
        .sidebar
        .add_project(StateProject::new("p"), "repo", &project)
        .unwrap();
    document.sidebar.threads.push(ThreadRecord::new(
        StateThread::new("t1"),
        StateProject::new("p"),
        "first",
    ));
    document.sidebar.refresh_derived();
    {
        let lock = store.lock().unwrap();
        store.write(&lock, &document).unwrap();
    }
    // The core resolves its helper from this variable at bootstrap; the only
    // value the process ever sets is the real renderer.
    // SAFETY: set once, before the core and its threads exist.
    unsafe { std::env::set_var("EVORCH_RENDERER_EXEC", executable()) };
    let results = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut core = BootstrappedHost::open_with_test_seams(
        BootstrapOptions {
            config_dir: root.clone(),
            project_root: project.clone(),
            ownership_root: dir.sockets().into(),
        },
        TestSeams {
            model: std::sync::Arc::new(InspectScript {
                step: std::sync::Mutex::new(0),
                results: results.clone(),
            }),
            sandbox: std::sync::Arc::new(Unisolated),
        },
    )
    .await
    .unwrap();
    let mut events = core.runtime.event_bus().subscribe();
    let send = SendChat {
        project: ProjectId::new("p").unwrap(),
        thread: ThreadId::new("t1").unwrap(),
        submission_id: SubmissionId::new("00000000-0000-4000-8000-000000000009").unwrap(),
        composer_role: ComposerRole::Worker,
        text: "Inspect the page".into(),
        skills: vec![],
        attachments: vec![],
        model_preference: None,
        fork_seed: None,
    };
    let run: runtime::RunId = {
        let mut host = core.host.lock().unwrap();
        host.reserve_new_chat_thread(&send).unwrap();
        host.handle_command(
            Command::Conversation(
                workbench_protocol::namespaces::ConversationCommand::SendChat(send),
            ),
            workbench_host::provenance::local_desktop(),
        )
        .unwrap()
        .run_id
        .unwrap()
        .as_str()
        .parse()
        .unwrap()
    };
    loop {
        let event = events.recv().await.unwrap();
        if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to, .. }) =
            &event.kind
            && run_id == &run.to_string()
        {
            assert!(
                !matches!(to, AgentRunPhase::Error | AgentRunPhase::Stopped),
                "{event:?}"
            );
            if *to == AgentRunPhase::Waiting {
                break;
            }
        }
    }
    // The model's last request carries every tool result of the script.
    let results = results.lock().unwrap().clone();
    assert_eq!(results.len(), 6, "{results:?}");
    for (index, (is_error, text)) in results.iter().enumerate() {
        assert!(!is_error, "tool call {index} failed: {text}");
    }
    assert!(results[2].1.contains("Add"), "{}", results[2].1);
    // The click returned the refreshed outline (artifact pages run no inline script).
    assert!(results[3].1.contains("button \"Add\""), "{}", results[3].1);
    assert!(results[4].1.contains("1024x768"), "{}", results[4].1);
    assert!(results[5].1.contains("closed"), "{}", results[5].1);
    core.runtime
        .stop(run, runtime::StopScope::SelfOnly)
        .unwrap();
    core.runtime.wait(run).await.unwrap();
    core.shutdown().await.unwrap();
}

/// The client library, the core's Unix socket, the host and the real helper
/// with real CEF behind it: frames, a DOM read, a Capture PNG and an image
/// artifact each reach their client on their own data connection, several
/// connections at once, with one that never reads (plan §8, F3).
mod data_plane {
    use super::{Unisolated, data_peer::Peer, data_peer::refusal, executable};
    use std::{sync::Arc, time::Duration};

    use evorch_core::{backend::HostBackend, unix};
    use runtime::artifacts::{CaptureRequest, store_with_refs};
    use workbench_client::renderer::FrameStream;
    use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams};
    use workbench_protocol::{
        errors::ErrorCode,
        ids::{ProjectId, ThreadId},
        namespaces::{
            artifacts::ArtifactRef,
            browser::data::{BlobSource, MediaType},
            browser::*,
        },
    };
    use workspace_state::{
        ProjectId as StateProject, SidebarDocument, SidebarStore, ThreadId as StateThread,
        ThreadRecord,
    };

    /// A real 1x1 PNG, stored as an image artifact.
    const TINY_PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];
    /// Only a stuck CEF may reach this; every wait below ends on an event.
    const GUARD: Duration = Duration::from_secs(90);

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
            "renderer-data-plane-cef".into()
        }
    }

    /// Frames until one of the wanted size arrives (a size change repaints).
    async fn frame_of(frames: &mut FrameStream, width: u32, height: u32) -> Vec<u8> {
        tokio::time::timeout(GUARD, async {
            loop {
                let frame = frames.next_frame().await.unwrap().expect("a frame");
                if (frame.start.width, frame.start.height) == (width, height) {
                    assert_eq!(frame.start.stride, width * 4);
                    assert_eq!(frame.bgra.len() as u32, width * height * 4);
                    return frame.bgra;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no {width}x{height} frame arrived"))
    }

    /// The page paints its whole viewport green, so a real paint is told from
    /// the blank frame before it by the centre pixel (BGRA).
    async fn painted_frame(frames: &mut FrameStream, width: u32, height: u32) {
        tokio::time::timeout(GUARD, async {
            loop {
                let bgra = frame_of(frames, width, height).await;
                let at = (((height / 2) * width + width / 2) * 4) as usize;
                if bgra[at] < 40 && bgra[at + 1] > 200 && bgra[at + 2] < 40 {
                    return;
                }
            }
        })
        .await
        .expect("the green page was never painted")
    }

    #[test]
    #[ignore = "requires actual CEF; mandatory cef-e2e supplies EVORCH_RENDERER_EXE"]
    fn frames_dom_capture_and_images_reach_clients_through_the_core_and_real_cef() {
        let dir = test_socket_dir::SplitDir::new();
        let root = dir.path().canonicalize().unwrap();
        // SAFETY: single-threaded here, and the runtime that reads it is built
        // below; the only value this process sets is the real renderer.
        unsafe { std::env::set_var("EVORCH_RENDERER_EXEC", executable()) };
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(run(dir, root));
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
        let mut core = BootstrappedHost::open_with_test_seams(
            BootstrapOptions {
                config_dir: root.clone(),
                project_root: project_root.clone(),
                ownership_root: dir.sockets().into(),
            },
            TestSeams {
                model: Arc::new(Idle),
                sandbox: Arc::new(Unisolated),
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
        std::fs::write(
            project_root.join("page.html"),
            "<!doctype html><body style=\"background:#00ff00\"><button>Add</button></body>",
        )
        .unwrap();
        std::fs::write(project_root.join("tiny.png"), TINY_PNG).unwrap();
        let artifacts = store_with_refs(root.join("artifacts"), writer).unwrap();
        let capture = |file: &str| {
            let record = artifacts
                .capture(
                    &project_root,
                    None,
                    &project_root.join(file),
                    CaptureRequest {
                        title: file,
                        caption: None,
                        run_id: "r1".into(),
                        root_run_id: "r1".into(),
                        thread_id: Some("t1".into()),
                    },
                )
                .unwrap();
            (
                ArtifactRef {
                    project: ProjectId::new("p1").unwrap(),
                    thread: ThreadId::new("t1").unwrap(),
                    artifact_id: record.artifact_id.clone(),
                    digest: record.sha256.clone(),
                },
                record,
            )
        };
        let (page, _) = capture("page.html");
        let (image, image_record) = capture("tiny.png");

        let socket = dir.sockets().join("renderer-data-plane-cef.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let backend = Arc::new(HostBackend::new(core.host.clone(), "data-plane-cef".into()));
        let (stop, stopped) = tokio::sync::watch::channel(false);
        let server = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (backend, stopped) = (backend.clone(), stopped.clone());
                tokio::spawn(async move {
                    let _ = unix::connection(stream, backend, stopped).await;
                });
            }
        });

        let mut a = Peer::connect(&socket, "client-a").await;
        let b = Peer::connect(&socket, "client-b").await;
        let session = a.session().await;
        let tab = a.open(&page, &session).await;

        // Three subscriptions of one tab: two readers and one that never reads.
        let mut tokens = Vec::new();
        for _ in 0..3 {
            let op = a
                .browser(BrowserCommand::SubscribeRendererFrames {
                    tab: tab.reference.clone(),
                })
                .await
                .unwrap();
            let RendererOutput::Subscription {
                subscription,
                token,
                ..
            } = a.output(&op).await
            else {
                panic!("not a subscription")
            };
            tokens.push((subscription, token));
        }
        // Mixed agent/renderer occupancy must use the SAME eight frame slots,
        // while the real CEF streams below still deliver unchanged BGRA frames.
        let agent_occupants = {
            use workbench_host::{
                areas::browser::observation::{
                    ObservedPage, fake::FakeObservationPort, service::ObservationService,
                },
                connections::ConnectionHooks,
            };
            use workbench_protocol::{
                envelope::{Provenance, Surface},
                ids::{ClientId, RunId},
                namespaces::browser::observation::*,
            };
            let scope = ObservationScope {
                project: ProjectId::new("p1").unwrap(),
                thread: ThreadId::new("t1").unwrap(),
                run: RunId::new("r1").unwrap(),
                incarnation: RunId::new("test-incarnation").unwrap(),
                core_epoch: 1,
                browser_generation: 1,
                page_generation: 1,
            };
            let port = Arc::new(FakeObservationPort::default());
            assert!(port.publish(ObservedPage {
                snapshot: ObservationSnapshot {
                    scope: scope.clone(),
                    revision: 1,
                    state: ObservationState::Starting,
                    tabs: vec![],
                    active_tab: None,
                    activity: vec![]
                },
                frame: None
            }));
            let hooks = Arc::new(ConnectionHooks::default());
            let client = ClientId::new("budget-test").unwrap();
            let connection = hooks.open(client.clone());
            let p = Provenance::human(Surface::Desktop, client);
            let plane = core.host.lock().unwrap().renderer_data_plane().unwrap();
            let service = ObservationService::new(Some(port), plane, hooks, 1);
            for _ in 0..5 {
                assert!(matches!(
                    service
                        .command(
                            BrowserCommand::SubscribeAgentFrames {
                                scope: scope.clone()
                            },
                            &p
                        )
                        .unwrap(),
                    ObservationResult::Subscribed { .. }
                ));
            }
            assert_eq!(
                service
                    .command(BrowserCommand::SubscribeAgentFrames { scope }, &p)
                    .unwrap(),
                ObservationResult::State(ObservationState::Capacity)
            );
            (service, connection)
        };
        // Another client cannot use what A was given.
        assert_eq!(
            refusal(
                b.channel()
                    .await
                    .bind_frames(&b.client, tokens[0].0.clone(), tokens[0].1.clone())
                    .await
            ),
            ErrorCode::PermissionDenied
        );
        let mut bound = Vec::new();
        for (subscription, token) in &tokens {
            bound.push(
                a.channel()
                    .await
                    .bind_frames(&a.client, subscription.clone(), token.clone())
                    .await
                    .unwrap(),
            );
        }
        let stalled = bound.pop().unwrap();
        let mut second = bound.pop().unwrap();
        let mut first = bound.pop().unwrap();
        a.browser(BrowserCommand::SetRendererStreaming {
            tab: tab.reference.clone(),
            enabled: true,
        })
        .await
        .unwrap();
        // Real pixels of the real page, on two connections at once.
        painted_frame(&mut first, 1024, 768).await;
        painted_frame(&mut second, 1024, 768).await;

        // The DOM, through the control channel and the helper.
        let cdp = a
            .browser(BrowserCommand::RendererCdp {
                tab: tab.reference.clone(),
                method: "DOM.getDocument".into(),
                params: serde_json::json!({ "depth": -1 }),
            })
            .await
            .unwrap();
        let RendererOutput::Cdp { result } = a.output(&cdp).await else {
            panic!("not a CDP result")
        };
        assert!(result.to_string().contains("BUTTON"), "{result}");

        // Capture: a PNG of the 1024x768 viewport, on a data connection.
        let op = a
            .browser(BrowserCommand::CaptureRendererTab {
                tab: tab.reference.clone(),
            })
            .await
            .unwrap();
        let RendererOutput::Capture {
            capture_id,
            byte_len,
        } = a.output(&op).await
        else {
            panic!("not a capture")
        };
        assert_eq!(
            refusal(
                b.channel()
                    .await
                    .bind_capture(&b.client, op.clone(), capture_id.clone())
                    .await
            ),
            ErrorCode::PermissionDenied
        );
        let blob = a
            .channel()
            .await
            .bind_capture(&a.client, op.clone(), capture_id)
            .await
            .unwrap();
        assert_eq!(blob.start().source, BlobSource::Capture);
        assert_eq!(blob.start().media_type, MediaType::Png);
        assert_eq!(blob.start().total_bytes, byte_len);
        let png = blob.read_to_end(64 << 20).await.unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), 1024);
        assert_eq!(u32::from_be_bytes(png[20..24].try_into().unwrap()), 768);

        // An image artifact: its original bytes and stored digest.
        let blob = a
            .channel()
            .await
            .bind_artifact(&a.client, image.clone())
            .await
            .unwrap();
        let digest: String = blob
            .start()
            .digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(digest, image_record.sha256);
        assert_eq!(blob.read_to_end(1 << 20).await.unwrap(), TINY_PNG);

        // The third connection holds frames it never reads. Each resize makes
        // the helper paint again, and both readers still get every new size.
        for (width, height) in [(800, 600), (1024, 768), (640, 480), (1280, 720)] {
            a.browser(BrowserCommand::ResizeRendererTab {
                tab: tab.reference.clone(),
                viewport: RendererViewport { width, height },
            })
            .await
            .unwrap();
            frame_of(&mut first, width, height).await;
            frame_of(&mut second, width, height).await;
        }

        let close = a
            .browser(BrowserCommand::CloseRendererTab {
                tab: tab.reference.clone(),
            })
            .await
            .unwrap();
        a.output(&close).await;
        let close = a
            .browser(BrowserCommand::CloseRendererSession { session })
            .await
            .unwrap();
        a.output(&close).await;
        drop((a, b, first, second, stalled, agent_occupants));
        let _ = stop.send(true);
        server.abort();
        core.shutdown().await.unwrap();
    }
}
