use std::sync::Arc;

use super::{BrowserAction, ChromiumSource, FrameSource};
use crate::theme::{text::muted, tokens::ERROR_FG, widgets::pane_root};

pub struct BrowserPane<S> {
    source: S,
    texture: Option<egui::TextureHandle>,
    url: String,
    selector: String,
    error: Option<String>,
}

impl<S: FrameSource> BrowserPane<S> {
    pub fn new(source: S) -> Self {
        Self {
            source,
            texture: None,
            url: String::new(),
            selector: String::new(),
            error: None,
        }
    }

    pub fn texture_size(&self) -> Option<[usize; 2]> {
        self.texture.as_ref().map(egui::TextureHandle::size)
    }

    pub fn render(&mut self, ui: &mut egui::Ui) {
        pane_root(ui, "Browser", |ui| {
            if let Some(image) = self.source.poll_frame() {
                match &mut self.texture {
                    Some(texture) => texture.set(image, egui::TextureOptions::LINEAR),
                    None => {
                        self.texture = Some(ui.ctx().load_texture(
                            "browser-screencast",
                            image,
                            egui::TextureOptions::LINEAR,
                        ))
                    }
                }
            }
            ui.horizontal_wrapped(|ui| {
                ui.label("URL");
                ui.text_edit_singleline(&mut self.url);
                if ui.button("Navigate").clicked() {
                    self.error = match self.url.parse() {
                        Ok(url) => self
                            .source
                            .submit(BrowserAction::Navigate(url))
                            .err()
                            .map(|error| error.to_string()),
                        Err(error) => Some(format!("Invalid URL: {error}")),
                    };
                }
            });
            ui.horizontal_wrapped(|ui| {
                ui.label("CSS selector");
                ui.text_edit_singleline(&mut self.selector);
                if ui.button("Click element").clicked() {
                    self.error = self
                        .source
                        .submit(BrowserAction::Click(self.selector.clone()))
                        .err()
                        .map(|error| error.to_string());
                }
            });
            if let Some(error) = self.error.clone().or_else(|| self.source.error()) {
                ui.colored_label(ERROR_FG, error);
            }
            match &self.texture {
                Some(texture) => {
                    ui.add(
                        egui::Image::new(texture)
                            .max_size(ui.available_size())
                            .maintain_aspect_ratio(true),
                    );
                }
                None => {
                    ui.label(muted("Waiting for the first browser frame..."));
                }
            }
        });
    }
}

pub struct BrowserWindow {
    bus: Arc<event_bus::EventBus>,
    runtime: tokio::runtime::Handle,
    pane: Option<BrowserPane<ChromiumSource>>,
    open: bool,
    headful: bool,
}

impl BrowserWindow {
    pub fn new(bus: Arc<event_bus::EventBus>, runtime: tokio::runtime::Handle) -> Self {
        Self {
            bus,
            runtime,
            pane: None,
            open: false,
            headful: false,
        }
    }

    pub fn render(&mut self, ui: &mut egui::Ui) {
        if ui.button("Browser").clicked() {
            self.open = !self.open;
        }
        egui::Window::new("Browser")
            .open(&mut self.open)
            .show(ui.ctx(), |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.add_enabled(
                        self.pane.is_none(),
                        egui::Checkbox::new(&mut self.headful, "Headful (starts minimized)"),
                    );
                    match &self.pane {
                        None => {
                            if ui.button("Start browser").clicked() {
                                self.pane = Some(BrowserPane::new(ChromiumSource::start(
                                    &self.runtime,
                                    self.bus.clone(),
                                    self.headful,
                                )));
                            }
                        }
                        Some(_) => {
                            if ui.button("Stop browser").clicked() {
                                self.pane = None;
                            }
                        }
                    }
                });
                ui.label(muted(
                    "Actions, screenshots and DOM changes are recorded in diagnostics.",
                ));
                match &mut self.pane {
                    Some(pane) => pane.render(ui),
                    None => {
                        ui.label("Stopped. Chromium starts only when requested.");
                    }
                }
            });
        if !self.open {
            self.pane = None;
        }
    }
}
