use std::sync::Arc;

use ::browser::agent::{Activity, AgentBrowser, AgentError, Screencast};
use tokio::sync::{oneshot, watch};

use crate::panes::agent_browser::{AgentBrowserSurface, BrowserTabView};
use crate::theme::{text::muted, tokens::palette, widgets::pane_root};

const OVERLAY_LINES: usize = 6;
/// Last frames kept, one per recently shown run, so switching back or a
/// closed page still shows something; each is up to 1280x720.
const KEPT_FRAMES: usize = 8;

/// What [`AgentBrowserPane`] reads. [`LiveAgentView`] wraps the runtime's
/// [`AgentBrowser`]; tests use a scripted fake.
pub trait AgentView {
    fn pages(&mut self, ctx: &egui::Context) -> Vec<String>;
    /// Screencasts `key`'s page from now on, or nothing with `None`.
    fn observe(&mut self, key: Option<&str>, ctx: &egui::Context);
    fn poll_frame(&mut self) -> Option<egui::ColorImage>;
    /// The run's tab the screencast shows (its active tab).
    fn tab(&self) -> Option<String>;
    fn activity(&self) -> Vec<Activity>;
    fn error(&self) -> Option<String>;
}

pub struct LiveAgentView {
    browser: Arc<AgentBrowser>,
    runtime: tokio::runtime::Handle,
    pages: watch::Receiver<Vec<String>>,
    repaint_on_page_change: bool,
    pending: Option<oneshot::Receiver<Result<Screencast, AgentError>>>,
    stream: Option<Screencast>,
    watching: Option<(String, egui::Context)>,
    error: Option<String>,
}

impl LiveAgentView {
    pub fn new(browser: Arc<AgentBrowser>, runtime: tokio::runtime::Handle) -> Self {
        Self {
            pages: browser.pages(),
            browser,
            runtime,
            repaint_on_page_change: false,
            pending: None,
            stream: None,
            watching: None,
            error: None,
        }
    }
}

impl AgentView for LiveAgentView {
    fn pages(&mut self, ctx: &egui::Context) -> Vec<String> {
        if !self.repaint_on_page_change {
            self.repaint_on_page_change = true;
            let mut changes = self.browser.pages();
            let ctx = ctx.clone();
            self.runtime.spawn(async move {
                while changes.changed().await.is_ok() {
                    crate::repaint::coalesced(&ctx);
                }
            });
        }
        self.pages.borrow().clone()
    }

    fn observe(&mut self, key: Option<&str>, ctx: &egui::Context) {
        self.pending = None;
        self.stream = None;
        self.error = None;
        self.watching = key.map(|key| (key.to_owned(), ctx.clone()));
        let Some(key) = key else {
            return;
        };
        let (sender, receiver) = oneshot::channel();
        let browser = Arc::clone(&self.browser);
        let key = key.to_owned();
        let repaint = ctx.clone();
        self.runtime.spawn(async move {
            let on_frame = {
                let repaint = repaint.clone();
                Arc::new(move || crate::repaint::coalesced(&repaint))
            };
            let _ = sender.send(browser.screencast(&key, on_frame).await);
            // A start that fails never produces a frame, and one that succeeds may not produce
            // one for a while, yet either result is only taken in by the next poll_frame.
            crate::repaint::coalesced(&repaint);
        });
        self.pending = Some(receiver);
    }

    fn poll_frame(&mut self) -> Option<egui::ColorImage> {
        // A run whose page closed and reopened under the same key keeps its
        // place in the list, so follow the new page.
        if self.stream.as_ref().is_some_and(Screencast::ended)
            && let Some((key, ctx)) = self.watching.clone()
            && self.pages.borrow().contains(&key)
        {
            self.observe(Some(&key), &ctx);
        }
        if let Some(pending) = &mut self.pending
            && let Ok(started) = pending.try_recv()
        {
            self.pending = None;
            match started {
                Ok(stream) => self.stream = Some(stream),
                Err(error) => self.error = Some(error.to_string()),
            }
        }
        let frame = self.stream.as_mut()?.poll_frame()?;
        Some(egui::ColorImage::from_rgba_unmultiplied(
            [frame.width() as usize, frame.height() as usize],
            frame.as_raw(),
        ))
    }

    fn tab(&self) -> Option<String> {
        self.stream.as_ref().map(Screencast::tab)
    }

    fn activity(&self) -> Vec<Activity> {
        self.stream
            .as_ref()
            .map(|stream| stream.agent().activity())
            .unwrap_or_default()
    }

    fn error(&self) -> Option<String> {
        self.error.clone()
    }
}

/// Watches one agent page at a time: its screencast with the latest actions
/// and refused requests laid over it. Read-only; nothing here drives the page.
/// The workbench picks the run (see [`AgentBrowserSurface`]); a run whose page
/// closed keeps its last frame.
pub struct AgentBrowserPane<V> {
    view: V,
    /// The run whose page is screencast, if any.
    observing: Option<String>,
    /// The last frame of each recently shown run, oldest first.
    frames: std::collections::VecDeque<(String, egui::TextureHandle)>,
}

impl<V: AgentView> AgentBrowserPane<V> {
    pub fn new(view: V) -> Self {
        Self {
            view,
            observing: None,
            frames: std::collections::VecDeque::new(),
        }
    }

    pub fn view(&self) -> &V {
        &self.view
    }

    pub fn view_mut(&mut self) -> &mut V {
        &mut self.view
    }

    /// The run being screencast.
    pub fn observing(&self) -> Option<&str> {
        self.observing.as_deref()
    }

    /// The size of the last frame kept for `run`.
    pub fn texture_size(&self, run: &str) -> Option<[usize; 2]> {
        self.frame(run).map(egui::TextureHandle::size)
    }

    fn frame(&self, run: &str) -> Option<&egui::TextureHandle> {
        self.frames
            .iter()
            .find(|(key, _)| key == run)
            .map(|(_, texture)| texture)
    }

    fn observe(&mut self, run: Option<&str>, ctx: &egui::Context) {
        if self.observing.as_deref() != run {
            self.view.observe(run, ctx);
            self.observing = run.map(str::to_owned);
        }
    }

    fn keep_frame(&mut self, run: &str, image: egui::ColorImage, ctx: &egui::Context) {
        if let Some((_, texture)) = self.frames.iter_mut().find(|(key, _)| key == run) {
            texture.set(image, egui::TextureOptions::LINEAR);
            return;
        }
        if self.frames.len() == KEPT_FRAMES {
            self.frames.pop_front();
        }
        let texture = ctx.load_texture(
            format!("agent-browser-{run}"),
            image,
            egui::TextureOptions::LINEAR,
        );
        self.frames.push_back((run.to_owned(), texture));
    }
}

impl<V: AgentView> AgentBrowserSurface for AgentBrowserPane<V> {
    fn pages(&mut self, ctx: &egui::Context) -> Vec<String> {
        self.view.pages(ctx)
    }

    fn render(&mut self, ui: &mut egui::Ui, tab: BrowserTabView<'_>) -> Option<String> {
        let ctx = ui.ctx().clone();
        let mut picked = None;
        pane_root(ui, "Agent browser", |ui| {
            let Some(selected) = tab.selected else {
                self.observe(None, &ctx);
                ui.label(muted(
                    "No agent page is open in this conversation. A page appears here when an agent uses the browser tools.",
                ));
                return;
            };
            let open = self.view.pages(&ctx).iter().any(|run| run == selected);
            if open {
                self.observe(Some(selected), &ctx);
            } else if self.observing.as_deref() != Some(selected) {
                // A run that is still observed keeps its ended screencast, and
                // with it the activity of its last page.
                self.observe(None, &ctx);
            }
            if tab.runs.len() > 1 {
                let label = |run: &str| {
                    tab.runs
                        .iter()
                        .find(|entry| entry.run_id == run)
                        .map_or_else(|| run.to_owned(), |entry| entry.label.clone())
                };
                egui::ComboBox::from_label("Run")
                    .selected_text(label(selected))
                    .show_ui(ui, |ui| {
                        for run in tab.runs {
                            if ui
                                .selectable_label(run.run_id == selected, &run.label)
                                .clicked()
                                && run.run_id != selected
                            {
                                picked = Some(run.run_id.clone());
                            }
                        }
                    });
            }
            let watched = self.observing.as_deref() == Some(selected);
            // Before anything below reads the view: this takes in a screencast that just started
            // or failed to, so the pass a wake-up triggered already shows it.
            if watched && let Some(image) = self.view.poll_frame() {
                self.keep_frame(selected, image, &ctx);
            }
            if open {
                if let Some(tab) = self.view.tab() {
                    ui.label(muted(format!("Showing the active tab {tab}")));
                }
                if let Some(error) = self.view.error() {
                    crate::theme::widgets::reason_label(ui, palette().ERROR_FG, error);
                }
            } else {
                ui.label(muted("The page was closed."));
            }
            let activity = if watched {
                self.view.activity()
            } else {
                Vec::new()
            };
            let recent = &activity[activity.len().saturating_sub(OVERLAY_LINES)..];
            match self.frame(selected) {
                Some(texture) => {
                    let image = ui.add(
                        egui::Image::new(texture)
                            .max_size(ui.available_size())
                            .maintain_aspect_ratio(true),
                    );
                    if !recent.is_empty() {
                        overlay(ui, image.rect, recent);
                    }
                }
                None => {
                    if open {
                        ui.label(muted("Waiting for the first frame of this page..."));
                    }
                    activity_lines(ui, recent);
                }
            }
        });
        picked
    }

    fn stop(&mut self, ctx: &egui::Context) {
        self.observe(None, ctx);
    }
}

fn overlay(ui: &mut egui::Ui, image: egui::Rect, recent: &[Activity]) {
    let height = (recent.len() as f32 * 18.0 + 12.0).min(image.height());
    let area = egui::Rect::from_min_max(
        egui::pos2(image.left(), image.bottom() - height),
        image.right_bottom(),
    );
    ui.painter()
        .rect_filled(area, 0.0, egui::Color32::from_black_alpha(170));
    ui.scope_builder(egui::UiBuilder::new().max_rect(area.shrink(6.0)), |ui| {
        activity_lines(ui, recent);
    });
}

fn activity_lines(ui: &mut egui::Ui, recent: &[Activity]) {
    for entry in recent {
        let (text, failed) = match entry {
            Activity::Action { label, error: None } => (label.clone(), false),
            Activity::Action {
                label,
                error: Some(error),
            } => (format!("{label} failed: {error}"), true),
            Activity::Blocked(request) => (format!("blocked {request}"), true),
            Activity::Dialog(dialog) => (format!("dialog {dialog}"), false),
        };
        let color = if failed {
            palette().ERROR_FG
        } else {
            egui::Color32::WHITE
        };
        ui.add(egui::Label::new(egui::RichText::new(text).color(color).monospace()).truncate());
    }
}

#[derive(Default)]
pub struct FakeAgentView {
    pub pages: Vec<String>,
    pub frames: std::collections::VecDeque<egui::ColorImage>,
    pub activity: Vec<Activity>,
    pub failure: Option<String>,
    pub tab: Option<String>,
    pub observed: Vec<Option<String>>,
}

impl AgentView for FakeAgentView {
    fn pages(&mut self, _ctx: &egui::Context) -> Vec<String> {
        self.pages.clone()
    }

    fn observe(&mut self, key: Option<&str>, _ctx: &egui::Context) {
        self.observed.push(key.map(str::to_owned));
    }

    fn poll_frame(&mut self) -> Option<egui::ColorImage> {
        self.frames.pop_front()
    }

    fn tab(&self) -> Option<String> {
        self.tab.clone()
    }

    fn activity(&self) -> Vec<Activity> {
        self.activity.clone()
    }

    fn error(&self) -> Option<String> {
        self.failure.clone()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use ::browser::agent::{LaunchOptions, RequestPolicy};
    use egui_kittest::kittest::Queryable as _;

    use super::*;

    struct NoPolicy;

    #[async_trait::async_trait]
    impl RequestPolicy for NoPolicy {
        async fn check(&self, _: &url::Url) -> Result<(), String> {
            Err("unused".into())
        }

        async fn resolve(
            &self,
            _: &str,
            _: u16,
            _: bool,
        ) -> Result<Vec<std::net::SocketAddr>, String> {
            Err("unused".into())
        }
    }

    #[test]
    fn a_screencast_that_fails_to_start_wakes_the_ui_to_show_the_error() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let browser = Arc::new(AgentBrowser::new(LaunchOptions {
            executable: None,
            policy: Arc::new(NoPolicy),
            max_pages: 1,
            max_tabs: 1,
        }));
        let mut view = LiveAgentView::new(browser, runtime.handle().clone());
        let ctx = egui::Context::default();
        let (woken, wake) = mpsc::channel();
        ctx.set_request_repaint_callback(move |_| {
            let _ = woken.send(());
        });

        // No tab is open for this run, so starting the screencast fails without a browser.
        view.observe(Some("run-without-a-page"), &ctx);
        wake.recv().expect("the failed start requests a repaint");

        assert!(view.poll_frame().is_none());
        assert!(
            view.error()
                .is_some_and(|error| error.contains("run-without-a-page"))
        );
    }

    struct Deferred {
        failure: mpsc::Receiver<String>,
        error: Option<String>,
    }

    impl AgentView for Deferred {
        fn pages(&mut self, _: &egui::Context) -> Vec<String> {
            vec!["run-1".into()]
        }

        fn observe(&mut self, _: Option<&str>, _: &egui::Context) {}

        fn poll_frame(&mut self) -> Option<egui::ColorImage> {
            if let Ok(error) = self.failure.try_recv() {
                self.error = Some(error);
            }
            None
        }

        fn tab(&self) -> Option<String> {
            None
        }

        fn activity(&self) -> Vec<Activity> {
            Vec::new()
        }

        fn error(&self) -> Option<String> {
            self.error.clone()
        }
    }

    #[test]
    fn the_pass_a_failed_start_wakes_already_shows_the_error() {
        let (fail, failure) = mpsc::channel();
        let pane = AgentBrowserPane::new(Deferred {
            failure,
            error: None,
        });
        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, pane: &mut AgentBrowserPane<Deferred>| {
                let runs = [];
                AgentBrowserSurface::render(
                    pane,
                    ui,
                    BrowserTabView {
                        runs: &runs,
                        selected: Some("run-1"),
                    },
                );
            },
            pane,
        );
        harness.run();
        assert!(
            harness
                .query_by_label_contains("screencast could not start")
                .is_none()
        );

        // The window is at rest; only the start failing wakes it.
        let (woken, wake) = mpsc::channel();
        harness.ctx.set_request_repaint_callback(move |_| {
            let _ = woken.send(());
        });
        fail.send("screencast could not start".into()).unwrap();
        crate::repaint::coalesced(&harness.ctx);
        wake.recv().expect("the failure requests a repaint");

        // That one pass, with nothing else in between, must draw it.
        harness.step();
        assert!(
            harness
                .query_by_label_contains("screencast could not start")
                .is_some()
        );
    }
}
