use crate::theme::{tokens::palette, widgets::badge};

pub(super) fn binding_status(
    ui: &mut egui::Ui,
    logical: Option<&str>,
    routes: &std::collections::BTreeSet<String>,
) -> Option<String> {
    let logical = logical.filter(|name| !routes.contains(*name))?;
    ui.horizontal_wrapped(|ui| {
        badge(
            ui,
            "未定義 (route なし)",
            palette().WARNING_FG,
            palette().WARNING_SURFACE,
        );
        ui.add_enabled(
            !logical.trim().is_empty(),
            egui::Button::new("route を作成"),
        )
        .clicked()
        .then(|| logical.to_owned())
    })
    .inner
}
