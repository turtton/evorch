use gui::model::composer::ComposerModel;
use gui::model::model_picker::ModelPickerState;

#[test]
fn generic_attachment_card_shows_metadata_without_decoding_file_bytes() {
    let mut composer = ComposerModel::default();
    let mut draft = gui::fixture::attachment_draft();
    draft.file.filename = "broken.png".into();
    draft.file.media_type = "image/png".into();
    draft.file.bytes = b"not an image".to_vec();
    composer.add_attachment(draft).unwrap();
    let mut harness = egui_kittest::Harness::builder().build_ui_state(
        |ui, model: &mut ComposerModel| {
            gui::panes::composer::composer_strip(
                ui,
                model,
                gui::panes::model_picker::ModelPickerContext {
                    profiles: &[],
                    preference: None,
                    default_model: None,
                    enabled: false,
                },
                &mut ModelPickerState::default(),
                None,
                Default::default(),
            );
        },
        composer,
    );
    harness.run();
    assert_eq!(harness.state().attachments[0].file.bytes, b"not an image");
    assert!(harness.state().input.is_empty());
    assert!(
        !harness
            .ctx
            .tex_manager()
            .read()
            .allocated()
            .any(|(_, meta)| meta.name == "attachment")
    );
}
