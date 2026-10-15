//! Reserve the title's normal space, then paint it after the body so it can
//! stay at the scroll viewport's top without introducing a nested scroll area.

use egui::{Align, FontId, Id, InnerResponse, Rect, Ui, UiBuilder, vec2};

use crate::theme::tokens::{FONT_BODY, SP_2, SP_3, palette};

pub(super) struct Header {
    id: Id,
    original: Rect,
}

impl Header {
    pub(super) fn reserve(ui: &mut Ui, id: Id) -> Self {
        let text_height =
            ui.fonts_mut(|fonts| fonts.row_height(&FontId::proportional(FONT_BODY + 1.0)));
        let height = ui.spacing().interact_size.y.max(text_height);
        let width = ui.available_width();
        let (_, reserved) = ui.allocate_space(vec2(0.0, height));
        let original = Rect::from_min_size(reserved.min, vec2(width, height));
        Self { id, original }
    }

    pub(super) fn show<R>(
        &self,
        ui: &mut Ui,
        expanded: bool,
        add: impl FnOnce(&mut Ui) -> R,
    ) -> InnerResponse<R> {
        // Use this frame's body height, not a cached height: resizing the pane
        // or receiving more output must not leave a stale floating title.
        let top = if expanded {
            ui.clip_rect()
                .top()
                .min(ui.min_rect().bottom() - self.original.height())
                .max(self.original.top())
        } else {
            self.original.top()
        };
        let rect = self
            .original
            .translate(vec2(0.0, top - self.original.top()));
        if top > self.original.top() {
            // The body was painted first. An opaque surface hides it beneath
            // the real, single title button, including its shell-log action.
            ui.painter()
                .rect_filled(rect.expand2(vec2(SP_3, SP_2)), 0.0, palette().SURFACE);
        }
        let mut title_ui = ui.new_child(UiBuilder::new().id(self.id.with("title")).max_rect(rect));
        let response = title_ui.horizontal(add);
        // Retain the card's natural width when closed, but do not count the
        // moved title as extra height in the scrolling body.
        ui.expand_to_include_x(response.response.rect.right());
        response
    }

    pub(super) fn reveal_after_collapse(&self, ui: &Ui) {
        if self.original.top() < ui.clip_rect().top() {
            ui.scroll_to_rect_animation(
                self.original,
                Some(Align::TOP),
                egui::style::ScrollAnimation::none(),
            );
        }
    }
}
