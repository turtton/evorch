use gui::app::WorkbenchState;
use gui::fixture::{DemoSource, ScriptedCodexAuthBackend};
use gui::headless::{HeadlessWorkbench, OffscreenError};
use gui::model::codex_auth::{
    CODEX_AUTHENTICATED_LABEL, CODEX_DEVICE_URL, CODEX_LOGIN_BUTTON,
    CODEX_UNAUTHENTICATED_GUIDANCE, CODEX_WAITING_LABEL, CodexAuthModel, CodexAuthState,
    CodexAuthSummary,
};
use gui::model::provider_settings::ProviderSettingsModel;
use workspace_ui::UiSettings;

#[test]
#[ignore = "writes PNG review evidence using an offscreen GPU adapter"]
fn capture_codex_auth_png_evidence() {
    // Given: an isolated scripted login and a writable review evidence directory.
    let directory =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.opencode/v04-captures");
    let probe = std::fs::create_dir_all(&directory)
        .and_then(|()| tempfile::NamedTempFile::new_in(&directory));
    match probe {
        Ok(probe) => drop(probe),
        Err(error) => {
            eprintln!("Codex capture evidence directory unavailable: {error}");
            return;
        }
    }
    let (backend, tx) = ScriptedCodexAuthBackend::gated();
    let state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .expect("default state builds")
        .with_provider_settings(ProviderSettingsModel {
            open: true,
            ..Default::default()
        })
        .with_codex_auth(CodexAuthModel::with_backend(backend, "codex"));
    let mut harness = HeadlessWorkbench::new(state, [1200.0, 900.0]);
    // When: the unauthenticated modal renders.
    harness.run();
    // Then: guidance is visible before login starts.
    for label in [
        CODEX_UNAUTHENTICATED_GUIDANCE,
        CODEX_DEVICE_URL,
        CODEX_LOGIN_BUTTON,
    ] {
        assert!(harness.has_label(label), "{label}");
    }
    let capture_available = capture_codex_frame(
        &mut harness,
        &directory.join("codex-auth-unauthenticated.png"),
    );

    // When: device login starts and publishes its prompt.
    harness.click_label(CODEX_LOGIN_BUTTON);
    step_until(&mut harness, |state| {
        matches!(state, CodexAuthState::Authenticating { prompt: Some(_) })
    });
    // Then: the device code and waiting state are visible.
    assert!(harness.has_label("ABCD-1234"));
    assert!(harness.has_label(CODEX_WAITING_LABEL));
    let capture_available = capture_available
        && capture_codex_frame(
            &mut harness,
            &directory.join("codex-auth-authenticating.png"),
        );

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
    if capture_available {
        capture_codex_frame(
            &mut harness,
            &directory.join("codex-auth-authenticated.png"),
        );
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

fn capture_codex_frame(
    harness: &mut HeadlessWorkbench<DemoSource>,
    path: &std::path::Path,
) -> bool {
    match harness.capture() {
        Ok(frame) => {
            assert_eq!((frame.width, frame.height), (1200, 900));
            frame.save_png(path).expect("Codex PNG saved");
            true
        }
        Err(OffscreenError::AdapterUnavailable(message)) => {
            eprintln!(
                "Skipping remaining Codex PNG captures at {}: {message}",
                path.display()
            );
            false
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
