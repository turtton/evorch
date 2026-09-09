use gui::app::WorkbenchState;
use gui::fixture::{DemoSource, ScriptedCodexAuthBackend};
use gui::headless::{HeadlessWorkbench, OffscreenError};
use gui::model::codex_auth::{
    CODEX_AUTHENTICATED_LABEL, CODEX_LOGIN_BUTTON, CODEX_UNAUTHENTICATED_GUIDANCE,
    CODEX_WAITING_LABEL, CodexAuthModel, CodexAuthState, CodexAuthSummary,
};
use gui::model::provider_settings::ProviderSettingsModel;
use workspace_ui::UiSettings;

#[test]
#[should_panic(expected = "Codex PNG capture required")]
fn codex_capture_panics_when_adapter_unavailable() {
    // Given: an unavailable adapter.
    let frame = Err(OffscreenError::AdapterUnavailable("test adapter".into()));
    // When: mandatory evidence is saved.
    save_codex_frame(frame, std::path::Path::new("unused.png"));
    // Then: the evidence test must fail, not skip.
}

#[test]
#[should_panic(expected = "Codex PNG saved")]
fn codex_capture_panics_when_png_save_fails() {
    // Given: a frame and a directory rather than a writable PNG path.
    let directory = tempfile::tempdir().expect("temp directory");
    let frame = gui::headless::CapturedFrame {
        width: 1200,
        height: 900,
        rgba: vec![0; 1200 * 900 * 4],
    };
    // When / Then: saving mandatory evidence must fail.
    save_codex_frame(Ok(frame), directory.path());
}

#[test]
#[ignore = "writes PNG review evidence using an offscreen GPU adapter"]
// CI headless-capture runs this explicitly; missing evidence is a failure.
fn capture_codex_auth_png_evidence() {
    // Given: an isolated scripted login and a writable review evidence directory.
    let directory =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.opencode/v04-captures");
    std::fs::create_dir_all(&directory).expect("Codex capture directory required");
    let (backend, tx) = ScriptedCodexAuthBackend::gated();
    let state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .expect("default state builds")
        .with_provider_settings({
            let mut settings = ProviderSettingsModel::default();
            settings.open = true;
            settings
        })
        .with_codex_auth(CodexAuthModel::with_backend(backend, "codex"));
    let mut harness = HeadlessWorkbench::new(state, [1200.0, 900.0]);
    // Given: the modal is opened on its default OpenAI tab.
    harness.run();
    // When: the Codex subscription tab is selected and rendered.
    harness.click_label("+ Add Codex subscription");
    harness.run();
    // Then: the browser sign-in guidance is visible before login starts.
    for label in [CODEX_UNAUTHENTICATED_GUIDANCE, CODEX_LOGIN_BUTTON] {
        assert!(harness.has_label(label), "{label}");
    }
    let unauthenticated = harness.capture();

    // When: browser login starts and publishes its prompt.
    harness.click_label(CODEX_LOGIN_BUTTON);
    step_until(&mut harness, |state| {
        matches!(
            state,
            CodexAuthState::Authenticating {
                prompt: Some(_),
                ..
            }
        )
    });
    // Then: the reopen link and waiting state are visible.
    assert!(harness.has_label("Open the sign-in page again"));
    assert!(harness.has_label(CODEX_WAITING_LABEL));
    let authenticating = harness.capture();

    // When: the scripted browser approval completes.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    tx.send(Ok(CodexAuthSummary {
        expires_at_unix: Some(now + 3600),
    }))
    .expect("approval");
    step_until(&mut harness, |state| {
        matches!(state, CodexAuthState::Authenticated { .. })
    });
    // Then: authentication is visible without exposing tokens.
    assert!(harness.has_label(CODEX_AUTHENTICATED_LABEL));
    let authenticated = harness.capture();
    for (frame, name) in [
        (unauthenticated, "codex-auth-unauthenticated.png"),
        (authenticating, "codex-auth-authenticating.png"),
        (authenticated, "codex-auth-authenticated.png"),
    ] {
        save_codex_frame(frame, &directory.join(name));
    }
}

fn step_until(
    harness: &mut HeadlessWorkbench<DemoSource>,
    predicate: impl Fn(&CodexAuthState) -> bool,
) {
    for _ in 0..200 {
        harness.step();
        if predicate(&harness.state().codex_auth().state) {
            return;
        }
        std::thread::yield_now();
    }
    panic!(
        "Codex state not reached: {:?}",
        harness.state().codex_auth().state
    );
}

fn save_codex_frame(
    frame: Result<gui::headless::CapturedFrame, OffscreenError>,
    path: &std::path::Path,
) {
    match frame {
        Ok(frame) => {
            assert_eq!((frame.width, frame.height), (1200, 900));
            frame.save_png(path).expect("Codex PNG saved");
        }
        Err(OffscreenError::AdapterUnavailable(message)) => {
            panic!(
                "Codex PNG capture required at {}: AdapterUnavailable: {message}",
                path.display()
            );
        }
        Err(error) => panic!("unexpected Codex capture error: {error}"),
    }
}

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
        let editor = gui::model::provider_settings::OpenAiEditorModel {
            open: true,
            name: "local".into(),
            base_url: "https://api.example.invalid/v1/chat/completions/very/long/path/that/should/not/clip/in/the/provider/settings/modal".into(),
            api_key_env: "LOCAL_API_KEY_WITH_A_VERY_LONG_NAME".into(),
        models: vec![config::types::provider::ModelEntryConfig::enabled("org/example/model-name-that-is-very-long-and-should-not-clip")],
            default_model: "org/example/model-name-that-is-very-long-and-should-not-clip".into(),
            ..Default::default()
        };
        let mut model = ProviderSettingsModel::default();
        model.open = true;
        model.editor = Some(gui::model::provider_settings::ProfileEditor::OpenAiCompatible(editor));
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
