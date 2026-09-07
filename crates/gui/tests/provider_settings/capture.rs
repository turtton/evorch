use gui::app::WorkbenchState;
use gui::fixture::DemoSource;
use gui::headless::HeadlessWorkbench;
use gui::model::provider_settings::ProviderSettingsModel;
use workspace_ui::UiSettings;

#[test]
#[ignore = "writes PNG review evidence using an offscreen GPU adapter"]
fn capture_modal_png_evidence() {
    // Given: a writable evidence directory and long provider fields at two viewport sizes.
    let directory =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.opencode/v04-captures");
    std::fs::create_dir_all(&directory).expect("capture directory writable");
    let probe = tempfile::NamedTempFile::new_in(&directory).expect("capture write probe");
    drop(probe);
    for (size, filename, dimensions) in [
        ([800.0, 600.0], "settings-800.png", (800, 600)),
        ([1600.0, 900.0], "settings-1600.png", (1600, 900)),
    ] {
        let model = ProviderSettingsModel {
            open: true,
            name: "local".into(),
            base_url: "https://api.example.invalid/v1/chat/completions/very/long/path/that/should/not/clip/in/the/provider/settings/modal".into(),
            api_key_env: "LOCAL_API_KEY_WITH_A_VERY_LONG_NAME".into(),
            models_text: "org/example/model-name-that-is-very-long-and-should-not-clip".into(),
            default_model: "org/example/model-name-that-is-very-long-and-should-not-clip".into(),
            ..ProviderSettingsModel::default()
        };
        let state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
            .expect("default state builds")
            .with_provider_settings(model);
        let mut harness = HeadlessWorkbench::new(state, size);
        // When: the seeded modal renders directly, independent of ambient credentials.
        harness.run();
        let frame = harness.capture().expect("modal capture");
        // Then: each viewport produces a correctly sized PNG with visible modal controls.
        assert_eq!((frame.width, frame.height), dimensions);
        assert!(harness.has_label("Provider settings"));
        assert!(harness.has_label("Save"));
        assert!(harness.has_label("Cancel"));
        frame
            .save_png(&directory.join(filename))
            .expect("PNG saved");
    }
}
