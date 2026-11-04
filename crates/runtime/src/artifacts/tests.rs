use super::*;
use crate::artifacts::MetaStop;
use crate::{AgentInvocationContext, AgentModel, AgentRunPhase, RunConfig, RunId, RuntimeError};
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolResultContent, Usage};
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use tokio::sync::Notify;

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR-test-image";

fn request(title: &str) -> CaptureRequest<'_> {
    CaptureRequest {
        title,
        caption: None,
        run_id: "1".into(),
        root_run_id: "1".into(),
        thread_id: None,
    }
}

fn store() -> (tempfile::TempDir, ArtifactStore) {
    let directory = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(directory.path().join("artifacts")).unwrap();
    (directory, store)
}

#[test]
fn capture_keeps_the_content_seen_at_capture_time() {
    let (_store_dir, store) = store();
    let workspace = tempfile::tempdir().unwrap();
    let source = workspace.path().join("mock.png");
    std::fs::write(&source, PNG).unwrap();
    let first = store
        .capture(workspace.path(), None, Path::new("mock.png"), request("v1"))
        .unwrap();
    std::fs::write(&source, b"\x89PNG\r\n\x1a\nsecond revision").unwrap();
    let second = store
        .capture(workspace.path(), None, &source, request("v2"))
        .unwrap();
    std::fs::remove_file(&source).unwrap();

    assert_ne!(first.artifact_id, second.artifact_id);
    assert_ne!(first.sha256, second.sha256);
    assert_eq!(std::fs::read(store.blob_path(&first)).unwrap(), PNG);
    assert_eq!(store.load(&first.artifact_id).unwrap(), first);
    assert_eq!(first.media_type, "image/png");
    assert_eq!(first.byte_len, PNG.len() as u64);
}

/// Looks at the store at the moment a capture asks to record its reference.
#[derive(Debug)]
struct ObservingRecorder {
    root: PathBuf,
    fail: bool,
    seen: Mutex<Vec<(storage::artifact_refs::ArtifactRef, bool, bool)>>,
}

impl ArtifactRefRecorder for ObservingRecorder {
    fn record(&self, reference: &storage::artifact_refs::ArtifactRef) -> Result<(), String> {
        let blob = self
            .root
            .join("blobs")
            .join(format!("{}.png", reference.sha256));
        let meta = self
            .root
            .join("meta")
            .join(format!("{}.json", reference.artifact_id));
        self.seen
            .lock()
            .unwrap()
            .push((reference.clone(), blob.is_file(), meta.exists()));
        if self.fail {
            Err("database is gone".into())
        } else {
            Ok(())
        }
    }
}

fn observed_store(
    fail: bool,
) -> (
    tempfile::TempDir,
    PathBuf,
    Arc<ObservingRecorder>,
    ArtifactStore,
) {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("artifacts");
    let recorder = Arc::new(ObservingRecorder {
        root: root.clone(),
        fail,
        seen: Mutex::new(Vec::new()),
    });
    let store = ArtifactStore::new(&root)
        .unwrap()
        .with_ref_recorder(recorder.clone());
    (directory, root, recorder, store)
}

// Given: a store with a reference recorder / When: an image is captured / Then: the
// reference is recorded after the blob exists and before the meta file is published.
#[test]
fn the_reference_is_recorded_between_the_blob_and_the_meta() {
    let (_directory, root, recorder, store) = observed_store(false);
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("mock.png"), PNG).unwrap();

    let record = store
        .capture(
            workspace.path(),
            None,
            Path::new("mock.png"),
            CaptureRequest {
                thread_id: Some("thread-1".into()),
                ..request("t")
            },
        )
        .unwrap();

    let seen = recorder.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let (reference, blob_existed, meta_existed) = &seen[0];
    assert!(*blob_existed && !*meta_existed);
    assert_eq!(reference.artifact_id, record.artifact_id);
    assert_eq!(reference.sha256, record.sha256);
    assert_eq!(reference.thread_id.as_deref(), Some("thread-1"));
    assert_eq!(reference.run_id.as_deref(), Some("1"));
    assert!(
        root.join("meta")
            .join(format!("{}.json", record.artifact_id))
            .is_file()
    );
}

// Given: a recorder that fails / When: an image is captured / Then: the capture fails
// without publishing a meta file; the blob stays for the collector to reclaim.
#[test]
fn a_capture_whose_reference_cannot_be_recorded_publishes_no_meta() {
    let (_directory, root, recorder, store) = observed_store(true);
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("mock.png"), PNG).unwrap();

    let error = store
        .capture(workspace.path(), None, Path::new("mock.png"), request("t"))
        .unwrap_err();

    assert!(error.contains("database is gone"), "{error}");
    assert_eq!(recorder.seen.lock().unwrap().len(), 1);
    assert_eq!(std::fs::read_dir(root.join("meta")).unwrap().count(), 0);
    assert_eq!(std::fs::read_dir(root.join("blobs")).unwrap().count(), 1);
}

// Given: a store wired to a storage writer that has stopped / When: an image is captured
// Then: the capture fails and no meta file appears (no reference, so no visible artifact).
#[test]
fn a_stopped_writer_fails_the_capture_before_any_meta_appears() {
    let directory = tempfile::tempdir().unwrap();
    let storage = storage::Storage::open(storage::StorageConfig {
        db_path: directory.path().join("events.db"),
        ..Default::default()
    })
    .unwrap();
    let root = directory.path().join("artifacts");
    let store = store_with_refs(&root, storage.handle()).unwrap();
    storage.close();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("mock.png"), PNG).unwrap();

    let error = store
        .capture(workspace.path(), None, Path::new("mock.png"), request("t"))
        .unwrap_err();

    assert!(error.contains("cannot record"), "{error}");
    assert_eq!(std::fs::read_dir(root.join("meta")).unwrap().count(), 0);
}

#[test]
fn capture_rejects_files_outside_or_unlike_supported_artifacts() {
    let (_store_dir, store) = store();
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let root = workspace.path();
    std::fs::write(outside.path().join("secret.png"), PNG).unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret.png"), root.join("link.png")).unwrap();
    std::fs::create_dir(root.join("folder.png")).unwrap();
    std::fs::write(root.join("vector.svg"), "<svg/>").unwrap();
    std::fs::write(root.join("text.png"), "not an image").unwrap();
    std::fs::write(root.join("binary.html"), b"<p>\0</p>").unwrap();
    std::fs::write(root.join("empty.gif"), b"").unwrap();
    let large = std::fs::File::create(root.join("large.png")).unwrap();
    large.set_len(MAX_IMAGE_BYTES + 1).unwrap();
    let mut html = b"<!doctype html>".to_vec();
    html.resize(usize::try_from(MAX_HTML_BYTES).unwrap() + 1, b' ');
    std::fs::write(root.join("large.html"), html).unwrap();
    std::fs::create_dir(root.join("sub")).unwrap();

    let outside_absolute = outside.path().join("secret.png");
    for (path, reason) in [
        (outside_absolute.as_path(), "outside"),
        (Path::new("sub/../../secret.png"), "cannot open"),
        (Path::new("link.png"), "outside"),
        (Path::new("folder.png"), "regular file"),
        (Path::new("vector.svg"), "not a supported"),
        (Path::new("text.png"), "valid image/png"),
        (Path::new("binary.html"), "valid text/html"),
        (Path::new("empty.gif"), "valid image/gif"),
        (Path::new("large.png"), "8 MiB"),
        (Path::new("large.html"), "2 MiB"),
        (Path::new("missing.png"), "cannot open"),
    ] {
        let error = store.capture(root, None, path, request("x")).unwrap_err();
        assert!(error.contains(reason), "{}: {error}", path.display());
    }
    assert_eq!(
        std::fs::read_dir(store.root.join("meta")).unwrap().count(),
        0,
        "rejected captures must not be stored"
    );
}

#[test]
fn html_with_credentials_is_not_captured() {
    let (_store_dir, store) = store();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("mock.html"),
        format!("<p>key: sk-{}</p>", "a".repeat(32)),
    )
    .unwrap();
    let error = store
        .capture(
            workspace.path(),
            None,
            Path::new("mock.html"),
            request("mock"),
        )
        .unwrap_err();
    assert!(error.contains("credential"), "{error}");
    assert_eq!(
        std::fs::read_dir(store.root.join("blobs")).unwrap().count(),
        0
    );
}

#[test]
fn load_rejects_ids_that_are_not_store_identities() {
    let (_store_dir, store) = store();
    for id in [
        "../meta/x",
        "artifact-../../etc/passwd",
        "artifact-xyz",
        "presentation-00000000000000000000000000000000",
    ] {
        assert!(store.load(id).unwrap_err().contains("invalid"), "{id}");
    }
    assert!(
        store
            .load("artifact-00000000000000000000000000000000")
            .unwrap_err()
            .contains("unknown")
    );
}

// --- runtime flow -----------------------------------------------------------

/// Builds the next tool calls from the conversation so far.
type Script = Box<dyn Fn(&[Message]) -> Vec<(&'static str, Value)> + Send>;

enum Step {
    Calls(Script),
    /// Signal `started`, then wait for `release` before building the calls.
    HoldThen(Arc<Notify>, Arc<Notify>, Script),
}

fn calls(calls: Vec<(&'static str, Value)>) -> Step {
    Step::Calls(Box::new(move |_| calls.clone()))
}

fn stop() -> Step {
    calls(vec![])
}

/// Each run follows its own script, so concurrent runs cannot steal steps.
struct Model {
    scripts: Mutex<HashMap<String, VecDeque<Step>>>,
    results: Mutex<HashMap<String, Vec<(bool, String)>>>,
    tools: Mutex<HashMap<String, Vec<String>>>,
}

#[async_trait::async_trait]
impl AgentModel for Model {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "test".into()
    }

    async fn complete(
        &self,
        ctx: &AgentInvocationContext,
        _: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.tools.lock().unwrap().insert(
            ctx.run_id.clone(),
            tools.iter().map(|tool| tool.name.clone()).collect(),
        );
        self.results
            .lock()
            .unwrap()
            .insert(ctx.run_id.clone(), tool_results(messages));
        let step = self
            .scripts
            .lock()
            .unwrap()
            .get_mut(&ctx.run_id)
            .and_then(VecDeque::pop_front)
            .unwrap_or_else(stop);
        let calls = match step {
            Step::Calls(script) => script(messages),
            Step::HoldThen(started, release, script) => {
                started.notify_one();
                release.notified().await;
                script(messages)
            }
        };
        let finished = calls.is_empty();
        let content = if finished {
            vec![ContentBlock::Text {
                text: "Result".into(),
            }]
        } else {
            calls
                .into_iter()
                .enumerate()
                .map(|(index, (name, input))| ContentBlock::ToolUse {
                    id: format!("call-{}-{}-{index}", ctx.run_id, messages.len()),
                    name: name.into(),
                    input,
                })
                .collect()
        };
        Ok(ChatResponse {
            usage_reported: true,
            message: Message {
                role: providers::Role::Assistant,
                content,
            },
            finish_reason: if finished {
                FinishReason::Stop
            } else {
                FinishReason::ToolUse
            },
            usage: Usage::default(),
        })
    }
}

fn tool_results(messages: &[Message]) -> Vec<(bool, String)> {
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

/// The artifact id in the latest successful `render_artifact` result.
fn last_artifact_id(messages: &[Message]) -> String {
    tool_results(messages)
        .into_iter()
        .rev()
        .find_map(|(error, text)| {
            (!error).then_some(())?;
            serde_json::from_str::<Value>(&text).ok()?["artifact_id"]
                .as_str()
                .map(str::to_owned)
        })
        .expect("a captured artifact")
}

struct Harness {
    runtime: AgentRuntime,
    model: Arc<Model>,
    events: event_bus::EventReceiver,
    workspace: tempfile::TempDir,
    _store: tempfile::TempDir,
}

impl Harness {
    fn new() -> Self {
        Self::build(|bus, executor, model, _| AgentRuntime::new(bus, executor, model))
    }

    /// Runs get their own sandbox, unisolated here, and share `scratch` per thread
    /// as their `$TMPDIR`, as in production.
    fn sandboxed(scratch: Arc<crate::scratch::ThreadScratch>) -> Self {
        struct Unisolated;
        impl crate::SandboxFactory for Unisolated {
            fn build(
                &self,
                _: &crate::ExecutionPolicy,
                _: &crate::WorkspaceMounts,
            ) -> Result<Arc<dyn sandbox::Sandbox>, sandbox::SandboxError> {
                Ok(Arc::new(sandbox::DirectSandbox::new_unchecked()))
            }
        }
        let harness = Self::build(|bus, executor, model, workspace| {
            AgentRuntime::with_sandbox_factory(bus, executor, model, Arc::new(Unisolated))
                .with_sandbox_root(workspace.to_path_buf())
        });
        harness.runtime.set_thread_scratch(scratch);
        harness
    }

    fn build(
        runtime: impl FnOnce(
            Arc<event_bus::EventBus>,
            Arc<::tools::ToolExecutor>,
            Arc<dyn AgentModel>,
            &Path,
        ) -> AgentRuntime,
    ) -> Self {
        let model = Arc::new(Model {
            scripts: Mutex::new(HashMap::new()),
            results: Mutex::new(HashMap::new()),
            tools: Mutex::new(HashMap::new()),
        });
        let bus = Arc::new(event_bus::EventBus::new(4096));
        let events = bus.subscribe();
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("mock.png"), PNG).unwrap();
        std::fs::write(
            workspace.path().join("mock.html"),
            "<!doctype html><p>Mock</p>",
        )
        .unwrap();
        let executor = ::tools::ToolExecutor::new(bus.clone());
        executor.set_default_cwd(workspace.path().to_path_buf());
        let store_dir = tempfile::tempdir().unwrap();
        let runtime = runtime(bus, Arc::new(executor), model.clone(), workspace.path())
            .with_artifact_store(Arc::new(ArtifactStore::new(store_dir.path()).unwrap()));
        Self {
            runtime,
            model,
            events,
            workspace,
            _store: store_dir,
        }
    }

    fn script(&self, run: RunId, steps: Vec<Step>) {
        self.model
            .scripts
            .lock()
            .unwrap()
            .insert(run.to_string(), steps.into());
    }

    fn root(&self, thread: &str, role: Role, steps: Vec<Step>) -> RunId {
        let run = self.runtime.reserve_run_id();
        self.script(run, steps);
        self.runtime.bind_thread_root(thread, run).unwrap();
        let category = (role == Role::Worker).then(|| CategoryId::Conversation.to_string());
        self.runtime.spawn_reserved(
            run,
            None,
            role,
            "Discuss the UI",
            RunConfig {
                conversation: true,
                category,
                ..Default::default()
            },
        )
    }

    fn visual_child(&self, parent: RunId, steps: Vec<Step>) -> RunId {
        let run = self.runtime.reserve_run_id();
        self.script(run, steps);
        self.runtime
            .spawn_reserved_child(
                parent,
                run,
                Role::Worker,
                "Draw the mock",
                RunConfig {
                    category: Some(CategoryId::Visual.to_string()),
                    ..Default::default()
                },
            )
            .expect("a non-Orchestrator child is admitted")
    }

    fn results(&self, run: RunId) -> Vec<(bool, String)> {
        self.model.results.lock().unwrap()[&run.to_string()].clone()
    }

    fn presentations(&mut self) -> Vec<(String, ArtifactPresentation)> {
        self.events
            .drain_pending_snapshot()
            .into_iter()
            .filter_map(|event| match event.kind {
                event_bus::EventKind::Tool(ToolEvent::ArtifactsPresented {
                    run_id,
                    presentation,
                }) => Some((run_id, presentation)),
                _ => None,
            })
            .collect()
    }
}

fn render(path: &str, title: &str) -> Step {
    calls(vec![(
        "render_artifact",
        json!({"path": path, "title": title}),
    )])
}

#[tokio::test]
async fn conversation_root_worker_captures_and_presents_its_own_mock() {
    let mut harness = Harness::new();
    let run = harness.root(
        "thread",
        Role::Worker,
        vec![
            render("mock.png", "Login v1"),
            Step::Calls(Box::new(|messages| {
                vec![(
                    "present",
                    json!({"artifact_ids": [last_artifact_id(messages)], "title": "Login"}),
                )]
            })),
            stop(),
        ],
    );
    assert_eq!(
        harness.runtime.wait(run).await.unwrap(),
        AgentRunPhase::Done
    );

    let results = harness.results(run);
    assert!(results.iter().all(|(error, _)| !error), "{results:?}");
    let presentations = harness.presentations();
    assert_eq!(presentations.len(), 1);
    let (presenter, presentation) = &presentations[0];
    assert_eq!(presenter, &run.to_string());
    assert_eq!(presentation.title.as_deref(), Some("Login"));
    let artifact = &presentation.artifacts[0];
    assert_eq!(artifact.title, "Login v1");
    assert!(artifact.is_image());
    assert_eq!(std::fs::read(&artifact.path).unwrap(), PNG);
    assert!(
        !artifact
            .path
            .starts_with(&*harness.workspace.path().to_string_lossy())
    );
    assert!(
        results[1]
            .1
            .contains(&reference(&artifact.artifact_id, "Login v1"))
    );
}

#[tokio::test]
async fn orchestrator_presents_what_its_visual_worker_returned() {
    let mut harness = Harness::new();
    let (started, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let artifact = Arc::new(Mutex::new(String::new()));
    let presented = artifact.clone();
    let root = harness.root(
        "thread",
        Role::Orchestrator,
        vec![
            // Present only after the delegated worker has returned its artifact.
            Step::HoldThen(
                started.clone(),
                release.clone(),
                Box::new(move |_| {
                    vec![(
                        "present",
                        json!({"artifact_ids": [presented.lock().unwrap().clone()]}),
                    )]
                }),
            ),
            stop(),
        ],
    );
    started.notified().await;
    let child = harness.visual_child(
        root,
        vec![
            render("mock.html", "Settings mock"),
            // A delegated agent cannot show anything to the user itself.
            Step::Calls(Box::new(|messages| {
                vec![(
                    "present",
                    json!({"artifact_ids": [last_artifact_id(messages)]}),
                )]
            })),
            stop(),
        ],
    );
    assert_eq!(
        harness.runtime.wait(child).await.unwrap(),
        AgentRunPhase::Done
    );
    let child_results = harness.results(child);
    assert!(!child_results[0].0, "{child_results:?}");
    assert!(child_results[1].0, "child present must be denied");
    assert!(harness.presentations().is_empty());
    *artifact.lock().unwrap() =
        serde_json::from_str::<Value>(&child_results[0].1).unwrap()["artifact_id"]
            .as_str()
            .unwrap()
            .to_owned();

    release.notify_one();
    assert_eq!(
        harness.runtime.wait(root).await.unwrap(),
        AgentRunPhase::Done
    );
    assert!(!harness.results(root)[0].0, "{:?}", harness.results(root));
    let presentations = harness.presentations();
    assert_eq!(presentations.len(), 1);
    assert_eq!(presentations[0].0, root.to_string());
    assert_eq!(presentations[0].1.artifacts[0].media_type, "text/html");
}

#[tokio::test]
async fn present_rejects_other_conversations_and_shows_nothing_on_any_error() {
    let mut harness = Harness::new();
    let other = harness.root("other", Role::Worker, vec![render("mock.png", "Other")]);
    assert_eq!(
        harness.runtime.wait(other).await.unwrap(),
        AgentRunPhase::Done
    );
    let foreign =
        serde_json::from_str::<Value>(&harness.results(other)[0].1).unwrap()["artifact_id"]
            .as_str()
            .unwrap()
            .to_owned();

    let own_then_foreign = foreign.clone();
    let run = harness.root(
        "thread",
        Role::Worker,
        vec![
            render("mock.png", "Mine"),
            Step::Calls(Box::new(move |messages| {
                vec![(
                    "present",
                    json!({"artifact_ids": [last_artifact_id(messages), own_then_foreign]}),
                )]
            })),
            calls(vec![(
                "present",
                json!({"artifact_ids": ["artifact-00000000000000000000000000000000"]}),
            )]),
            calls(vec![("present", json!({"artifact_ids": []}))]),
            stop(),
        ],
    );
    assert_eq!(
        harness.runtime.wait(run).await.unwrap(),
        AgentRunPhase::Done
    );
    let results = harness.results(run);
    assert!(
        results[1].0 && results[1].1.contains("does not belong"),
        "{results:?}"
    );
    assert!(results[2].0 && results[2].1.contains("unknown artifact"));
    assert!(results[3].0);
    assert!(harness.presentations().is_empty());

    // A later root of the same thread, such as an escalated Orchestrator, may show it.
    let mine = serde_json::from_str::<Value>(&results[0].1).unwrap()["artifact_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let successor = harness.root(
        "thread",
        Role::Orchestrator,
        vec![calls(vec![("present", json!({"artifact_ids": [mine]}))])],
    );
    assert_eq!(
        harness.runtime.wait(successor).await.unwrap(),
        AgentRunPhase::Done
    );
    assert!(!harness.results(successor)[0].0);
    assert_eq!(harness.presentations().len(), 1);
}

#[tokio::test]
async fn orchestrator_cannot_capture_and_missing_store_fails_closed() {
    let harness = Harness::new();
    let run = harness.root(
        "thread",
        Role::Orchestrator,
        vec![render("mock.png", "Nope")],
    );
    assert_eq!(
        harness.runtime.wait(run).await.unwrap(),
        AgentRunPhase::Done
    );
    assert!(harness.results(run)[0].0);

    let model = harness.model.clone();
    let bus = Arc::new(event_bus::EventBus::new(64));
    let executor = ::tools::ToolExecutor::new(bus.clone());
    executor.set_default_cwd(harness.workspace.path().to_path_buf());
    let runtime = AgentRuntime::new(bus, Arc::new(executor), model.clone());
    let run = runtime.reserve_run_id();
    model
        .scripts
        .lock()
        .unwrap()
        .insert(run.to_string(), vec![render("mock.png", "No store")].into());
    runtime.spawn_reserved(
        run,
        None,
        Role::Worker,
        "Draw",
        RunConfig {
            category: Some(CategoryId::Visual.to_string()),
            ..Default::default()
        },
    );
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Done);
    let result = &model.results.lock().unwrap()[&run.to_string()][0];
    assert!(
        result.0 && result.1.contains("not configured"),
        "{result:?}"
    );
}

mod inspect;
mod scratch;

fn capture_for(
    store: &ArtifactStore,
    workspace: &Path,
    run: &str,
    root: &str,
    thread: Option<&str>,
) -> ArtifactRecord {
    store
        .capture(
            workspace,
            None,
            Path::new("mock.png"),
            CaptureRequest {
                title: "t",
                caption: None,
                run_id: run.into(),
                root_run_id: root.into(),
                thread_id: thread.map(str::to_owned),
            },
        )
        .unwrap()
}

fn set(items: &[&str]) -> std::collections::HashSet<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

// Given: captures of a deleted thread, of a deleted run without a thread, and of a
// survivor, all sharing one blob / When: their meta is removed
// Then: only the family's meta files go; the shared blob and the survivor stay.
#[test]
fn remove_meta_for_deletes_matching_thread_and_run_meta_but_never_blobs() {
    let (_store_dir, store) = store();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("mock.png"), PNG).unwrap();
    let by_thread = capture_for(&store, workspace.path(), "7", "7", Some("gone"));
    let by_run = capture_for(&store, workspace.path(), "8", "8", None);
    let by_root = capture_for(&store, workspace.path(), "9", "8", Some("other"));
    let survivor = capture_for(&store, workspace.path(), "10", "10", Some("kept"));

    let removal = store.remove_meta_for(&set(&["gone"]), &set(&["8"]), 100);

    assert!(removal.is_complete() && removal.failed.is_empty());
    let mut removed = removal.removed.clone();
    removed.sort();
    let mut expected = vec![
        by_thread.artifact_id.clone(),
        by_run.artifact_id.clone(),
        by_root.artifact_id.clone(),
    ];
    expected.sort();
    assert_eq!(removed, expected);
    assert!(store.load(&survivor.artifact_id).is_ok());
    assert!(store.load(&by_thread.artifact_id).is_err());
    assert!(
        store.blob_path(&survivor).is_file(),
        "the blob is shared and is never removed here"
    );
}

// Given: a meta file that cannot be parsed next to a matching one / When: removing
// Then: the unreadable file is reported as failed (not skipped as "not ours") and
// the readable match is still removed.
#[test]
fn remove_meta_for_reports_unreadable_meta_as_failed() {
    let (_store_dir, store) = store();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("mock.png"), PNG).unwrap();
    let ours = capture_for(&store, workspace.path(), "7", "7", Some("gone"));
    let corrupt = store
        .meta_path("artifact-00000000000000000000000000000000")
        .unwrap();
    std::fs::write(&corrupt, b"{ not json").unwrap();

    let removal = store.remove_meta_for(&set(&["gone"]), &set(&[]), 100);

    assert_eq!(removal.removed, vec![ours.artifact_id]);
    assert_eq!(removal.failed.len(), 1);
    assert_eq!(removal.failed[0].0, corrupt);
    assert!(corrupt.exists(), "an unreadable file is left in place");
}

// Given: more meta files than the budget / When: continuing from `last_name`
// Then: each call reads at most `budget` files and the calls together cover all.
#[test]
fn remove_meta_after_resumes_within_a_budget() {
    let (_store_dir, store) = store();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("mock.png"), PNG).unwrap();
    let records = (0..5)
        .map(|index| {
            capture_for(
                &store,
                workspace.path(),
                &index.to_string(),
                "1",
                Some("gone"),
            )
        })
        .collect::<Vec<_>>();

    let mut after: Option<String> = None;
    let mut removed = Vec::new();
    let mut calls = 0;
    loop {
        let step = store.remove_meta_after(&set(&["gone"]), &set(&[]), after.as_deref(), 2);
        assert!(step.scanned <= 2);
        let complete = step.is_complete();
        removed.extend(step.removed);
        calls += 1;
        if complete {
            break;
        }
        after = step.last_name;
    }

    assert_eq!(calls, 3);
    removed.sort();
    let mut expected = records
        .into_iter()
        .map(|r| r.artifact_id)
        .collect::<Vec<_>>();
    expected.sort();
    assert_eq!(removed, expected);
}

fn meta_dir(store_dir: &tempfile::TempDir) -> std::path::PathBuf {
    store_dir.path().join("artifacts/meta")
}

fn meta_name(index: u32) -> String {
    format!("artifact-{index:032x}.json")
}

// Given: the meta directory cannot be listed (a file stands where it belongs)
// When: removing / Then: the call reports a listing failure with one counted failure,
// not "complete": nothing is known about what was left.
#[test]
fn an_unlistable_meta_directory_is_a_listing_failure_not_completion() {
    let (store_dir, store) = store();
    std::fs::remove_dir_all(meta_dir(&store_dir)).unwrap();
    std::fs::write(meta_dir(&store_dir), b"not a directory").unwrap();

    let removal = store.remove_meta_for(&set(&["gone"]), &set(&[]), 100);

    assert!(
        matches!(removal.stop, Some(MetaStop::Listing(_))),
        "{removal:?}"
    );
    assert!(!removal.is_complete());
    assert_eq!(removal.failed_total, 1);
    assert_eq!(removal.scanned, 0);

    // A directory that does not exist holds no meta: that, and only that, is complete.
    std::fs::remove_file(meta_dir(&store_dir)).unwrap();
    let missing = store.remove_meta_for(&set(&["gone"]), &set(&[]), 100);
    assert!(missing.is_complete() && missing.failed_total == 0);
}

// Given: an entry that cannot be read (a directory with a meta name), one that is gone
// (a dangling link), an oversized file and many corrupt files / When: removing
// Then: read errors, oversize and corruption are failures (capped in detail, all counted);
// only the explicitly missing entry is treated as already gone; reads are bounded.
#[test]
fn only_an_explicit_not_found_counts_as_already_gone() {
    let (store_dir, store) = store();
    let dir = meta_dir(&store_dir);
    std::fs::create_dir(dir.join(meta_name(1))).unwrap();
    std::os::unix::fs::symlink(dir.join("nowhere"), dir.join(meta_name(2))).unwrap();
    std::fs::write(
        dir.join(meta_name(3)),
        vec![b' '; (crate::artifacts::MAX_META_BYTES + 10) as usize],
    )
    .unwrap();
    for index in 100..140 {
        std::fs::write(dir.join(meta_name(index)), b"{ broken").unwrap();
    }

    let removal = store.remove_meta_for(&set(&["gone"]), &set(&[]), 1_000);

    assert!(removal.is_complete());
    assert_eq!(removal.scanned, 43);
    // 1 unreadable + 1 oversized + 40 corrupt; the dangling link is not a failure.
    assert_eq!(removal.failed_total, 42, "{removal:?}");
    assert_eq!(removal.failed.len(), crate::artifacts::MAX_FAILURES_KEPT);
    assert!(removal.bytes_read <= crate::artifacts::MAX_META_BYTES + 1 + 40 * 8);
    assert!(removal.removed.is_empty());
}

// Given: a file that exists but cannot be opened / When: removing
// Then: it is a failure (skipped when the process ignores permissions).
#[test]
fn a_permission_error_is_a_failure_not_an_absence() {
    use std::os::unix::fs::PermissionsExt;
    let (store_dir, store) = store();
    let path = meta_dir(&store_dir).join(meta_name(7));
    std::fs::write(&path, br#"{"thread_id":"gone"}"#).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::File::open(&path).is_ok() {
        return;
    }

    let removal = store.remove_meta_for(&set(&["gone"]), &set(&[]), 10);

    assert_eq!(removal.failed_total, 1);
    assert!(removal.removed.is_empty() && path.exists());
}

// Given: thousands of meta files and a small budget / When: removing in steps
// Then: each call keeps at most `budget` names and opens at most `budget` files, lists
// every entry exactly once, resumes from the cursor in name order and the steps together
// cover each file once.
#[test]
fn a_large_meta_directory_is_walked_in_budgeted_steps() {
    let (store_dir, store) = store();
    let dir = meta_dir(&store_dir);
    for index in 0..2_000 {
        std::fs::write(dir.join(meta_name(index)), br#"{"thread_id":"other"}"#).unwrap();
    }
    let expected_last = meta_name(99);

    let first = store.remove_meta_for(&set(&["gone"]), &set(&[]), 100);
    assert_eq!(first.listed, 2_000);
    assert_eq!(first.scanned, 100);
    assert_eq!(first.stop, Some(MetaStop::Budget));
    assert_eq!(first.last_name.as_deref(), Some(expected_last.as_str()));
    assert_eq!(first.bytes_read, 100 * 21);

    let mut after = first.last_name;
    let mut scanned = first.scanned;
    let mut calls = 1;
    loop {
        let step = store.remove_meta_after(&set(&["gone"]), &set(&[]), after.as_deref(), 100);
        assert_eq!(step.listed, 2_000, "one listing pass per call");
        assert!(step.scanned <= 100);
        scanned += step.scanned;
        calls += 1;
        if step.is_complete() {
            break;
        }
        after = step.last_name;
    }
    assert_eq!((scanned, calls), (2_000, 20));
}
