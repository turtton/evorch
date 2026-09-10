use gui::app::WorkbenchState;
use gui::fixture::DemoSource;
use gui::headless::HeadlessWorkbench;
use gui::model::provider_settings::{OpenAiEditorModel, ProfileEditor, ProviderSettingsModel};

#[test]
#[ignore = "writes offscreen PNG evidence"]
fn capture_model_metadata() {
    let dir = std::env::var_os("EVORCH_METADATA_EVIDENCE").expect("evidence directory");
    for size in [[1200.0, 900.0], [800.0, 600.0]] {
        let mut settings = ProviderSettingsModel::default();
        settings.open = true;
        settings.model_presets.insert(
            "large".into(),
            config::ModelPresetConfig {
                context_window: Some(128_000),
                max_output_tokens: Some(8_000),
                input_price_per_million_usd: Some(2.5),
                output_price_per_million_usd: Some(10.0),
            },
        );
        let mut model = config::ModelEntryConfig::enabled("test-model");
        model.preset = Some("large".into());
        settings.editor = Some(ProfileEditor::OpenAiCompatible(OpenAiEditorModel {
            name: "test-provider".into(),
            base_url: "https://example.invalid/v1".into(),
            models: vec![model],
            default_model: "test-model".into(),
            ..Default::default()
        }));
        let state =
            WorkbenchState::new(DemoSource(Vec::new()), &workspace_ui::UiSettings::default())
                .unwrap()
                .with_provider_settings(settings);
        let mut harness = HeadlessWorkbench::new(state, size);
        harness.run();
        harness
            .capture()
            .unwrap()
            .save_png(
                &std::path::Path::new(&dir).join(format!("metadata-{}-collapsed.png", size[0])),
            )
            .unwrap();
        for _ in 0..30 {
            harness.step();
        }
        harness.click_label("Metadata: test-model");
        harness.run();
        assert!(harness.has_label("Preset"));
        harness
            .capture()
            .unwrap()
            .save_png(
                &std::path::Path::new(&dir).join(format!("metadata-{}-expanded.png", size[0])),
            )
            .unwrap();
    }
}
