//! Real routing/HTTP cache contract with the production GUI initialization
//! bridge. Only the CEF backend is replaced, so this gate never downloads CEF.
#![cfg(feature = "browser")]
#[path = "../src/html_preview/runtime_requests.rs"]
mod runtime_requests;

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use browser::agent::{AgentError, CdpTarget, PageAgent};
use event_bus::{AgentRunPhase, EventBus, EventKind, LifecycleEvent, ProviderEvent};
use mock_openai::cache_contract::{CacheProtocol, assert_append_only};
use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
use runtime::artifacts::{ArtifactPage, ArtifactPages, ArtifactStore, CaptureRequest};
use runtime::{ModelSource, Role, RunConfig, RuntimeComposition, compose_runtime};
use runtime_requests::{RuntimeRequestSender, RuntimeRequests};
use serde_json::{Value, json};

fn context() -> egui::Context {
    let ctx = egui::Context::default();
    idle_logic(&ctx);
    ctx
}

fn idle_logic(ctx: &egui::Context) {
    loop {
        let _ = ctx.run_logic(&Default::default(), |_| {});
        if !ctx.has_requested_repaint() {
            break;
        }
    }
}

#[test]
fn preview_initialization_is_reused_without_a_gui_handoff() {
    let requests = RuntimeRequests::new();
    let sender = requests.sender(&context(), Duration::ZERO);
    requests.finish(Ok(()));
    assert_eq!(sender.ensure_runtime(), Ok(()));
    assert!(requests.take_pending().is_empty());
}

#[test]
fn hidden_logic_initializes_once_and_cached_opens_need_no_gui_pass() {
    let ctx = context();
    let requests = RuntimeRequests::new();
    let sender = requests.sender(&ctx, Duration::MAX);
    let (wake, repaint) = std::sync::mpsc::channel();
    ctx.set_request_repaint_callback(move |_| wake.send(()).unwrap());
    let calls = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let first = scope.spawn(|| sender.ensure_runtime());
        repaint.recv().unwrap();
        let _ = ctx.run_logic(&Default::default(), |_| {
            for request in requests.take_pending() {
                request.respond_with(|| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                });
            }
        });
        assert_eq!(first.join().unwrap(), Ok(()));
    });
    assert_eq!(sender.ensure_runtime(), Ok(()));
    assert!(requests.take_pending().is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    drop(requests);
    assert_eq!(
        sender.ensure_runtime(),
        Err("the GUI is shutting down".into())
    );
}

#[test]
fn expired_requests_do_not_initialize_and_a_fresh_request_can_retry() {
    let ctx = context();
    let requests = RuntimeRequests::new();
    let expired = requests.sender(&ctx, Duration::ZERO);
    assert_eq!(
        expired.ensure_runtime(),
        Err("the GUI did not initialize CEF in time".into())
    );
    for request in requests.take_pending() {
        request.respond_with(|| panic!("expired requests must not initialize CEF"));
    }
    let sender = requests.sender(&ctx, Duration::MAX);
    let (wake, repaint) = std::sync::mpsc::channel();
    idle_logic(&ctx);
    ctx.set_request_repaint_callback(move |_| wake.send(()).unwrap());
    std::thread::scope(|scope| {
        let retry = scope.spawn(|| sender.ensure_runtime());
        repaint.recv().unwrap();
        let _ = ctx.run_logic(&Default::default(), |_| {
            for request in requests.take_pending() {
                request.respond_with(|| Ok(()));
            }
        });
        assert_eq!(retry.join().unwrap(), Ok(()));
    });
}

#[test]
fn queued_waiters_are_released_when_the_gui_drops() {
    let ctx = context();
    let requests = RuntimeRequests::new();
    let sender = requests.sender(&ctx, Duration::MAX);
    let (wake, repaint) = std::sync::mpsc::channel();
    ctx.set_request_repaint_callback(move |_| wake.send(()).unwrap());
    std::thread::scope(|scope| {
        let pending = scope.spawn(|| sender.ensure_runtime());
        repaint.recv().unwrap();
        drop(requests);
        assert_eq!(
            pending.join().unwrap(),
            Err("the GUI is shutting down".into())
        );
    });
}

struct Target;

#[async_trait::async_trait]
impl CdpTarget for Target {
    async fn call(&self, method: &str, _: Value) -> Result<Value, AgentError> {
        Ok(if method == "Accessibility.getFullAXTree" {
            json!({"nodes": [{
                "nodeId": "1", "role": {"value": "heading"},
                "name": {"value": "artifact ready"}, "backendDOMNodeId": 1
            }]})
        } else {
            json!({})
        })
    }

    async fn navigate(&self, _: &str) -> Result<(), AgentError> {
        Err(AgentError::Navigation("offline artifact".into()))
    }
}

struct Page(PageAgent);

impl ArtifactPage for Page {
    fn agent(&self) -> &PageAgent {
        &self.0
    }
    fn take_blocked(&self) -> Vec<String> {
        vec![]
    }
    fn take_dialogs(&self) -> Vec<String> {
        vec![]
    }
}

struct Pages {
    initializer: RuntimeRequestSender,
    pages: Mutex<HashMap<String, Arc<Page>>>,
}

#[async_trait::async_trait]
impl ArtifactPages for Pages {
    async fn open(&self, key: &str, html: Vec<u8>) -> Result<Arc<dyn ArtifactPage>, String> {
        assert_eq!(html, b"<!doctype html><h1>artifact ready</h1>");
        let initializer = self.initializer.clone();
        tokio::task::spawn_blocking(move || initializer.ensure_runtime())
            .await
            .unwrap()?;
        let page = Arc::new(Page(PageAgent::new(Arc::new(Target))));
        self.pages
            .lock()
            .unwrap()
            .insert(key.into(), Arc::clone(&page));
        Ok(page)
    }

    fn page(&self, key: &str) -> Option<Arc<dyn ArtifactPage>> {
        self.pages
            .lock()
            .unwrap()
            .get(key)
            .cloned()
            .map(|page| page as Arc<dyn ArtifactPage>)
    }

    fn close(&self, key: &str) -> bool {
        self.pages.lock().unwrap().remove(key).is_some()
    }
}

async fn cache_contract(initialize_ok: bool) {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(ArtifactStore::new(directory.path().join("artifacts")).unwrap());
    std::fs::write(
        directory.path().join("mock.html"),
        b"<!doctype html><h1>artifact ready</h1>",
    )
    .unwrap();
    let record = store
        .capture(
            directory.path(),
            None,
            Path::new("mock.html"),
            CaptureRequest {
                title: "mock",
                caption: None,
                run_id: "run-1".into(),
                root_run_id: "run-1".into(),
                thread_id: None,
            },
        )
        .unwrap();
    let inspect = |id: usize, input: Value| {
        ScriptedResponse::tool_call(
            &format!("response-{id}"),
            "mock-model",
            0,
            &format!("call-{id}"),
            "inspect_artifact",
            [input.to_string()],
        )
    };
    let open = json!({"action":"open", "artifact_id":record.artifact_id});
    let mock = StreamingMockOpenAi::spawn_with_prompt_cache(vec![
        inspect(0, json!({"action":"close"})),
        inspect(1, open.clone()),
        inspect(2, json!({"action":"close"})),
        inspect(3, open),
        ScriptedResponse::text_stream("done", "mock-model", ["done"]),
    ]);
    let config_path = config::project_main_config_path(directory.path());
    std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    std::fs::write(
        &config_path,
        format!(
            r#"
[providers.local]
type = "openai-compatible"
base_url = "{}"
api_key_env = "CACHE_ARTIFACT_KEY"
models = ["mock-model"]
default_model = "mock-model"
[[routing.routes.worker]]
profile = "local"
[compaction]
context_window_tokens = 1000000
"#,
            mock.base_url()
        ),
    )
    .unwrap();
    let config = config::Config::load(&config::LoadOptions {
        project_dir: Some(directory.path().into()),
        user_config_dir: Some(config_path.parent().unwrap().to_path_buf()),
        read_env: false,
        ..Default::default()
    })
    .unwrap();
    let bus = Arc::new(EventBus::new(1024));
    let mut events = bus.subscribe();
    let runtime = compose_runtime(RuntimeComposition {
        model_catalog_dir: Some(test_support::model_catalog_dir()),
        user_config_dir: Some(directory.path().join("empty-user-config")),
        config: &config,
        bus: Arc::clone(&bus),
        executor: Arc::new(tools::ToolExecutor::new(Arc::clone(&bus))),
        credential_store: Arc::new(
            sandbox::credential::FileCredentialStore::open(directory.path().join("credentials"))
                .unwrap(),
        ),
        env: Arc::new(routing::MapEnv::from_iter([(
            "CACHE_ARTIFACT_KEY",
            "offline-key",
        )])),
        model_source: ModelSource::Configured,
        workspace: None,
    })
    .unwrap()
    .runtime
    .with_sequential_run_ids()
    .with_artifact_store(store);
    let ctx = context();
    let requests = RuntimeRequests::new();
    let pages = Arc::new(Pages {
        initializer: requests.sender(&ctx, Duration::MAX),
        pages: Mutex::default(),
    });
    runtime.set_artifact_pages(pages);
    let (wake, mut repaint) = tokio::sync::mpsc::unbounded_channel();
    ctx.set_request_repaint_callback(move |_| {
        let _ = wake.send(());
    });
    let run = runtime.reserve_run_id();
    assert_eq!(run.to_string(), record.root_run_id);
    runtime.bind_thread_root("cache-artifact", run).unwrap();
    runtime.spawn_reserved(
        run,
        None,
        Role::Worker,
        "Inspect the artifact",
        RunConfig {
            conversation: true,
            category: Some(config::agent_categories::CategoryId::Conversation.to_string()),
            ..Default::default()
        },
    );
    let mut initialization_count = 0;
    let mut usage = vec![];
    loop {
        tokio::select! {
            Some(()) = repaint.recv() => {
                let _ = ctx.run_logic(&Default::default(), |_| {
                    for request in requests.take_pending() {
                        request.respond_with(|| {
                            initialization_count += 1;
                            // The first normal request/close completed before CEF starts.
                            assert_eq!(mock.recorded_requests().iter().filter(|r| r.path == "/v1/chat/completions").count(), 2);
                            if initialize_ok { Ok(()) } else { Err("CEF initialization failed".into()) }
                        });
                    }
                });
            },
            event = events.recv() => {
                match event.unwrap().kind {
                    EventKind::Provider(ProviderEvent::RequestCompleted {
                        input_tokens, cache_read_tokens, run_id: Some(id), ..
                    }) if id == run.to_string() => usage.push((input_tokens, cache_read_tokens)),
                    EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to, .. })
                        if run_id == run.to_string() && matches!(to, AgentRunPhase::Done | AgentRunPhase::Error) => {
                            assert_eq!(to, AgentRunPhase::Done);
                            break;
                        },
                    _ => {}
                }
            }
        }
    }
    runtime.wait(run).await.unwrap();
    assert_eq!(
        initialization_count, 1,
        "later opens must reuse success/error without GUI work"
    );
    assert_eq!(mock.remaining_scripts(), 0);
    let wire: Vec<_> = mock
        .recorded_requests()
        .into_iter()
        .filter(|r| r.path == "/v1/chat/completions")
        .map(|r| r.body)
        .collect();
    assert_eq!(wire.len(), 5);
    assert_eq!(usage.len(), wire.len());
    assert_eq!(usage[0].1, 0, "input-derived cache starts cold");
    assert!(
        wire[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["function"]["name"] == "inspect_artifact")
    );
    for (index, pair) in wire.windows(2).enumerate() {
        assert_append_only(CacheProtocol::OpenAi, &pair[0], &pair[1]).unwrap();
        assert_eq!(
            pair[0]["tools"], pair[1]["tools"],
            "tool schemas AND order stay fixed"
        );
        assert!(
            usage[index + 1].1 >= usage[index].0,
            "the mock must reuse every byte processed before initialization/normal turns"
        );
    }
    for (request, call) in [(&wire[2], "call-1"), (&wire[4], "call-3")] {
        let result = request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "tool" && message["tool_call_id"] == call)
            .unwrap();
        let text = result["content"].as_str().unwrap();
        assert!(
            text.contains(if initialize_ok {
                "artifact ready"
            } else {
                "CEF initialization failed"
            }),
            "{text}"
        );
    }
}

#[tokio::test]
async fn lazy_initialization_and_later_opens_preserve_the_wire_prefix() {
    cache_contract(true).await;
}

#[tokio::test]
async fn initialization_errors_and_normal_turns_preserve_the_wire_prefix() {
    cache_contract(false).await;
}
