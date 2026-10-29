//! In-app HTML artifact previews on CEF off-screen rendering (ADR 0034 B).
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use browser_cef::offline::ArtifactBundle;
use browser_cef::{
    ArtifactContext, CefRuntime, CefView, Dialogs, NetLog, OsrBrowser, RuntimeConfig,
    render_snapshot,
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
/// artifacts in. CEF starts only on the first preview, snapshot or page request;
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
    runtime_config: RuntimeConfig,
    runtime_requests: Receiver<RuntimeRequest>,
    runtime_request_tx: Sender<RuntimeRequest>,
    // Declared last so it drops after every browser has closed.
    runtime: Option<CefRuntime>,
}

type RuntimeRequest = Sender<Result<(), String>>;

/// Blocking callers ask the GUI thread to initialize CEF, never initialize it
/// themselves. Dropping the GUI's receiver also releases pending callers.
#[derive(Clone)]
pub(super) struct RuntimeRequestSender {
    requests: Sender<RuntimeRequest>,
    ctx: egui::Context,
    timeout: Duration,
}

impl RuntimeRequestSender {
    fn ensure_runtime(&self) -> Result<(), String> {
        let (reply, response) = mpsc::channel();
        self.requests
            .send(reply)
            .map_err(|_| "the GUI is shutting down".to_owned())?;
        // Wake even an idle window: agents must not depend on user input.
        self.ctx.request_repaint();
        response
            .recv_timeout(self.timeout)
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => {
                    "the GUI did not initialize CEF in time".to_owned()
                }
                mpsc::RecvTimeoutError::Disconnected => "the GUI is shutting down".to_owned(),
            })?
    }
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
        let (runtime_request_tx, runtime_requests) = mpsc::channel();
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
            runtime_config: RuntimeConfig::offline(),
            runtime_requests,
            runtime_request_tx,
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
        let requests: Vec<_> = self.runtime_requests.try_iter().collect();
        for reply in requests {
            let _ = reply.send(self.ensure_runtime());
        }
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

    /// Registers pages without starting CEF. Tool availability is decided once,
    /// using only this lightweight helper check; initialization errors are
    /// returned by the first tool call, not by changing the tool schema.
    pub fn artifact_pages(&mut self, ctx: &egui::Context) -> Result<Arc<CefArtifactPages>, String> {
        if !self.runtime_config.helper_path.is_file() {
            return Err(format!(
                "CEF helper not found at {}",
                self.runtime_config.helper_path.display()
            ));
        }
        let timeout = self.page_timeout;
        let initializer = RuntimeRequestSender {
            requests: self.runtime_request_tx.clone(),
            ctx: ctx.clone(),
            timeout,
        };
        Ok(Arc::clone(self.agent_pages.get_or_insert_with(|| {
            Arc::new(CefArtifactPages::new(timeout, initializer))
        })))
    }

    fn ensure_runtime(&mut self) -> Result<(), String> {
        if self.runtime.is_some() {
            return Ok(());
        }
        if let Some(error) = &self.init_error {
            return Err(error.clone());
        }
        match CefRuntime::initialize(self.runtime_config.clone()) {
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
            Dialogs::Ask,
            30,
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
    use runtime::artifacts::ArtifactPages;

    fn idle_context() -> egui::Context {
        let ctx = egui::Context::default();
        for _ in 0..3 {
            ctx.run_ui(Default::default(), |_| {})
                .drop_without_applying_deltas();
        }
        ctx
    }

    fn windows(dir: &Path) -> HtmlPreviewWindows {
        let (_opener, requests) = HtmlPreviewOpener::new(Arc::new(PortalArtifactOpener));
        HtmlPreviewWindows::new(
            requests,
            Arc::new(PortalArtifactOpener),
            Arc::new(HtmlSnapshots::new(dir.join("snapshots"))),
        )
    }

    fn initializer(ctx: egui::Context) -> (RuntimeRequestSender, Receiver<RuntimeRequest>) {
        let (requests, receiver) = mpsc::channel();
        (
            RuntimeRequestSender {
                requests,
                ctx,
                timeout: Duration::MAX,
            },
            receiver,
        )
    }

    #[test]
    fn registration_and_idle_render_do_not_initialize_cef() {
        let dir = tempfile::tempdir().unwrap();
        let mut windows = windows(dir.path());
        let ctx = egui::Context::default();
        windows.runtime_config.helper_path = dir.path().join("helper");
        assert!(windows.artifact_pages(&ctx).is_err());
        // Deliberately not an executable: registration must only check is_file.
        std::fs::write(&windows.runtime_config.helper_path, "").unwrap();
        let pages = windows.artifact_pages(&ctx).unwrap();
        assert!(Arc::ptr_eq(&pages, &windows.artifact_pages(&ctx).unwrap()));
        windows.render(&ctx);
        assert!(windows.runtime.is_none());
        assert!(windows.init_error.is_none());
        assert!(windows.snapshot_worker.is_none());
        drop(windows);
        // The registry can outlive a GUI that never initialized CEF.
        assert!(pages.page("run").is_none());
    }

    #[test]
    fn initialization_requests_wake_gui_and_relay_each_response() {
        let ctx = idle_context();
        let (wake, repaint) = mpsc::channel();
        ctx.set_request_repaint_callback(move |_| {
            let _ = wake.send(());
        });
        let (initializer, requests) = initializer(ctx);
        std::thread::scope(|scope| {
            let first = scope.spawn(|| initializer.ensure_runtime());
            repaint.recv().unwrap();
            let first_reply = requests.recv().unwrap();
            let second = scope.spawn(|| initializer.ensure_runtime());
            requests
                .recv()
                .unwrap()
                .send(Err("initialization failed".into()))
                .unwrap();
            first_reply.send(Ok(())).unwrap();
            assert_eq!(first.join().unwrap(), Ok(()));
            assert_eq!(second.join().unwrap(), Err("initialization failed".into()));
        });
    }

    #[test]
    fn initialization_waiters_are_released_when_gui_shuts_down() {
        let (initializer, requests) = initializer(egui::Context::default());
        std::thread::scope(|scope| {
            let pending = scope.spawn(|| initializer.ensure_runtime());
            let reply = requests.recv().unwrap();
            drop(requests);
            drop(reply);
            assert_eq!(
                pending.join().unwrap(),
                Err("the GUI is shutting down".into())
            );
        });
        assert_eq!(
            initializer.ensure_runtime(),
            Err("the GUI is shutting down".into())
        );
    }

    #[test]
    fn initialization_request_uses_the_page_timeout() {
        let (mut initializer, _requests) = initializer(egui::Context::default());
        // No GUI processes the queued request; a zero budget deterministically
        // exercises the production deadline without a wall-clock test wait.
        initializer.timeout = Duration::ZERO;
        assert_eq!(
            initializer.ensure_runtime(),
            Err("the GUI did not initialize CEF in time".into())
        );
    }

    #[tokio::test]
    async fn page_open_returns_initialization_error_without_opening_cef() {
        let dir = tempfile::tempdir().unwrap();
        let mut windows = windows(dir.path());
        let ctx = idle_context();
        windows.runtime_config.helper_path = dir.path().join("helper");
        std::fs::write(&windows.runtime_config.helper_path, "").unwrap();
        windows.page_timeout = Duration::MAX;
        let pages = windows.artifact_pages(&ctx).unwrap();
        // Missing helpers fail before any CEF API call, even in this CEF build.
        std::fs::remove_file(&windows.runtime_config.helper_path).unwrap();
        let (wake, mut repaint) = tokio::sync::mpsc::unbounded_channel();
        ctx.set_request_repaint_callback(move |_| {
            let _ = wake.send(());
        });
        let opening_pages = Arc::clone(&pages);
        let opening =
            tokio::spawn(async move { opening_pages.open("run", b"<h1>mock</h1>".to_vec()).await });
        repaint.recv().await.unwrap();
        windows.render(&ctx);
        let error = opening.await.unwrap().err().unwrap();
        assert_eq!(Some(&error), windows.init_error.as_ref());
        assert!(pages.page("run").is_none());
        std::fs::write(&windows.runtime_config.helper_path, "").unwrap();
        assert!(Arc::ptr_eq(&pages, &windows.artifact_pages(&ctx).unwrap()));
        assert!(windows.runtime.is_none());
    }

    #[tokio::test]
    async fn pending_page_open_is_released_when_uninitialized_gui_drops() {
        let dir = tempfile::tempdir().unwrap();
        let mut windows = windows(dir.path());
        let ctx = idle_context();
        windows.runtime_config.helper_path = dir.path().join("helper");
        std::fs::write(&windows.runtime_config.helper_path, "").unwrap();
        windows.page_timeout = Duration::MAX;
        let pages = windows.artifact_pages(&ctx).unwrap();
        let (wake, mut repaint) = tokio::sync::mpsc::unbounded_channel();
        ctx.set_request_repaint_callback(move |_| {
            let _ = wake.send(());
        });
        let opening =
            tokio::spawn(async move { pages.open("run", b"<h1>mock</h1>".to_vec()).await });
        repaint.recv().await.unwrap();
        drop(windows);
        assert_eq!(
            opening.await.unwrap().err().unwrap(),
            "the GUI is shutting down"
        );
    }

    #[test]
    #[ignore = "needs the CEF runtime and EVORCH_CEF_HELPER; see docs/browser.md"]
    fn preview_starts_cef_on_first_request() {
        let dir = tempfile::tempdir().unwrap();
        let mut windows = windows(dir.path());
        let ctx = egui::Context::default();
        let _pages = windows.artifact_pages(&ctx).unwrap();
        assert!(windows.runtime.is_none());
        let html = dir.path().join("mock.html");
        std::fs::write(&html, "<!doctype html><h1>mock</h1>").unwrap();
        windows.open(&html, 1.0).unwrap();
        assert!(windows.runtime.is_some());
        assert_eq!(windows.previews.len(), 1);
    }

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
<label>Size <select id="size"><option value="s">Small</option><option value="l">Large</option></select></label>
<p id="chosen">Size none</p>
<button id="info">Info</button>
<p id="hint">Not hovered</p>
<img src="http://127.0.0.1:{port}/logo.png" alt="logo">
<script>
let count = 0;
document.getElementById("add").addEventListener("click", () => {
  document.getElementById("count").textContent = "Count " + ++count;
});
document.getElementById("name").addEventListener("input", (event) => {
  document.getElementById("greeting").textContent = "Hello " + event.target.value;
});
document.getElementById("size").addEventListener("change", (event) => {
  document.getElementById("chosen").textContent = "Size " + event.target.value;
});
document.getElementById("info").addEventListener("mouseover", () => {
  document.getElementById("hint").textContent = "Hovered";
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
        enum GuiEvent {
            Repaint,
            Finished(runtime::AgentRunPhase),
        }
        let ctx = idle_context();
        let (wake, events) = mpsc::channel();
        let repaint = wake.clone();
        ctx.set_request_repaint_callback(move |_| {
            let _ = repaint.send(GuiEvent::Repaint);
        });
        let pages = windows.artifact_pages(&ctx).unwrap();
        assert!(windows.runtime.is_none());

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
            Box::new(|results| {
                let size = reference(&results[4].1, "combobox \"Size\"");
                vec![(
                    "inspect_artifact",
                    serde_json::json!({"action": "select_option", "ref": size, "values": ["Large"]}),
                )]
            }),
            Box::new(|results| {
                let info = reference(&results[5].1, "button \"Info\"");
                vec![(
                    "inspect_artifact",
                    serde_json::json!({"action": "hover", "ref": info}),
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
        let agent_script = Arc::clone(&script);
        tokio.spawn(async move {
            let bus = Arc::new(event_bus::EventBus::new(1024));
            let executor = tools::ToolExecutor::new(Arc::clone(&bus));
            executor.set_default_cwd(workspace.clone());
            let store =
                runtime::artifacts::ArtifactStore::new(dir.path().join("artifacts")).unwrap();
            let agents = runtime::AgentRuntime::new(bus, Arc::new(executor), agent_script)
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
            let phase = agents.wait(run).await.unwrap();
            wake.send(GuiEvent::Finished(phase)).unwrap();
        });
        // The GUI thread services repaint/initialization events while the agent
        // runs; completion is explicit rather than a fixed render-loop timeout.
        let phase = loop {
            match events.recv().unwrap() {
                GuiEvent::Repaint => {
                    ctx.run_ui(Default::default(), |ui| windows.render(ui.ctx()))
                        .drop_without_applying_deltas();
                }
                GuiEvent::Finished(phase) => break phase,
            }
        };
        drop(tokio);
        drop(windows);

        assert_eq!(phase, runtime::AgentRunPhase::Done);
        let results = script.results.lock().unwrap().clone();
        assert_eq!(results.len(), 8, "{results:#?}");
        assert!(results.iter().all(|(error, _)| !error), "{results:#?}");
        let (opened, clicked, typed) = (&results[2].1, &results[3].1, &results[4].1);
        let (selected, hovered, screenshot) = (&results[5].1, &results[6].1, &results[7].1);
        assert!(
            opened.starts_with("URL: https://artifact.evorch.invalid/index.html"),
            "{opened}"
        );
        assert!(opened.contains("Count 0"), "{opened}");
        assert!(clicked.contains("Count 1"), "{clicked}");
        assert!(typed.contains("Hello Ada"), "{typed}");
        assert!(selected.contains("Size l"), "{selected}");
        assert!(hovered.contains("Hovered"), "{hovered}");
        assert!(screenshot.contains("[image image/png]"), "{screenshot}");
        assert!(screenshot.contains("1024x768"), "{screenshot}");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
}
