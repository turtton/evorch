//! In-app HTML artifact previews on CEF off-screen rendering (ADR 0034 B).
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use browser_cef::offline::ArtifactBundle;
use browser_cef::{
    ArtifactContext, CefRuntime, CefView, NetLog, OsrBrowser, RuntimeConfig, render_snapshot,
};

use crate::model::artifact_opener::ArtifactOpener;
use crate::model::html_snapshots::{HtmlSnapshots, SnapshotWorker, read_html};

mod agent_pages;
pub use agent_pages::CefArtifactPages;

const DEFAULT_SIZE: [f32; 2] = [960.0, 640.0];
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(15);
const PAGE_TIMEOUT: Duration = Duration::from_secs(30);

/// Preview windows for the paths [`crate::model::artifact_opener::HtmlPreviewOpener`]
/// queues, the renderer for card snapshots and the pages agents inspect
/// artifacts in. CEF starts on the first request or on [`Self::artifact_pages`];
/// if it cannot, the receiver is dropped, every artifact goes to `fallback` and
/// snapshots fail.
pub struct HtmlPreviewWindows {
    previews: Vec<Preview>,
    requests: Option<Receiver<PathBuf>>,
    fallback: Arc<dyn ArtifactOpener>,
    snapshots: Arc<HtmlSnapshots>,
    next_id: u64,
    init_error: Option<String>,
    snapshot_timeout: Duration,
    snapshot_worker: Option<SnapshotWorker>,
    page_timeout: Duration,
    agent_pages: Option<Arc<CefArtifactPages>>,
    // Declared last so it drops after every browser has closed.
    runtime: Option<CefRuntime>,
}

struct Preview {
    id: u64,
    name: String,
    view: CefView,
    log: Arc<NetLog>,
    open: bool,
}

impl HtmlPreviewWindows {
    pub fn new(
        requests: Receiver<PathBuf>,
        fallback: Arc<dyn ArtifactOpener>,
        snapshots: Arc<HtmlSnapshots>,
    ) -> Self {
        Self {
            previews: Vec::new(),
            requests: Some(requests),
            fallback,
            snapshots,
            next_id: 0,
            init_error: None,
            snapshot_timeout: SNAPSHOT_TIMEOUT,
            snapshot_worker: None,
            page_timeout: PAGE_TIMEOUT,
            agent_pages: None,
            runtime: None,
        }
    }

    /// Lets conversation cards request snapshots from this renderer.
    pub fn install_snapshots(&self, ctx: &egui::Context) {
        Arc::clone(&self.snapshots).install(ctx);
    }

    pub fn render(&mut self, ctx: &egui::Context) {
        let requested: Vec<PathBuf> = self
            .requests
            .as_ref()
            .map(|requests| requests.try_iter().collect())
            .unwrap_or_default();
        for path in requested {
            if let Err(error) = self.open(&path, ctx.pixels_per_point()) {
                tracing::warn!(%error, path = %path.display(), "HTML preview unavailable");
                self.fallback.open(&path);
            }
        }
        self.render_snapshots(ctx);
        for preview in &mut self.previews {
            preview.show(ctx);
        }
        self.previews.retain(|preview| preview.open);
    }

    fn render_snapshots(&mut self, ctx: &egui::Context) {
        let requests = self.snapshots.take_requests();
        if requests.is_empty() {
            return;
        }
        match self.snapshot_worker(ctx) {
            Ok(worker) => requests
                .into_iter()
                .for_each(|request| worker.submit(request)),
            Err(error) => {
                for request in requests {
                    self.snapshots.finish(&request.sha256, Err(error.clone()));
                }
            }
        }
    }

    fn snapshot_worker(&mut self, ctx: &egui::Context) -> Result<&SnapshotWorker, String> {
        self.ensure_runtime()?;
        if self.snapshot_worker.is_none() {
            let timeout = self.snapshot_timeout;
            let worker =
                SnapshotWorker::spawn(Arc::clone(&self.snapshots), ctx.clone(), move |html| {
                    render_snapshot(html, timeout)
                })?;
            self.snapshot_worker = Some(worker);
        }
        self.snapshot_worker
            .as_ref()
            .ok_or_else(|| "the snapshot worker did not start".into())
    }

    /// Starts CEF and returns the pages agents inspect artifacts in. Call it on
    /// the thread that drops this value: CEF shuts down where it started.
    pub fn artifact_pages(&mut self) -> Result<Arc<CefArtifactPages>, String> {
        self.ensure_runtime()?;
        let timeout = self.page_timeout;
        Ok(Arc::clone(self.agent_pages.get_or_insert_with(|| {
            Arc::new(CefArtifactPages::new(timeout))
        })))
    }

    fn ensure_runtime(&mut self) -> Result<(), String> {
        if self.runtime.is_some() {
            return Ok(());
        }
        if let Some(error) = &self.init_error {
            return Err(error.clone());
        }
        match CefRuntime::initialize(RuntimeConfig::offline()) {
            Ok(runtime) => {
                self.runtime = Some(runtime);
                Ok(())
            }
            Err(error) => {
                self.requests = None;
                self.init_error = Some(error.clone());
                Err(error)
            }
        }
    }

    fn open(&mut self, path: &Path, pixels_per_point: f32) -> Result<(), String> {
        let html = read_html(path)?;
        self.ensure_runtime()?;
        let context = ArtifactContext::create(ArtifactBundle::html(html))?;
        let log = Arc::clone(&context.log);
        let size = DEFAULT_SIZE.map(|value| value as i32);
        let browser = OsrBrowser::create(
            context,
            &ArtifactContext::url("index.html"),
            size,
            pixels_per_point,
        )?;
        self.next_id += 1;
        self.previews.push(Preview {
            id: self.next_id,
            name: path.file_name().map_or_else(
                || path.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            ),
            view: CefView::new(browser),
            log,
            open: true,
        });
        Ok(())
    }
}

impl Drop for HtmlPreviewWindows {
    fn drop(&mut self) {
        if let Some(pages) = &self.agent_pages {
            pages.shutdown();
        }
    }
}

impl Preview {
    fn show(&mut self, ctx: &egui::Context) {
        let (title, crash, blocked) = {
            let status = self.view.browser().status();
            let title = if status.title.is_empty() {
                self.name.clone()
            } else {
                format!("{} — {}", status.title, self.name)
            };
            let blocked = status.blocked_navigations.len()
                + status.blocked_popups.len()
                + self
                    .log
                    .blocked
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .len();
            (title, status.last_crash.clone(), blocked)
        };
        let view = &mut self.view;
        egui::Window::new(title)
            .id(egui::Id::new(("html-preview", self.id)))
            .open(&mut self.open)
            .default_size(DEFAULT_SIZE)
            .resizable(true)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("Reload").clicked() {
                        view.browser().reload();
                    }
                    ui.label("Offline preview");
                    if blocked > 0 {
                        ui.weak(format!("{blocked} network requests blocked"));
                    }
                    if let Some(crash) = crash {
                        ui.colored_label(
                            ui.visuals().warn_fg_color,
                            format!("Renderer crashed ({crash})"),
                        );
                    }
                });
                view.ui(ui);
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::artifact_opener::{HtmlPreviewOpener, PortalArtifactOpener};
    use crate::model::html_snapshots::SnapshotState;

    #[test]
    #[ignore = "needs the CEF runtime and EVORCH_CEF_HELPER; see docs/browser.md"]
    fn card_snapshot_requests_render_through_cef() {
        let dir = tempfile::tempdir().unwrap();
        let html = dir.path().join("mock.html");
        std::fs::write(&html, "<!doctype html><h1>mock</h1>").unwrap();
        let sha256 = "cd".repeat(32);
        let snapshots = Arc::new(HtmlSnapshots::new(dir.path().join("snapshots")));
        let (_opener, requests) = HtmlPreviewOpener::new(Arc::new(PortalArtifactOpener));
        let mut windows = HtmlPreviewWindows::new(
            requests,
            Arc::new(PortalArtifactOpener),
            Arc::clone(&snapshots),
        );
        // nextest's slow-timeout bounds the render; it has no other deadline.
        windows.snapshot_timeout = Duration::MAX;
        assert_eq!(snapshots.state(&sha256, &html), SnapshotState::Pending);

        windows.render(&egui::Context::default());
        // Joins the snapshot worker before CEF shuts down.
        drop(windows);

        let SnapshotState::Ready(png) = snapshots.state(&sha256, &html) else {
            panic!("{:?}", snapshots.state(&sha256, &html));
        };
        let image = image::open(png).unwrap();
        assert_eq!(
            (image.width(), image.height()),
            (
                browser_cef::SNAPSHOT_SIZE[0] as u32,
                browser_cef::SNAPSHOT_SIZE[1] as u32
            )
        );
    }

    const COUNTER: &str = r#"<!doctype html><title>Counter</title>
<h1 id="count">Count 0</h1>
<button id="add">Add</button>
<label>Name <input id="name"></label>
<p id="greeting">Hello</p>
<img src="http://127.0.0.1:{port}/logo.png" alt="logo">
<script>
let count = 0;
document.getElementById("add").addEventListener("click", () => {
  document.getElementById("count").textContent = "Count " + ++count;
});
document.getElementById("name").addEventListener("input", (event) => {
  document.getElementById("greeting").textContent = "Hello " + event.target.value;
});
</script>"#;

    type Results = [(bool, String)];
    type Step = Box<dyn Fn(&Results) -> Vec<(&'static str, serde_json::Value)> + Send + Sync>;

    /// Plays one tool call per turn, built from the tool results so far.
    struct Script {
        steps: std::sync::Mutex<std::collections::VecDeque<Step>>,
        results: std::sync::Mutex<Vec<(bool, String)>>,
    }

    #[async_trait::async_trait]
    impl runtime::AgentModel for Script {
        fn selected_model(&self, _: runtime::Role, _: Option<&str>) -> String {
            "script".into()
        }

        async fn complete(
            &self,
            _: &runtime::AgentInvocationContext,
            _: runtime::Role,
            messages: &[providers::Message],
            _: &[providers::ToolSpec],
        ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
            use providers::{ContentBlock, ToolResultContent};
            let results: Vec<(bool, String)> = messages
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
                .collect();
            let step = self.steps.lock().unwrap().pop_front();
            let calls = step.map(|step| step(&results)).unwrap_or_default();
            *self.results.lock().unwrap() = results;
            let finished = calls.is_empty();
            let content = if finished {
                vec![ContentBlock::Text {
                    text: "Checked".into(),
                }]
            } else {
                calls
                    .into_iter()
                    .enumerate()
                    .map(|(index, (name, input))| ContentBlock::ToolUse {
                        id: format!("call-{}-{index}", messages.len()),
                        name: name.into(),
                        input,
                    })
                    .collect()
            };
            Ok(providers::ChatResponse {
                message: providers::Message {
                    role: providers::Role::Assistant,
                    content,
                },
                finish_reason: if finished {
                    providers::FinishReason::Stop
                } else {
                    providers::FinishReason::ToolUse
                },
                usage: providers::Usage::default(),
            })
        }
    }

    fn reference(outline: &str, element: &str) -> String {
        let line = outline
            .lines()
            .find(|line| line.contains(element))
            .unwrap_or_else(|| panic!("no {element} in\n{outline}"));
        let start = line.find("[ref=").expect("a ref") + "[ref=".len();
        line[start..].split(']').next().unwrap().to_owned()
    }

    fn inspect(input: serde_json::Value) -> Step {
        Box::new(move |_| vec![("inspect_artifact", input.clone())])
    }

    #[test]
    #[ignore = "needs the CEF runtime and EVORCH_CEF_HELPER; see docs/browser.md"]
    fn agent_operates_the_artifact_it_presented_through_cef() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port().to_string();
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        std::thread::spawn(move || {
            for _ in listener.incoming() {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });
        std::fs::write(
            workspace.join("counter.html"),
            COUNTER.replace("{port}", &port),
        )
        .unwrap();
        let (_opener, requests) = HtmlPreviewOpener::new(Arc::new(PortalArtifactOpener));
        let mut windows = HtmlPreviewWindows::new(
            requests,
            Arc::new(PortalArtifactOpener),
            Arc::new(HtmlSnapshots::new(dir.path().join("snapshots"))),
        );
        // nextest's slow-timeout bounds the page; it has no other deadline.
        windows.page_timeout = Duration::MAX;
        let pages = windows.artifact_pages().unwrap();

        let artifact = |results: &Results| -> serde_json::Value {
            serde_json::from_str::<serde_json::Value>(&results[0].1).unwrap()["artifact_id"].clone()
        };
        let steps: Vec<Step> = vec![
            Box::new(|_| {
                vec![(
                    "render_artifact",
                    serde_json::json!({"path": "counter.html", "title": "Counter"}),
                )]
            }),
            Box::new(move |results| {
                vec![(
                    "present",
                    serde_json::json!({"artifact_ids": [artifact(results)]}),
                )]
            }),
            Box::new(move |results| {
                vec![(
                    "inspect_artifact",
                    serde_json::json!({"action": "open", "artifact_id": artifact(results)}),
                )]
            }),
            Box::new(|results| {
                let add = reference(&results[2].1, "button \"Add\"");
                vec![(
                    "inspect_artifact",
                    serde_json::json!({"action": "click", "ref": add}),
                )]
            }),
            Box::new(|results| {
                let name = reference(&results[3].1, "textbox \"Name\"");
                vec![(
                    "inspect_artifact",
                    serde_json::json!({"action": "type", "ref": name, "text": "Ada"}),
                )]
            }),
            inspect(serde_json::json!({"action": "screenshot"})),
        ];
        let script = Arc::new(Script {
            steps: std::sync::Mutex::new(steps.into()),
            results: std::sync::Mutex::default(),
        });

        let tokio = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let phase = tokio.block_on(async {
            let bus = Arc::new(event_bus::EventBus::new(1024));
            let executor = tools::ToolExecutor::new(Arc::clone(&bus));
            executor.set_default_cwd(workspace.clone());
            let store =
                runtime::artifacts::ArtifactStore::new(dir.path().join("artifacts")).unwrap();
            let agents = runtime::AgentRuntime::new(bus, Arc::new(executor), script.clone())
                .with_artifact_store(Arc::new(store));
            agents.set_artifact_pages(pages);
            let run = agents.reserve_run_id();
            agents.bind_thread_root("thread", run).unwrap();
            agents.spawn_reserved(
                run,
                None,
                runtime::Role::Worker,
                "Check the counter mock",
                runtime::RunConfig {
                    conversation: true,
                    category: Some(config::agent_categories::CategoryId::Conversation.to_string()),
                    ..Default::default()
                },
            );
            agents.wait(run).await.unwrap()
        });
        drop(tokio);
        drop(windows);

        assert_eq!(phase, runtime::AgentRunPhase::Done);
        let results = script.results.lock().unwrap().clone();
        assert_eq!(results.len(), 6, "{results:#?}");
        assert!(results.iter().all(|(error, _)| !error), "{results:#?}");
        let (opened, clicked, typed, screenshot) =
            (&results[2].1, &results[3].1, &results[4].1, &results[5].1);
        assert!(
            opened.starts_with("URL: https://artifact.evorch.invalid/index.html"),
            "{opened}"
        );
        assert!(opened.contains("Count 0"), "{opened}");
        assert!(clicked.contains("Count 1"), "{clicked}");
        assert!(typed.contains("Hello Ada"), "{typed}");
        assert!(screenshot.contains("[image image/png]"), "{screenshot}");
        assert!(screenshot.contains("1024x768"), "{screenshot}");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
}
