use super::*;
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
        .capture(workspace.path(), Path::new("mock.png"), request("v1"))
        .unwrap();
    std::fs::write(&source, b"\x89PNG\r\n\x1a\nsecond revision").unwrap();
    let second = store
        .capture(workspace.path(), &source, request("v2"))
        .unwrap();
    std::fs::remove_file(&source).unwrap();

    assert_ne!(first.artifact_id, second.artifact_id);
    assert_ne!(first.sha256, second.sha256);
    assert_eq!(std::fs::read(store.blob_path(&first)).unwrap(), PNG);
    assert_eq!(store.load(&first.artifact_id).unwrap(), first);
    assert_eq!(first.media_type, "image/png");
    assert_eq!(first.byte_len, PNG.len() as u64);
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
        let error = store.capture(root, path, request("x")).unwrap_err();
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
        .capture(workspace.path(), Path::new("mock.html"), request("mock"))
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
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
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
                        ToolResultContent::Text { text } => text.as_str(),
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
        let model = Arc::new(Model {
            scripts: Mutex::new(HashMap::new()),
            results: Mutex::new(HashMap::new()),
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
        let runtime = AgentRuntime::new(bus, Arc::new(executor), model.clone())
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
        self.runtime.spawn_reserved_child(
            parent,
            run,
            Role::Worker,
            "Draw the mock",
            RunConfig {
                category: Some(CategoryId::Visual.to_string()),
                ..Default::default()
            },
        )
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
