use crate::theme::tokens::palette;

pub(super) fn show(ui: &mut egui::Ui, id: egui::Id, content: (&str, bool)) {
    let (text, streaming) = content;
    let previous = ui.data_mut(|data| {
        let previous = data.get_temp::<bool>(id);
        data.insert_temp(id, streaming);
        previous
    });
    let transition = previous
        .filter(|previous| *previous != streaming)
        .map(|_| streaming);
    egui::CollapsingHeader::new(egui::RichText::new("thinking").color(palette().TEXT_MUTED))
        .id_salt(id)
        .default_open(streaming)
        .open(transition)
        .show(ui, |ui| {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(text)
                        .italics()
                        .color(palette().TEXT_MUTED),
                )
                .wrap(),
            );
        });
}
