use crate::model::composer::{ComposerModel, ImageAttachment};
use base64::{Engine, engine::general_purpose::STANDARD};

pub(super) fn render(ui: &mut egui::Ui, model: &mut ComposerModel) {
    ui.input_mut(|input| {
        input.events.retain(|event| match event {
            egui::Event::Paste(value) => !model.add_pasted_image(value),
            _ => true,
        });
        for file in std::mem::take(&mut input.raw.dropped_files) {
            if let Ok(bytes) = file.bytes() {
                let media_type = match image::guess_format(&bytes) {
                    Ok(image::ImageFormat::Png) => "image/png",
                    _ => continue,
                };
                model.attachments.push(ImageAttachment {
                    media_type: media_type.into(),
                    data: STANDARD.encode(bytes),
                });
            }
        }
    });
    if let Some(warning) = model.image_warning() {
        ui.colored_label(crate::theme::tokens::WARNING_FG, warning);
    }
    if model.attachments.is_empty() {
        return;
    }
    ui.horizontal_wrapped(|ui| {
        let mut remove = None;
        for (index, attachment) in model.attachments.iter().enumerate() {
            ui.push_id(index, |ui| {
                ui.vertical(|ui| {
                    let texture_id = ui.id().with((&attachment.media_type, &attachment.data));
                    let texture = ui
                        .data_mut(|data| data.get_temp::<egui::TextureHandle>(texture_id))
                        .or_else(|| {
                            let bytes = STANDARD.decode(&attachment.data).ok()?;
                            let image = image::load_from_memory(&bytes)
                                .ok()?
                                .thumbnail(96, 96)
                                .into_rgba8();
                            let size = [
                                usize::try_from(image.width()).ok()?,
                                usize::try_from(image.height()).ok()?,
                            ];
                            let texture = ui.ctx().load_texture(
                                "attachment",
                                egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw()),
                                egui::TextureOptions::LINEAR,
                            );
                            ui.data_mut(|data| data.insert_temp(texture_id, texture.clone()));
                            Some(texture)
                        });
                    match texture {
                        Some(texture) => {
                            ui.image(&texture);
                        }
                        None => {
                            ui.label("Invalid image");
                        }
                    }
                    ui.label(&attachment.media_type);
                    if ui.small_button("Remove").clicked() {
                        remove = Some(index);
                    }
                });
            });
        }
        if let Some(index) = remove {
            model.remove_attachment(index);
        }
    });
}
