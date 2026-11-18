//! The renderer data plane (plan §8, F3) over the core's real Unix socket and
//! the real helper protocol: raw frames, a Capture PNG and the original bytes
//! of an image artifact travel on their own connections, and only the control
//! command of the owning client can bind one. Two clients, A and B, connect
//! through the production `unix::connection`; every wait is an event.
#![cfg(all(target_os = "linux", feature = "renderer-test-helper"))]

use std::sync::Arc;

use evorch_core::{backend::HostBackend, unix};
use runtime::artifacts::{CaptureRequest, store_with_refs};
use workbench_client::renderer::DataClientError;
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams};
use workbench_protocol::{
    errors::ErrorCode,
    ids::{ProjectId, ThreadId},
    namespaces::{
        artifacts::ArtifactRef,
        browser::data::{self, BlobSource, MediaType},
        browser::*,
    },
};
use workspace_state::{
    ProjectId as StateProject, SidebarDocument, SidebarStore, ThreadId as StateThread, ThreadRecord,
};

/// What the helper's `capture` returns.
const FAKE_PIXELS: &[u8] = b"\x89PNG\r\n\x1a\nFAKE-PIXELS";
/// A real 1x1 PNG, stored as an image artifact.
const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae,
    0x42, 0x60, 0x82,
];

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
        "renderer-data-plane-uds".into()
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

#[path = "renderer_support/data_peer.rs"]
mod data_peer;
use data_peer::{Peer, refusal};

#[test]
fn frames_captures_and_images_travel_on_data_connections_that_only_their_owner_can_bind() {
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

async fn run(dir: test_socket_dir::SplitDir, root: std::path::PathBuf) {
    let project_root = root.join("one");
    let other_root = root.join("two");
    let store = SidebarStore::new(root.join("sidebar.json"));
    let mut document = SidebarDocument::default();
    for (project, path, thread) in [("p1", &project_root, "t1"), ("p2", &other_root, "t2")] {
        std::fs::create_dir_all(path).unwrap();
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
    let mut core = BootstrappedHost::open_with_test_seams(
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
    for (project, path, thread, run) in [
        ("p1", &project_root, "t1", "r1"),
        ("p2", &other_root, "t2", "r2"),
    ] {
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
    std::fs::write(project_root.join("page.html"), "<p>one</p>").unwrap();
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
        let reference = ArtifactRef {
            project: ProjectId::new("p1").unwrap(),
            thread: ThreadId::new("t1").unwrap(),
            artifact_id: record.artifact_id.clone(),
            digest: record.sha256.clone(),
        };
        (reference, record)
    };
    let (page, _) = capture("page.html");
    let (image, image_record) = capture("tiny.png");
    assert_eq!(image_record.media_type, "image/png");

    let socket = dir.sockets().join("renderer-data-plane.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let backend = Arc::new(HostBackend::new(core.host.clone(), "data-plane".into()));
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
    let session_a = a.session().await;
    let tab = a.open(&page, &session_a).await;

    // Frames: the subscription's owner binds a data connection with the
    // token it was given, and reads raw BGRA frames from it.
    let subscribe = a
        .browser(BrowserCommand::SubscribeRendererFrames {
            tab: tab.reference.clone(),
        })
        .await
        .unwrap();
    let RendererOutput::Subscription {
        subscription,
        token,
        ..
    } = a.output(&subscribe).await
    else {
        panic!("not a subscription")
    };
    // At most eight subscriptions are live at once; the ninth is refused
    // until one is released, and a refusal leaves the others untouched.
    for _ in 1..data::MAX_FRAME_SUBSCRIPTIONS {
        a.browser(BrowserCommand::SubscribeRendererFrames {
            tab: tab.reference.clone(),
        })
        .await
        .unwrap();
    }
    assert_eq!(
        a.browser(BrowserCommand::SubscribeRendererFrames {
            tab: tab.reference.clone(),
        })
        .await
        .unwrap_err(),
        ErrorCode::Busy
    );

    // Another client cannot use what A was given, even knowing the token;
    // a refused binding does not consume it.
    assert_eq!(
        refusal(
            b.channel()
                .await
                .bind_frames(&b.client, subscription.clone(), token.clone())
                .await
        ),
        ErrorCode::PermissionDenied
    );

    let mut frames = a
        .channel()
        .await
        .bind_frames(&a.client, subscription.clone(), token.clone())
        .await
        .unwrap();
    a.browser(BrowserCommand::SetRendererStreaming {
        tab: tab.reference.clone(),
        enabled: true,
    })
    .await
    .unwrap();
    let first = frames.next_frame().await.unwrap().expect("a frame");
    assert_eq!((first.start.width, first.start.height), (4, 2));
    assert_eq!(first.start.stride, 16);
    assert_eq!(first.bgra.len(), 32);
    assert!(
        first.bgra.iter().all(|byte| *byte == first.bgra[0]) && first.bgra[0] != 0,
        "the pixels are the helper's, unchanged"
    );
    // The token is consumed by the first binding.
    assert_eq!(
        refusal(
            a.channel()
                .await
                .bind_frames(&a.client, subscription.clone(), token.clone())
                .await
        ),
        ErrorCode::PermissionDenied
    );
    // A resize makes the helper paint again: a newer frame arrives.
    a.browser(BrowserCommand::ResizeRendererTab {
        tab: tab.reference.clone(),
        viewport: RendererViewport {
            width: 800,
            height: 600,
        },
    })
    .await
    .unwrap();
    let second = frames.next_frame().await.unwrap().expect("a newer frame");
    assert!(second.start.sequence > first.start.sequence);
    assert_ne!(second.bgra, first.bgra);

    // Capture: the PNG travels on a data connection of the same client.
    let capture_op = a
        .browser(BrowserCommand::CaptureRendererTab {
            tab: tab.reference.clone(),
        })
        .await
        .unwrap();
    let RendererOutput::Capture {
        capture_id,
        byte_len,
    } = a.output(&capture_op).await
    else {
        panic!("not a capture")
    };
    assert_eq!(byte_len, FAKE_PIXELS.len() as u64);
    assert_eq!(
        refusal(
            b.channel()
                .await
                .bind_capture(&b.client, capture_op.clone(), capture_id.clone())
                .await
        ),
        ErrorCode::PermissionDenied
    );
    let png = a
        .channel()
        .await
        .bind_capture(&a.client, capture_op.clone(), capture_id.clone())
        .await
        .unwrap();
    assert_eq!(png.start().source, BlobSource::Capture);
    assert_eq!(png.start().media_type, MediaType::Png);
    assert_eq!(png.read_to_end(1 << 20).await.unwrap(), FAKE_PIXELS);

    // An image artifact: the original bytes and the stored digest.
    let blob = a
        .channel()
        .await
        .bind_artifact(&a.client, image.clone())
        .await
        .unwrap();
    assert_eq!(blob.start().source, BlobSource::Artifact);
    let digest: String = blob
        .start()
        .digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(digest, image_record.sha256);
    assert_eq!(blob.read_to_end(1 << 20).await.unwrap(), TINY_PNG);
    // An artifact that is not an image has no data-connection form, and a
    // reference naming another thread is verified against the stored one.
    assert_eq!(
        refusal(
            a.channel()
                .await
                .bind_artifact(&a.client, page.clone())
                .await
        ),
        ErrorCode::InvalidRequest
    );
    let mut claimed = image.clone();
    claimed.project = ProjectId::new("p2").unwrap();
    claimed.thread = ThreadId::new("t2").unwrap();
    assert!(
        a.channel()
            .await
            .bind_artifact(&a.client, claimed)
            .await
            .is_err()
    );

    // Closing the tab ends the frame stream: the core says why, then closes.
    let close = a
        .browser(BrowserCommand::CloseRendererTab {
            tab: tab.reference.clone(),
        })
        .await
        .unwrap();
    a.output(&close).await;
    loop {
        match frames.next_frame().await {
            Ok(Some(_)) => continue,
            Ok(None) | Err(DataClientError::Remote(_)) => break,
            Err(other) => panic!("unexpected end of the frames: {other:?}"),
        }
    }

    let close = a
        .browser(BrowserCommand::CloseRendererSession { session: session_a })
        .await
        .unwrap();
    a.output(&close).await;
    drop((a, b));
    let _ = stop.send(true);
    server.abort();
    core.shutdown().await.unwrap();
}
