use std::sync::{Arc, Mutex};

use cef::ImplBrowserHost;
use egui::{Color32, Pos2, Rect, Sense, TextureOptions, Vec2};

use crate::browser::{DialogKind, JsDialog, OsrBrowser};
use crate::clipboard::Clipboard;
use crate::frame::PaintBuffer;
use crate::host;
use crate::input::{FrameInput, InputTranslator};

/// Shows an [`OsrBrowser`] in egui with its native popups and JS dialogs, and
/// forwards pointer, wheel, keyboard and IME input. Text the page copies goes
/// to egui's clipboard unless [`CefView::with_clipboard`] names another.
pub struct CefView {
    browser: OsrBrowser,
    texture: Option<egui::TextureHandle>,
    popup_texture: Option<egui::TextureHandle>,
    clipboard: Option<Arc<dyn Clipboard>>,
    input: InputTranslator,
    prompt: Option<(u64, String)>,
    had_focus: bool,
    repaint_hooked: bool,
}

impl CefView {
    pub fn new(browser: OsrBrowser) -> Self {
        Self {
            browser,
            texture: None,
            popup_texture: None,
            clipboard: None,
            input: InputTranslator::default(),
            prompt: None,
            had_focus: false,
            repaint_hooked: false,
        }
    }

    pub fn with_clipboard(mut self, clipboard: Arc<dyn Clipboard>) -> Self {
        self.clipboard = Some(clipboard);
        self
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
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, Color32::WHITE);
        if let Some(texture) = upload(
            &mut self.texture,
            &self.browser.shared.frame,
            ui.ctx(),
            "html-preview",
        ) {
            paint(&painter, texture, rect.min, pixels_per_point);
        }
        let popup = self.browser.status().popup;
        match popup {
            Some(popup) => {
                if let Some(texture) = upload(
                    &mut self.popup_texture,
                    &self.browser.shared.popup_frame,
                    ui.ctx(),
                    "html-preview-popup",
                ) {
                    let at = rect.min + Vec2::new(popup.shown.x as f32, popup.shown.y as f32);
                    paint(&painter, texture, at, pixels_per_point);
                }
            }
            None => self.popup_texture = None,
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
        let dialog = self.browser.status().dialog.clone();
        match dialog {
            Some(dialog) => self.show_dialog(ui.ctx(), response.id, dialog),
            None => self.forward_input(ui, rect, response.hovered(), focused),
        }
        for text in self.browser.take_copies() {
            match &self.clipboard {
                Some(clipboard) => clipboard.write_text(text),
                None => ui.ctx().copy_text(text),
            }
        }
        self.show_status(ui, rect, &response, focused);
        response
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
        let popup = self.browser.status().popup;
        for input in self.input.translate(&events, &frame) {
            let input = match popup {
                Some(popup) => popup.route(input),
                None => input,
            };
            host::apply(&host, &input);
        }
    }

    fn show_dialog(&mut self, ctx: &egui::Context, id: egui::Id, dialog: JsDialog) {
        if self
            .prompt
            .as_ref()
            .is_none_or(|(shown, _)| *shown != dialog.id)
        {
            let default = match &dialog.kind {
                DialogKind::Prompt { default } => default.clone(),
                _ => String::new(),
            };
            self.prompt = Some((dialog.id, default));
        }
        let (title, accept, cancel) = match dialog.kind {
            DialogKind::Alert => ("The page says", "OK", None),
            DialogKind::Confirm | DialogKind::Prompt { .. } => {
                ("The page asks", "OK", Some("Cancel"))
            }
            DialogKind::BeforeUnload { reload: true } => {
                ("Reload this page?", "Reload", Some("Stay"))
            }
            DialogKind::BeforeUnload { reload: false } => {
                ("Leave this page?", "Leave", Some("Stay"))
            }
        };
        let is_prompt = matches!(dialog.kind, DialogKind::Prompt { .. });
        let mut answer = None;
        let modal = egui::Modal::new(id.with(("dialog", dialog.id))).show(ctx, |ui| {
            ui.set_max_width(420.0);
            ui.heading(title);
            if !dialog.message.is_empty() {
                ui.label(&dialog.message);
            }
            if is_prompt && let Some((_, text)) = &mut self.prompt {
                ui.text_edit_singleline(text).request_focus();
            }
            ui.horizontal(|ui| {
                if ui.button(accept).clicked() {
                    answer = Some(true);
                }
                if let Some(cancel) = cancel
                    && ui.button(cancel).clicked()
                {
                    answer = Some(false);
                }
            });
        });
        if answer.is_none() && modal.should_close() {
            answer = Some(cancel_value(&dialog.kind));
        }
        if let Some(accept) = answer {
            let text = self
                .prompt
                .take()
                .filter(|_| is_prompt && accept)
                .map(|(_, text)| text);
            self.browser
                .answer_dialog(dialog.id, accept, text.as_deref());
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

/// Dismissing an alert closes it; dismissing anything else means Cancel or Stay.
fn cancel_value(kind: &DialogKind) -> bool {
    matches!(kind, DialogKind::Alert)
}

fn upload<'a>(
    texture: &'a mut Option<egui::TextureHandle>,
    buffer: &Mutex<PaintBuffer>,
    ctx: &egui::Context,
    name: &str,
) -> Option<&'a egui::TextureHandle> {
    let texture_size = texture.as_ref().map(egui::TextureHandle::size);
    let upload = buffer
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take_upload(texture_size);
    if let Some(upload) = upload {
        match texture {
            Some(texture) if upload.partial => {
                texture.set_partial(upload.at, upload.image, TextureOptions::NEAREST);
            }
            _ => *texture = Some(ctx.load_texture(name, upload.image, TextureOptions::NEAREST)),
        }
    }
    texture.as_ref()
}

fn paint(painter: &egui::Painter, texture: &egui::TextureHandle, at: Pos2, pixels_per_point: f32) {
    let [width, height] = texture.size();
    painter.image(
        texture.id(),
        Rect::from_min_size(
            at,
            Vec2::new(width as f32, height as f32) / pixels_per_point,
        ),
        Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
        Color32::WHITE,
    );
}
