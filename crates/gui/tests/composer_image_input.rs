use base64::{Engine, engine::general_purpose::STANDARD};
use gui::model::composer::{ComposerModel, ProviderStatus};
use std::sync::Arc;

#[derive(Debug)]
struct DroppedPng(Vec<u8>);

impl egui::DroppedFile for DroppedPng {
    fn path(&self) -> &std::path::Path {
        std::path::Path::new("image.png")
    }
    fn bytes(&self) -> Result<Vec<u8>, String> {
        Ok(self.0.clone())
    }
}

fn png() -> Vec<u8> {
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgba8(2, 3)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .expect("PNG");
    bytes.into_inner()
}

#[test]
fn paste_and_drop_render_real_thumbnails_without_pasting_base64_into_text() {
    for paste in [true, false] {
        let bytes = png();
        let mut harness = egui_kittest::Harness::builder().build_ui_state(
            |ui, model: &mut ComposerModel| {
                gui::panes::composer::composer_strip(ui, model, &ProviderStatus::Configured, None);
            },
            ComposerModel {
                image_input_supported: true,
                ..Default::default()
            },
        );
        if paste {
            harness.event(egui::Event::Paste(format!(
                "data:image/png;base64,{}",
                STANDARD.encode(&bytes)
            )));
        } else {
            harness
                .input_mut()
                .dropped_files
                .push(Arc::new(DroppedPng(bytes.clone())));
        }
        harness.run();
        assert_eq!(harness.state().attachments.len(), 1);
        assert_eq!(
            STANDARD
                .decode(&harness.state().attachments[0].data)
                .expect("base64"),
            bytes
        );
        assert!(harness.state().input.is_empty());
        assert!(
            harness
                .ctx
                .tex_manager()
                .read()
                .allocated()
                .any(|(_, meta)| meta.name == "attachment" && meta.size == [64, 96])
        );
    }
}
