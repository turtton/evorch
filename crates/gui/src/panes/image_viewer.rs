//! In-app viewer for presented image artifacts: a modal over the workbench
//! that zooms with the wheel, pans by dragging and offers the desktop's
//! application from the context menu.

use std::path::PathBuf;

use egui::{Pos2, Rect, RichText, Sense, Ui, Vec2};

use crate::theme::icons;
use crate::theme::tokens::{FONT_SMALL, SP_2, palette};
use crate::theme::widgets::{icon_button, surface_frame};

/// Smallest and largest zoom, as image pixels per screen pixel.
const MIN_ZOOM: f32 = 0.05;
const MAX_ZOOM: f32 = 16.0;
/// Zoom factor of one toolbar or keyboard step.
const ZOOM_STEP: f32 = 1.25;
/// Wheel points per e-fold of zoom; one notch (about 50 points) is ~1.28x.
const WHEEL_POINTS_PER_EFOLD: f32 = 200.0;
/// Part of the image kept inside the viewport while panning.
const PAN_MARGIN: f32 = 48.0;
/// Above this zoom, pixels are drawn sharp instead of interpolated.
const NEAREST_FROM_ZOOM: f32 = 2.0;
/// Share of the window the modal covers.
const VIEWPORT_SHARE: f32 = 0.92;

pub const OPEN_EXTERNALLY: &str = "Open in external app";

fn state_id() -> egui::Id {
    egui::Id::new("image-viewer")
}

/// Opens `path` in the viewer, replacing any image already shown.
pub fn open(ctx: &egui::Context, path: PathBuf, title: String) {
    ctx.data_mut(|data| {
        data.insert_temp(
            state_id(),
            Viewer {
                path,
                title,
                view: View::Fit,
            },
        );
    });
}

pub fn is_open(ctx: &egui::Context) -> bool {
    ctx.data(|data| data.get_temp::<Viewer>(state_id()).is_some())
}

pub fn close(ctx: &egui::Context) {
    ctx.data_mut(|data| data.remove::<Viewer>(state_id()));
}

/// Context menu entries shared by card previews and the viewer.
pub fn context_menu(response: &egui::Response, path: &std::path::Path) {
    response.context_menu(|ui| {
        if ui.button(OPEN_EXTERNALLY).clicked() {
            crate::panes::artifact_card::request_open(ui.ctx(), path.to_path_buf());
            ui.close();
        }
    });
}

#[derive(Clone)]
struct Viewer {
    path: PathBuf,
    title: String,
    view: View,
}

/// How the image sits in the viewport.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum View {
    /// Whole image visible, never enlarged; follows viewport resizes.
    Fit,
    /// `zoom` image pixels per screen pixel, image center offset by `pan`
    /// points from the viewport center.
    Free { zoom: f32, pan: Vec2 },
}

/// Fit zoom for an image of `image_px` pixels in `viewport` points.
pub(crate) fn fit_zoom(image_px: Vec2, viewport: Vec2, pixels_per_point: f32) -> f32 {
    let image = image_px / pixels_per_point;
    if image.x <= 0.0 || image.y <= 0.0 {
        return 1.0;
    }
    (viewport.x / image.x)
        .min(viewport.y / image.y)
        .min(1.0)
        .clamp(MIN_ZOOM, MAX_ZOOM)
}

impl View {
    pub(crate) fn resolve(self, image_px: Vec2, viewport: Vec2, ppp: f32) -> (f32, Vec2) {
        match self {
            View::Fit => (fit_zoom(image_px, viewport, ppp), Vec2::ZERO),
            View::Free { zoom, pan } => (zoom, pan),
        }
    }

    /// Multiplies the zoom by `factor`, keeping the image point under
    /// `anchor` (screen offset from the viewport center) in place.
    pub(crate) fn zoom_by(
        self,
        factor: f32,
        anchor: Vec2,
        image_px: Vec2,
        viewport: Vec2,
        ppp: f32,
    ) -> View {
        let (zoom, pan) = self.resolve(image_px, viewport, ppp);
        let next = (zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        let applied = next / zoom;
        let pan = anchor - (anchor - pan) * applied;
        View::Free {
            zoom: next,
            pan: clamp_pan(pan, image_px * next / ppp, viewport),
        }
    }

    pub(crate) fn pan_by(self, delta: Vec2, image_px: Vec2, viewport: Vec2, ppp: f32) -> View {
        let (zoom, pan) = self.resolve(image_px, viewport, ppp);
        View::Free {
            zoom,
            pan: clamp_pan(pan + delta, image_px * zoom / ppp, viewport),
        }
    }
}

/// Keeps at least [`PAN_MARGIN`] points of the image inside the viewport.
fn clamp_pan(pan: Vec2, shown: Vec2, viewport: Vec2) -> Vec2 {
    let limit = |shown: f32, viewport: f32| ((shown + viewport) / 2.0 - PAN_MARGIN).max(0.0);
    let x = limit(shown.x, viewport.x);
    let y = limit(shown.y, viewport.y);
    egui::vec2(pan.x.clamp(-x, x), pan.y.clamp(-y, y))
}

fn zoom_label(view: View, zoom: f32) -> String {
    let percent = (zoom * 100.0).round();
    match view {
        View::Fit => format!("Fit · {percent}%"),
        View::Free { .. } => format!("{percent}%"),
    }
}

/// Renders the viewer if one is open. Call once per frame after the panes.
pub fn show(ctx: &egui::Context) {
    let Some(mut viewer) = ctx.data(|data| data.get_temp::<Viewer>(state_id())) else {
        return;
    };
    crate::panes::file_viewer::install_image_loader(ctx);
    let screen = ctx.content_rect().size();
    let size = screen * VIEWPORT_SHARE;
    let mut close_clicked = false;
    let modal = egui::Modal::new(egui::Id::new("image-viewer-modal"))
        .backdrop_color(egui::Color32::from_black_alpha(200))
        .frame(surface_frame(palette().SURFACE_RAISED))
        .show(ctx, |ui| {
            ui.set_width(size.x);
            ui.set_height(size.y);
            close_clicked = viewer_contents(ui, &mut viewer);
        });
    if close_clicked || modal.should_close() {
        close(ctx);
    } else {
        ctx.data_mut(|data| data.insert_temp(state_id(), viewer));
    }
}

/// Returns whether the close button was clicked.
fn viewer_contents(ui: &mut Ui, viewer: &mut Viewer) -> bool {
    let ppp = ui.ctx().pixels_per_point();
    let uri = crate::panes::file_viewer::image_uri(&viewer.path);
    let options = |zoom: f32| {
        if zoom >= NEAREST_FROM_ZOOM {
            egui::TextureOptions::NEAREST
        } else {
            egui::TextureOptions::LINEAR
        }
    };
    let probe = uri.as_deref().map(|uri| {
        ui.ctx()
            .try_load_texture(uri, egui::TextureOptions::LINEAR, Default::default())
    });
    let image_px = match &probe {
        Some(Ok(egui::load::TexturePoll::Ready { texture })) => Some(texture.size),
        _ => None,
    };

    let mut close = false;
    let mut toolbar = None;
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(icons::with_icon(icons::IMAGE, &viewer.title))
                .color(palette().TEXT)
                .strong(),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            close = icon_button(ui, icons::X, "Close image viewer").clicked();
            if image_px.is_some() {
                if icon_button(ui, icons::FRAME_CORNERS, "Actual size").clicked() {
                    toolbar = Some(Toolbar::Actual);
                }
                if icon_button(ui, icons::ARROWS_IN, "Fit to window").clicked() {
                    toolbar = Some(Toolbar::Fit);
                }
                if icon_button(ui, icons::MAGNIFYING_GLASS_PLUS, "Zoom in").clicked() {
                    toolbar = Some(Toolbar::Zoom(ZOOM_STEP));
                }
                if icon_button(ui, icons::MAGNIFYING_GLASS_MINUS, "Zoom out").clicked() {
                    toolbar = Some(Toolbar::Zoom(1.0 / ZOOM_STEP));
                }
            }
        });
    });
    ui.add_space(SP_2);

    let (viewport, response) =
        ui.allocate_exact_size(ui.available_size_before_wrap(), Sense::click_and_drag());
    let response = response.on_hover_cursor(egui::CursorIcon::Grab);
    context_menu(&response, &viewer.path);

    let Some(image_px) = image_px else {
        let text = match probe {
            None => "This path cannot be shown.".to_owned(),
            Some(Err(error)) => format!("Cannot show the image: {error}"),
            Some(Ok(_)) => {
                ui.put(viewport, egui::Spinner::new());
                return close;
            }
        };
        ui.put(
            viewport,
            egui::Label::new(RichText::new(text).color(palette().WARNING_FG)),
        );
        return close;
    };

    let size = viewport.size();
    let pointer = |pos: Option<Pos2>| {
        pos.filter(|pos| viewport.contains(*pos))
            .map_or(Vec2::ZERO, |pos| pos - viewport.center())
    };
    match toolbar {
        Some(Toolbar::Fit) => viewer.view = View::Fit,
        Some(Toolbar::Actual) => {
            viewer.view = View::Free {
                zoom: 1.0,
                pan: Vec2::ZERO,
            }
        }
        Some(Toolbar::Zoom(factor)) => {
            viewer.view = viewer.view.zoom_by(factor, Vec2::ZERO, image_px, size, ppp);
        }
        None => {}
    }

    if response.dragged() {
        viewer.view = viewer
            .view
            .pan_by(response.drag_delta(), image_px, size, ppp);
    }
    if response.double_clicked() {
        let anchor = pointer(response.interact_pointer_pos());
        viewer.view = match viewer.view {
            View::Fit => View::Fit.zoom_by(
                1.0 / fit_zoom(image_px, size, ppp),
                anchor,
                image_px,
                size,
                ppp,
            ),
            View::Free { .. } => View::Fit,
        };
    }
    if response.hovered() {
        let (wheel, pinch, hover) = ui.input(|input| {
            (
                input.smooth_scroll_delta.y,
                input.zoom_delta(),
                input.pointer.hover_pos(),
            )
        });
        let factor = pinch * (wheel / WHEEL_POINTS_PER_EFOLD).exp();
        if factor != 1.0 {
            viewer.view = viewer
                .view
                .zoom_by(factor, pointer(hover), image_px, size, ppp);
        }
    }
    if let Some(key) = keyboard(ui) {
        viewer.view = match key {
            Toolbar::Fit => View::Fit,
            Toolbar::Actual => View::Free {
                zoom: 1.0,
                pan: Vec2::ZERO,
            },
            Toolbar::Zoom(factor) => viewer.view.zoom_by(factor, Vec2::ZERO, image_px, size, ppp),
        };
    }

    let (zoom, pan) = viewer.view.resolve(image_px, size, ppp);
    if let Some(uri) = uri.as_deref()
        && let Ok(egui::load::TexturePoll::Ready { texture }) =
            ui.ctx()
                .try_load_texture(uri, options(zoom), Default::default())
    {
        let rect = Rect::from_center_size(viewport.center() + pan, image_px * zoom / ppp);
        ui.painter().with_clip_rect(viewport).image(
            texture.id,
            rect,
            Rect::from_min_max(Pos2::ZERO, egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );
    }
    let label = zoom_label(viewer.view, zoom);
    response.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Image,
            true,
            format!("{} ({label})", viewer.title),
        )
    });
    let galley = ui.painter().layout_no_wrap(
        label,
        egui::FontId::proportional(FONT_SMALL),
        egui::Color32::from_gray(235),
    );
    let pill = Rect::from_min_size(
        viewport.right_bottom() - galley.size() - egui::vec2(SP_2 * 2.0, SP_2 * 1.5),
        galley.size() + egui::vec2(SP_2, SP_2 / 2.0),
    );
    ui.painter().rect_filled(
        pill,
        pill.height() / 2.0,
        egui::Color32::from_black_alpha(160),
    );
    ui.painter().galley(
        pill.center() - galley.size() / 2.0,
        galley,
        egui::Color32::from_gray(235),
    );
    close
}

#[derive(Clone, Copy)]
enum Toolbar {
    Fit,
    Actual,
    Zoom(f32),
}

fn keyboard(ui: &Ui) -> Option<Toolbar> {
    // A focused text field behind or inside the modal keeps its keys.
    if ui.ctx().egui_wants_keyboard_input() {
        return None;
    }
    ui.input(|input| {
        if input.key_pressed(egui::Key::Plus) || input.key_pressed(egui::Key::Equals) {
            Some(Toolbar::Zoom(ZOOM_STEP))
        } else if input.key_pressed(egui::Key::Minus) {
            Some(Toolbar::Zoom(1.0 / ZOOM_STEP))
        } else if input.key_pressed(egui::Key::Num0) {
            Some(Toolbar::Fit)
        } else if input.key_pressed(egui::Key::Num1) {
            Some(Toolbar::Actual)
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests;
