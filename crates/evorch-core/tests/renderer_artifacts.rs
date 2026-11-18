//! F2's artifact read API through the production composition: the core
//! authorizes every artifact by the thread and project it is stored under, never
//! by what the client states, and the Web surface reads metadata only.
//!
//! The core is bootstrapped with a real store, sidebar and storage index; the
//! helper is never started. Everything is driven through `CommandHost`, so no
//! test waits for time.
use std::sync::Arc;

use runtime::artifacts::{ArtifactRecord, CaptureRequest, store_with_refs};
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams};
use workbench_protocol::{
    envelope::{Provenance, Surface},
    errors::ErrorCode,
    ids::{ClientId, ProjectId, SessionId, ThreadId},
    namespaces::{
        artifacts::{ArtifactRef, ArtifactsQuery, ArtifactsReply},
        browser::{BrowserQuery, RendererId},
    },
    queries::{Query, QueryReply},
};
use workspace_state::{
    ProjectId as StateProject, SidebarDocument, SidebarStore, ThreadId as StateThread, ThreadRecord,
};

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR-test-image";
const INLINE: &str = "data:image/png;base64,QUJD";

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
        "artifact-api".into()
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

fn provenance(surface: Surface) -> Provenance {
    Provenance::human(surface, ClientId::new("client-1").unwrap())
}
fn reference(project: &str, thread: &str, record: &ArtifactRecord) -> ArtifactRef {
    ArtifactRef {
        project: ProjectId::new(project).unwrap(),
        thread: ThreadId::new(thread).unwrap(),
        artifact_id: record.artifact_id.clone(),
        digest: record.sha256.clone(),
    }
}

struct World {
    core: BootstrappedHost,
    _dir: test_socket_dir::SplitDir,
    html: ArtifactRecord,
    second: ArtifactRecord,
    image: ArtifactRecord,
    html_text: String,
}
impl World {
    async fn new() -> Self {
        let dir = test_socket_dir::SplitDir::new();
        let root = dir.path().canonicalize().unwrap();
        let (one, two) = (root.join("one"), root.join("two"));
        for path in [&one, &two] {
            std::fs::create_dir_all(path).unwrap();
        }
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
        // Multi-byte text and an inline data URI must come back byte for byte.
        let html_text = format!("<p>日本語の本文 é</p><img src=\"{INLINE}\">");
        std::fs::write(one.join("page.html"), &html_text).unwrap();
        std::fs::write(one.join("note.html"), "plain <b>text</b>").unwrap();
        std::fs::write(one.join("shot.png"), PNG).unwrap();
        let artifacts = store_with_refs(root.join("artifacts"), writer).unwrap();
        let capture = |file: &str| {
            artifacts
                .capture(
                    &one,
                    None,
                    &one.join(file),
                    CaptureRequest {
                        title: file,
                        caption: None,
                        run_id: "r1".into(),
                        root_run_id: "r1".into(),
                        thread_id: Some("t1".into()),
                    },
                )
                .unwrap()
        };
        let (html, second, image) = (
            capture("page.html"),
            capture("note.html"),
            capture("shot.png"),
        );
        Self {
            core,
            _dir: dir,
            html,
            second,
            image,
            html_text,
        }
    }
    fn query(&self, query: Query, provenance: &Provenance) -> Result<QueryReply, ErrorCode> {
        self.core
            .host
            .lock()
            .unwrap()
            .handle_query(&SessionId::new("session").unwrap(), &query, provenance)
            .map_err(|error| error.code)
    }
    fn artifacts(
        &self,
        query: ArtifactsQuery,
        provenance: &Provenance,
    ) -> Result<ArtifactsReply, ErrorCode> {
        match self.query(Query::Artifacts(query), provenance)? {
            QueryReply::Artifacts(reply) => Ok(reply),
            other => panic!("unexpected reply: {other:?}"),
        }
    }
    fn read(
        &self,
        artifact: &ArtifactRef,
        offset: u64,
        max_bytes: u32,
        provenance: &Provenance,
    ) -> Result<(String, bool), ErrorCode> {
        match self.artifacts(
            ArtifactsQuery::ReadContent {
                artifact: artifact.clone(),
                offset,
                max_bytes,
            },
            provenance,
        )? {
            ArtifactsReply::Content { text, eof, .. } => Ok((text, eof)),
            other => panic!("unexpected reply: {other:?}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn content_is_authorized_by_the_stored_thread_and_project_and_chunks_on_characters() {
    let mut world = World::new().await;
    let desktop = provenance(Surface::Desktop);
    let good = reference("p1", "t1", &world.html);

    // The whole body comes back unmodified, inline data URI included, in chunks
    // that never split a character.
    let mut offset = 0;
    let mut body = String::new();
    loop {
        let (chunk, eof) = world.read(&good, offset, 7, &desktop).unwrap();
        assert!(!chunk.is_empty());
        offset += chunk.len() as u64;
        body.push_str(&chunk);
        if eof {
            break;
        }
    }
    assert_eq!(body, world.html_text);
    assert!(body.contains(INLINE));
    assert_eq!(
        world
            .read(&reference("p1", "t1", &world.second), 0, 4096, &desktop)
            .unwrap(),
        ("plain <b>text</b>".to_owned(), true)
    );
    // Invalid ranges are refused.
    for (offset, max) in [(0, 0), (world.html.byte_len + 1, 8), (0, u32::MAX)] {
        assert_eq!(
            world.read(&good, offset, max, &desktop).unwrap_err(),
            ErrorCode::InvalidRequest
        );
    }
    // An image body is not served here (F3 owns the binary path).
    assert_eq!(
        world
            .read(&reference("p1", "t1", &world.image), 0, 64, &desktop)
            .unwrap_err(),
        ErrorCode::InvalidRequest
    );

    // What the client claims never replaces what is stored: another project's
    // thread, another thread of the right project, an unknown project and a wrong
    // digest all look like a missing artifact.
    let mut wrong_digest = good.clone();
    wrong_digest.digest = world.second.sha256.clone();
    for claim in [
        reference("p2", "t2", &world.html),
        reference("p1", "t2", &world.html),
        reference("p2", "t1", &world.html),
        reference("unknown", "t1", &world.html),
        wrong_digest,
    ] {
        assert_eq!(
            world.read(&claim, 0, 64, &desktop).unwrap_err(),
            ErrorCode::NotFound,
            "{claim:?}"
        );
        assert_eq!(
            world
                .artifacts(
                    ArtifactsQuery::Get {
                        artifact: claim.clone()
                    },
                    &desktop
                )
                .unwrap_err(),
            ErrorCode::NotFound
        );
    }
    // Listing only returns what belongs to the thread asked about.
    let ArtifactsReply::List { artifacts, .. } = world
        .artifacts(
            ArtifactsQuery::List {
                project: ProjectId::new("p1").unwrap(),
                thread: ThreadId::new("t1").unwrap(),
                after: None,
                limit: 10,
            },
            &desktop,
        )
        .unwrap()
    else {
        panic!("not a list")
    };
    assert_eq!(artifacts.len(), 3);
    assert_eq!(
        world
            .artifacts(
                ArtifactsQuery::List {
                    project: ProjectId::new("p2").unwrap(),
                    thread: ThreadId::new("t1").unwrap(),
                    after: None,
                    limit: 10,
                },
                &desktop,
            )
            .unwrap_err(),
        ErrorCode::NotFound
    );
    world.core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_web_surface_reads_authorized_metadata_but_never_content_or_renderer_queries() {
    let mut world = World::new().await;
    let web = provenance(Surface::Web);
    let good = reference("p1", "t1", &world.html);

    // Metadata is allowed and carries no path or body.
    let ArtifactsReply::Metadata(metadata) = world
        .artifacts(
            ArtifactsQuery::Get {
                artifact: good.clone(),
            },
            &web,
        )
        .unwrap()
    else {
        panic!("not metadata")
    };
    assert_eq!(metadata.reference, good);
    assert!(!format!("{metadata:?}").contains("/one/"));
    assert!(
        world
            .artifacts(
                ArtifactsQuery::List {
                    project: ProjectId::new("p1").unwrap(),
                    thread: ThreadId::new("t1").unwrap(),
                    after: None,
                    limit: 10,
                },
                &web,
            )
            .is_ok()
    );
    // Metadata of an unauthorized claim is still refused for Web.
    assert_eq!(
        world
            .artifacts(
                ArtifactsQuery::Get {
                    artifact: reference("p2", "t2", &world.html)
                },
                &web
            )
            .unwrap_err(),
        ErrorCode::NotFound
    );

    // Every content read is one fixed error, text included, and it does not
    // depend on whether the artifact exists, so it cannot be used as an oracle.
    let missing = reference("unknown", "unknown", &world.html);
    for artifact in [
        good.clone(),
        reference("p1", "t1", &world.second),
        reference("p1", "t1", &world.image),
        reference("p2", "t2", &world.html),
        missing,
    ] {
        assert_eq!(
            world.read(&artifact, 0, 64, &web).unwrap_err(),
            ErrorCode::PermissionDenied
        );
    }
    // Renderer queries (a capture reference included) are refused whatever the id.
    assert_eq!(
        world
            .query(
                Query::Browser(BrowserQuery::GetRendererOperation {
                    operation_id: RendererId::new("0".repeat(32)).unwrap(),
                }),
                &web,
            )
            .unwrap_err(),
        ErrorCode::PermissionDenied
    );
    world.core.shutdown().await.unwrap();
}
