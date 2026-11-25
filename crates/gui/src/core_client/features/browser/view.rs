//! Drawing of the feature: the tab strip, the toolbar, the preview area with
//! its input, and the bounded results (Capture, notices, inspection).
//!
//! Drawing changes state but sends nothing: what the user asked for is left in
//! `requests` and applied by the drive step, which holds the context to send
//! with.
use egui::{
    Align2, Color32, EventFilter, FontId, Rect, Sense, Vec2, load::SizedTexture, pos2, vec2,
};
use workbench_protocol::namespaces::browser::RendererViewport;

use crate::theme::icons;
use crate::theme::text::{medium, muted};
use crate::theme::tokens::palette;
use crate::theme::widgets::{
    accessible, ghost, ghost_icon_button, icon_button, readable_selection, reason_label,
};

use super::{
    BrowserFeature, MIN_VIEWPORT, UiRequest,
    frame::{MAX_HEIGHT, MAX_WIDTH},
    input::Surface,
    preview::{Action, Capture, InspectKind, Phase, Preview, Stream},
};

/// A visible part smaller than this does not count as shown (the area is
/// scrolled out or its header is closing).
const MIN_VISIBLE_PX: f32 = 8.0;

impl BrowserFeature {
    pub(super) fn draw(&mut self, ui: &mut egui::Ui) {
        self.select_default();
        for p in &mut self.previews {
            p.visible = false;
        }
        if let Some(notice) = &self.notice {
            reason_label(ui, palette().WARNING_FG, notice);
        }
        if self.previews.is_empty() {
            ui.label(muted(
                "No preview is open. A preview appears here when you open one from Artifacts.",
            ));
            return;
        }
        let mut chosen = None;
        let mut closes = Vec::new();
        ui.horizontal_wrapped(|ui| {
            readable_selection(ui);
            for p in &self.previews {
                if ui
                    .selectable_label(self.selected == Some(p.id), &p.title)
                    .clicked()
                {
                    chosen = Some(p.id);
                }
                if icon_button(ui, icons::X, &format!("Close {}", p.title)).clicked() {
                    closes.push(UiRequest::Close(p.id));
                }
            }
        });
        if chosen.is_some() {
            self.selected = chosen;
        }
        self.requests.extend(closes);
        let Some(idx) = self.selected.and_then(|id| self.index(id)) else {
            return;
        };
        let unknown =
            matches!(self.previews[idx].phase, Phase::Opening(op) if self.ops.is_unknown(op));
        let mut requests = Vec::new();
        draw_preview(ui, &mut self.previews[idx], unknown, &mut requests);
        self.requests.extend(requests);
    }
}

fn draw_preview(ui: &mut egui::Ui, p: &mut Preview, unknown: bool, requests: &mut Vec<UiRequest>) {
    let id = p.id;
    let warn = palette().WARNING_FG;
    ui.horizontal_wrapped(|ui| match &p.phase {
        Phase::Waiting => {
            ui.label(muted("Preparing the session…"));
        }
        Phase::Opening(_) if unknown => {
            reason_label(
                ui,
                warn,
                "Not known whether it opened (not sent again). Waiting for the result…",
            );
        }
        Phase::Opening(_) => {
            ui.label(muted("Opening…"));
        }
        Phase::OpenUnknown => {
            reason_label(
                ui,
                warn,
                "Not known whether it opened. Checking the tab list…",
            );
        }
        Phase::Suspended => {
            ui.label(muted("Reconnecting…"));
        }
        Phase::Lost(why) => {
            reason_label(ui, warn, why);
            if ui.add(ghost("Open again")).clicked() {
                requests.push(UiRequest::Reopen(id));
            }
        }
        Phase::Open => {
            match &p.stream {
                Stream::Blocked(why) => {
                    reason_label(ui, warn, format!("Could not start the stream: {why}"));
                    if ui.add(ghost("Retry")).clicked() {
                        requests.push(UiRequest::Retry(id));
                    }
                }
                Stream::Subscribing(_) | Stream::Binding { .. } => {
                    ui.label(muted("Connecting…"));
                }
                Stream::Off | Stream::Live { .. } => {}
            }
            let capturing = matches!(p.capture, Capture::Receiving(_) | Capture::Decoding(_));
            let capture = ui.add_enabled(
                !capturing,
                ghost(icons::with_icon(icons::CAMERA, "Capture")),
            );
            accessible(&capture, "Capture");
            if capture.clicked() {
                requests.push(UiRequest::Do(id, Action::Capture));
            }
            if ghost_icon_button(ui, icons::BELL, "Notices").clicked() {
                requests.push(UiRequest::Do(id, Action::Notices));
            }
            ui.menu_button("Inspect", |ui| {
                for kind in InspectKind::ALL {
                    if ui.button(kind.label()).clicked() {
                        requests.push(UiRequest::Do(id, Action::Inspect(kind)));
                        ui.close();
                    }
                }
            });
        }
    });

    let width = ui.available_width().max(MIN_VIEWPORT as f32);
    // 16:9: the area of a 1280-wide window is 1280x720 and of a 3840-wide one 3840x2160,
    // the sizes the measurements name (docs/renderer-measurement.md).
    let height = (width * 9.0 / 16.0).clamp(200.0, MAX_HEIGHT as f32);
    let (rect, response) = ui.allocate_exact_size(vec2(width, height), Sense::click_and_drag());
    let clipped = rect.intersect(ui.clip_rect());
    p.visible = ui.is_visible()
        && clipped.is_positive()
        && clipped.width() >= MIN_VISIBLE_PX
        && clipped.height() >= MIN_VISIBLE_PX;
    // One CSS pixel per frame pixel: the page is laid out at the size of the
    // area in points, and shown stretched over it on a denser display.
    // A texture may not be larger than the device allows (egui refuses it), so the page
    // is never asked to be larger than that either.
    let side = ui.ctx().input(|i| i.max_texture_side) as u32;
    p.want_viewport = Some(RendererViewport {
        width: (width.round() as u32).clamp(MIN_VIEWPORT, MAX_WIDTH.min(side.max(MIN_VIEWPORT))),
        height: (height.round() as u32).clamp(MIN_VIEWPORT, MAX_HEIGHT.min(side.max(MIN_VIEWPORT))),
    });

    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, Color32::WHITE);
    match p.picture.texture() {
        Some(texture) => {
            painter.image(
                texture.id(),
                rect,
                Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                Color32::WHITE,
            );
        }
        None => {
            let text = if p.is_open() {
                "Waiting for the first frame…"
            } else {
                "The preview cannot be shown"
            };
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                text,
                FontId::proportional(14.0),
                palette().TEXT_MUTED,
            );
        }
    }

    if p.is_open() {
        forward_input(ui, p, rect, &response);
    }

    if let Some(status) = &p.status {
        reason_label(ui, warn, status);
    }
    // A resize that failed or ended unknown is not sent again by itself for the same
    // size; this is the explicit way to try it again.
    if p.resize_failed.is_some()
        && ui
            .add(ghost("Retry resize"))
            .on_hover_text("Sends the same size again")
            .clicked()
    {
        requests.push(UiRequest::Retry(id));
    }
    match p.input.dropped() {
        0 => {}
        1 => {
            ui.label(muted("1 input was dropped"));
        }
        n => {
            ui.label(muted(format!("{n} inputs were dropped")));
        }
    }
    draw_results(ui, p);
}

fn forward_input(ui: &mut egui::Ui, p: &mut Preview, rect: Rect, response: &egui::Response) {
    if response.clicked() || response.drag_started() {
        response.request_focus();
    }
    let focused = response.has_focus();
    if !focused && p.focused {
        p.translator.release_all(&mut p.input);
    }
    p.focused = focused;
    if focused {
        // Keys egui would use to move focus belong to the page.
        ui.memory_mut(|memory| {
            memory.set_focus_lock_filter(
                response.id,
                EventFilter {
                    tab: true,
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    escape: true,
                },
            )
        });
        ui.ctx().output_mut(|output| {
            output.ime = Some(egui::output::IMEOutput {
                purpose: egui::IMEPurpose::Normal,
                rect,
                cursor_rect: Rect::from_min_size(rect.min, vec2(1.0, 16.0)),
                should_interrupt_composition: false,
            })
        });
    }
    let css = match (p.shown, &p.tab) {
        (Some([w, h]), _) => vec2(w as f32, h as f32),
        (None, Some(tab)) => vec2(tab.viewport.width as f32, tab.viewport.height as f32),
        _ => return,
    };
    let (events, time, hover_pos) = ui.input(|i| (i.events.clone(), i.time, i.pointer.hover_pos()));
    let surface = Surface {
        rect,
        css,
        hovered: response.hovered(),
        focused,
        time,
        hover_pos,
    };
    p.translator.translate(&events, &surface, &mut p.input);
}

fn draw_results(ui: &mut egui::Ui, p: &Preview) {
    match &p.capture {
        Capture::Receiving(_) => {
            ui.label(muted("Receiving the capture…"));
        }
        Capture::Decoding(_) => {
            ui.label(muted("Decoding the capture…"));
        }
        Capture::Failed(why) => {
            reason_label(ui, palette().WARNING_FG, why);
        }
        Capture::Idle => {}
    }
    if let Some(texture) = p.capture_picture.texture() {
        egui::CollapsingHeader::new(medium("Captured image"))
            .id_salt(("capture", p.id))
            .default_open(true)
            .show(ui, |ui| {
                let width = ui.available_width();
                ui.add(
                    egui::Image::new(SizedTexture::from_handle(texture))
                        .max_width(width)
                        .maintain_aspect_ratio(true),
                );
            });
    }
    if let Some(notices) = &p.notices {
        egui::CollapsingHeader::new(medium("Page notices"))
            .id_salt(("notices", p.id))
            .default_open(true)
            .show(ui, |ui| {
                if notices.blocked.is_empty() && notices.dialogs.is_empty() {
                    ui.label(muted("No notices"));
                }
                for line in &notices.blocked {
                    ui.label(format!("Blocked: {line}"));
                }
                for line in &notices.dialogs {
                    ui.label(format!("Dialog: {line}"));
                }
                if notices.truncated {
                    ui.label(muted("… (some were left out)"));
                }
            });
    }
    if let Some(inspection) = &p.inspection {
        egui::CollapsingHeader::new(medium(format!("Inspection: {}", inspection.label)))
            .id_salt(("inspection", p.id))
            .default_open(true)
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(240.0)
                    .id_salt(("inspection-scroll", p.id))
                    .show(ui, |ui| {
                        ui.monospace(&inspection.text);
                    });
                if inspection.truncated {
                    ui.label(muted("… (the result was cut off)"));
                }
            });
    }
}

#[allow(dead_code)]
fn _size_hint(_: Vec2) {}
