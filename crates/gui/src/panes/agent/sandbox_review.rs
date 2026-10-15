use egui::{Label, RichText, Ui};

use crate::theme::icons;
use crate::theme::tokens::{FONT_SMALL, SP_2, palette};
use crate::theme::widgets::copy_id_button;

/// A review whose tool card is not in the transcript, e.g. in restored history.
pub(super) fn show(ui: &mut Ui, text: &str, call_id: Option<&str>) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = SP_2;
        ui.label(
            RichText::new(icons::SHIELD_WARNING)
                .size(FONT_SMALL + 1.0)
                .color(palette().WARNING_FG),
        );
        ui.add(Label::new(text).wrap());
        if let Some(call_id) = call_id {
            copy_id_button(ui, "Copy tool call ID", call_id);
        }
    });
}
