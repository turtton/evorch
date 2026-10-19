use std::sync::Arc;

use ::browser::agent::{Activity, AgentBrowser, AgentError, Screencast};
use tokio::sync::{oneshot, watch};

use crate::theme::{text::muted, tokens::palette, widgets::pane_root};

const OVERLAY_LINES: usize = 6;

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
                    ctx.request_repaint();
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
            let on_frame = Arc::new(move || repaint.request_repaint());
            let _ = sender.send(browser.screencast(&key, on_frame).await);
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
pub struct AgentBrowserPane<V> {
    view: V,
    selected: Option<String>,
    texture: Option<egui::TextureHandle>,
}

impl<V: AgentView> AgentBrowserPane<V> {
    pub fn new(view: V) -> Self {
        Self {
            view,
            selected: None,
            texture: None,
        }
    }

    pub fn view(&self) -> &V {
        &self.view
    }

    pub fn view_mut(&mut self) -> &mut V {
        &mut self.view
    }

    pub fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    pub fn texture_size(&self) -> Option<[usize; 2]> {
        self.texture.as_ref().map(egui::TextureHandle::size)
    }

    pub fn stop(&mut self, ctx: &egui::Context) {
        if self.selected.take().is_some() {
            self.view.observe(None, ctx);
        }
        self.texture = None;
    }

    pub fn render(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        pane_root(ui, "Agent browser", |ui| {
            let pages = self.view.pages(&ctx);
            let current = self
                .selected
                .clone()
                .filter(|key| pages.contains(key))
                .or_else(|| pages.first().cloned());
            if current != self.selected {
                self.select(current, &ctx);
            }
            let Some(selected) = self.selected.clone() else {
                ui.label(muted(
                    "No agent page is open. A run's page appears here when it uses the browser tools.",
                ));
                return;
            };
            egui::ComboBox::from_label("Run")
                .selected_text(selected.as_str())
                .show_ui(ui, |ui| {
                    for key in &pages {
                        if ui.selectable_label(*key == selected, key).clicked() && *key != selected
                        {
                            self.select(Some(key.clone()), &ctx);
                        }
                    }
                });
            if let Some(tab) = self.view.tab() {
                ui.label(muted(format!("Showing the active tab {tab}")));
            }
            if let Some(error) = self.view.error() {
                ui.colored_label(palette().ERROR_FG, error);
            }
            if let Some(image) = self.view.poll_frame() {
                match &mut self.texture {
                    Some(texture) => texture.set(image, egui::TextureOptions::LINEAR),
                    None => {
                        self.texture = Some(ctx.load_texture(
                            "agent-browser-screencast",
                            image,
                            egui::TextureOptions::LINEAR,
                        ));
                    }
                }
            }
            let activity = self.view.activity();
            let recent = &activity[activity.len().saturating_sub(OVERLAY_LINES)..];
            match &self.texture {
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
                    ui.label(muted("Waiting for the first frame of this page..."));
                    activity_lines(ui, recent);
                }
            }
        });
    }

    fn select(&mut self, key: Option<String>, ctx: &egui::Context) {
        self.view.observe(key.as_deref(), ctx);
        self.selected = key;
        self.texture = None;
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

/// The "Agent browser" toggle and its window. The screencast runs only while
/// the window is open.
pub struct AgentBrowserWindow {
    pane: AgentBrowserPane<LiveAgentView>,
    open: bool,
}

impl AgentBrowserWindow {
    pub fn new(browser: Arc<AgentBrowser>, runtime: tokio::runtime::Handle) -> Self {
        Self {
            pane: AgentBrowserPane::new(LiveAgentView::new(browser, runtime)),
            open: false,
        }
    }

    pub fn render(&mut self, ui: &mut egui::Ui) {
        if ui.button("Agent browser").clicked() {
            self.open = !self.open;
        }
        egui::Window::new("Agent browser")
            .open(&mut self.open)
            .default_size([960.0, 680.0])
            .show(ui.ctx(), |ui| self.pane.render(ui));
        if !self.open {
            self.pane.stop(ui.ctx());
        }
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
