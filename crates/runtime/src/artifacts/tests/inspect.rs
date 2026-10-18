use std::sync::atomic::{AtomicBool, Ordering};

use browser::agent::{AgentError, CdpTarget, PageAgent};

use super::*;
use crate::artifacts::{ArtifactPage, ArtifactPages};

/// 1×1 PNG, as `Page.captureScreenshot` returns it.
const SCREENSHOT: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

/// A page whose button reads "Add" until it is clicked.
#[derive(Default)]
struct Target {
    clicked: AtomicBool,
}

#[async_trait::async_trait]
impl CdpTarget for Target {
    async fn call(&self, method: &str, _: Value) -> Result<Value, AgentError> {
        if method == "Input.dispatchMouseEvent" {
            self.clicked.store(true, Ordering::SeqCst);
        }
        let label = if self.clicked.load(Ordering::SeqCst) {
            "Added"
        } else {
            "Add"
        };
        Ok(match method {
            "Accessibility.getFullAXTree" => json!({"nodes": [{
                "nodeId": "1", "role": {"value": "button"}, "name": {"value": label},
                "backendDOMNodeId": 7
            }]}),
            "DOM.getContentQuads" => json!({"quads": [[10, 20, 30, 20, 30, 40, 10, 40]]}),
            "Page.captureScreenshot" => json!({"data": SCREENSHOT}),
            _ => json!({}),
        })
    }

    async fn navigate(&self, _: &str) -> Result<(), AgentError> {
        Err(AgentError::Navigation(
            "artifact pages do not navigate".into(),
        ))
    }
}

struct Page {
    agent: PageAgent,
    blocked: Mutex<Vec<String>>,
}

impl ArtifactPage for Page {
    fn agent(&self) -> &PageAgent {
        &self.agent
    }

    fn take_blocked(&self) -> Vec<String> {
        std::mem::take(&mut self.blocked.lock().unwrap())
    }
}

#[derive(Default)]
struct Pages {
    open: Mutex<HashMap<String, Arc<Page>>>,
    opened: Mutex<Vec<(String, Vec<u8>)>>,
    closed: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl ArtifactPages for Pages {
    async fn open(&self, key: &str, html: Vec<u8>) -> Result<Arc<dyn ArtifactPage>, String> {
        self.opened.lock().unwrap().push((key.into(), html));
        let page = Arc::new(Page {
            agent: PageAgent::new(Arc::new(Target::default())),
            blocked: Mutex::new(vec!["request https://cdn.example/app.js".into()]),
        });
        self.open
            .lock()
            .unwrap()
            .insert(key.into(), Arc::clone(&page));
        Ok(page)
    }

    fn page(&self, key: &str) -> Option<Arc<dyn ArtifactPage>> {
        self.open
            .lock()
            .unwrap()
            .get(key)
            .cloned()
            .map(|page| page as Arc<dyn ArtifactPage>)
    }

    fn close(&self, key: &str) -> bool {
        self.closed.lock().unwrap().push(key.into());
        self.open.lock().unwrap().remove(key).is_some()
    }
}

fn with_pages() -> (Harness, Arc<Pages>) {
    let harness = Harness::new();
    let pages = Arc::new(Pages::default());
    harness
        .runtime
        .set_artifact_pages(Arc::clone(&pages) as Arc<dyn ArtifactPages>);
    (harness, pages)
}

fn inspect(input: Value) -> Step {
    calls(vec![("inspect_artifact", input)])
}

fn open_last_capture() -> Step {
    Step::Calls(Box::new(|messages| {
        vec![(
            "inspect_artifact",
            json!({"action": "open", "artifact_id": last_artifact_id(messages)}),
        )]
    }))
}

#[tokio::test]
async fn conversation_root_operates_the_mock_it_presented() {
    let (harness, pages) = with_pages();
    let run = harness.root(
        "thread",
        Role::Worker,
        vec![
            render("mock.html", "Counter"),
            Step::Calls(Box::new(|messages| {
                vec![(
                    "present",
                    json!({"artifact_ids": [last_artifact_id(messages)]}),
                )]
            })),
            open_last_capture(),
            inspect(json!({"action": "click", "ref": "e7"})),
            inspect(json!({"action": "screenshot"})),
            inspect(json!({"action": "close"})),
            stop(),
        ],
    );
    assert_eq!(
        harness.runtime.wait(run).await.unwrap(),
        AgentRunPhase::Done
    );

    let results = harness.results(run);
    assert!(results.iter().all(|(error, _)| !error), "{results:?}");
    let (opened, click, screenshot) = (&results[2].1, &results[3].1, &results[4].1);
    assert!(opened.contains("button \"Add\" [ref=e7]"), "{opened}");
    assert!(
        opened.contains("Blocked requests:\n- request https://cdn.example/app.js"),
        "{opened}"
    );
    assert!(click.contains("button \"Added\" [ref=e7]"), "{click}");
    assert!(!click.contains("Blocked requests"), "{click}");
    assert!(screenshot.contains("1x1"), "{screenshot}");
    assert!(screenshot.contains("[image image/png]"), "{screenshot}");
    assert!(results[5].1.contains("closed"));
    assert_eq!(
        pages.opened.lock().unwrap()[0],
        (run.to_string(), b"<!doctype html><p>Mock</p>".to_vec())
    );
    assert!(
        harness.model.tools.lock().unwrap()[&run.to_string()]
            .contains(&"inspect_artifact".to_owned())
    );
}

#[tokio::test]
async fn visual_worker_checks_its_capture_and_its_page_closes_with_the_run() {
    let (harness, pages) = with_pages();
    let (started, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let root = harness.root(
        "thread",
        Role::Orchestrator,
        vec![
            Step::HoldThen(started.clone(), release.clone(), Box::new(|_| vec![])),
            stop(),
        ],
    );
    started.notified().await;
    let child = harness.visual_child(
        root,
        vec![
            render("mock.html", "Settings"),
            open_last_capture(),
            inspect(json!({"action": "click", "ref": "e8"})),
            stop(),
        ],
    );
    assert_eq!(
        harness.runtime.wait(child).await.unwrap(),
        AgentRunPhase::Done
    );
    release.notify_one();
    harness.runtime.wait(root).await.unwrap();

    let results = harness.results(child);
    assert!(!results[1].0, "{results:?}");
    assert!(
        results[2].0 && results[2].1.contains("take a new snapshot"),
        "{results:?}"
    );
    assert!(pages.closed.lock().unwrap().contains(&child.to_string()));
    assert!(pages.open.lock().unwrap().is_empty());
}

#[tokio::test]
async fn inspect_refuses_images_foreign_artifacts_and_missing_pages() {
    let (harness, pages) = with_pages();
    let store = harness.runtime.artifact_store().unwrap();
    let foreign = store
        .capture(
            harness.workspace.path(),
            Path::new("mock.html"),
            CaptureRequest {
                title: "Foreign",
                caption: None,
                run_id: "other".into(),
                root_run_id: "other".into(),
                thread_id: Some("other-thread".into()),
            },
        )
        .unwrap()
        .artifact_id;
    let run = harness.root(
        "thread",
        Role::Worker,
        vec![
            inspect(json!({"action": "snapshot"})),
            inspect(json!({"action": "open", "artifact_id": foreign})),
            render("mock.png", "Image"),
            open_last_capture(),
            inspect(json!({"action": "open"})),
            stop(),
        ],
    );
    assert_eq!(
        harness.runtime.wait(run).await.unwrap(),
        AgentRunPhase::Done
    );

    let results = harness.results(run);
    assert!(results[0].0 && results[0].1.contains("no artifact page is open"));
    assert!(results[1].0 && results[1].1.contains("does not belong"));
    assert!(
        results[3].0 && results[3].1.contains("only HTML"),
        "{results:?}"
    );
    assert!(results[4].0 && results[4].1.contains("missing artifact_id"));
    assert!(pages.opened.lock().unwrap().is_empty());
}

#[tokio::test]
async fn without_pages_the_tool_is_not_offered_and_fails_closed() {
    let harness = Harness::new();
    let run = harness.root(
        "thread",
        Role::Worker,
        vec![render("mock.html", "Counter"), open_last_capture(), stop()],
    );
    assert_eq!(
        harness.runtime.wait(run).await.unwrap(),
        AgentRunPhase::Done
    );

    let tools = harness.model.tools.lock().unwrap()[&run.to_string()].clone();
    assert!(tools.contains(&"present".to_owned()), "{tools:?}");
    assert!(!tools.contains(&"inspect_artifact".to_owned()), "{tools:?}");
    let results = harness.results(run);
    assert!(results[1].0, "{results:?}");
}
