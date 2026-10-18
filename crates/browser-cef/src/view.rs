use cef::ImplBrowserHost;
use egui::{Color32, Pos2, Rect, Sense, TextureOptions, Vec2};

use crate::browser::OsrBrowser;
use crate::host;
use crate::input::{FrameInput, InputTranslator};

/// Shows an [`OsrBrowser`] in egui and forwards pointer, wheel, keyboard and IME input.
pub struct CefView {
    browser: OsrBrowser,
    texture: Option<egui::TextureHandle>,
    input: InputTranslator,
    had_focus: bool,
    repaint_hooked: bool,
}

impl CefView {
    pub fn new(browser: OsrBrowser) -> Self {
        Self {
            browser,
            texture: None,
            input: InputTranslator::default(),
            had_focus: false,
            repaint_hooked: false,
        }
    }

    pub fn browser(&self) -> &OsrBrowser {
        &self.browser
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) -> egui::Response {
        let (rect, response) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
        if !self.repaint_hooked {
            self.repaint_hooked = true;
            let ctx = ui.ctx().clone();
            self.browser
                .set_repaint_callback(move || ctx.request_repaint());
        }
        let pixels_per_point = ui.ctx().pixels_per_point();
        self.browser.resize(
            rect.width().floor() as i32,
            rect.height().floor() as i32,
            pixels_per_point,
        );
        self.upload(ui.ctx());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, Color32::WHITE);
        if let Some(texture) = &self.texture {
            let [width, height] = texture.size();
            let size = Vec2::new(width as f32, height as f32) / pixels_per_point;
            painter.image(
                texture.id(),
                Rect::from_min_size(rect.min, size),
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                Color32::WHITE,
            );
        }
        if response.clicked() || response.drag_started() {
            response.request_focus();
        }
        let focused = response.has_focus();
        if focused != self.had_focus {
            self.had_focus = focused;
            if let Some(host) = self.browser.host() {
                host.set_focus(focused.into());
            }
        }
        if focused {
            // Keys egui would otherwise use to move focus belong to the page.
            ui.memory_mut(|memory| {
                memory.set_focus_lock_filter(
                    response.id,
                    egui::EventFilter {
                        tab: true,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: true,
                    },
                )
            });
        }
        self.forward_input(ui, rect, response.hovered(), focused);
        self.show_status(ui, rect, &response, focused);
        response
    }

    fn upload(&mut self, ctx: &egui::Context) {
        let texture_size = self.texture.as_ref().map(egui::TextureHandle::size);
        let Some(upload) = self
            .browser
            .shared
            .frame
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take_upload(texture_size)
        else {
            return;
        };
        match &mut self.texture {
            Some(texture) if upload.partial => {
                texture.set_partial(upload.at, upload.image, TextureOptions::NEAREST);
            }
            _ => {
                self.texture =
                    Some(ctx.load_texture("html-preview", upload.image, TextureOptions::NEAREST));
            }
        }
    }

    fn forward_input(&mut self, ui: &egui::Ui, rect: Rect, hovered: bool, focused: bool) {
        let Some(host) = self.browser.host() else {
            return;
        };
        let (events, time, hover_pos) =
            ui.input(|input| (input.events.clone(), input.time, input.pointer.hover_pos()));
        let frame = FrameInput {
            rect,
            hovered,
            focused,
            time,
            hover_pos,
        };
        for input in self.input.translate(&events, &frame) {
            host::apply(&host, &input);
        }
    }

    fn show_status(&self, ui: &egui::Ui, rect: Rect, response: &egui::Response, focused: bool) {
        let status = self.browser.status();
        if response.hovered() {
            ui.ctx().set_cursor_icon(host::cursor_icon(status.cursor));
        }
        let tooltip = status.tooltip.clone();
        let caret = status.ime_caret;
        drop(status);
        if focused {
            let cursor_rect = caret.map_or(
                Rect::from_min_size(rect.min, Vec2::new(1.0, 16.0)),
                |caret| {
                    Rect::from_min_size(
                        rect.min + Vec2::new(caret.x as f32, caret.y as f32),
                        Vec2::new(1.0, caret.height as f32),
                    )
                },
            );
            ui.ctx().output_mut(|output| {
                output.ime = Some(egui::output::IMEOutput {
                    purpose: egui::IMEPurpose::Normal,
                    rect,
                    cursor_rect,
                    should_interrupt_composition: false,
                })
            });
        }
        if let Some(text) = tooltip
            && response.hovered()
        {
            response.clone().on_hover_text_at_pointer(text);
        }
    }
}
