use std::sync::Arc;

use gui::app::WorkbenchState;
use gui::fixture::{DemoSource, ScriptedCodexAuthBackend};
use gui::headless::HeadlessWorkbench;
use gui::model::codex_auth::{
    CODEX_AUTHENTICATED_LABEL, CODEX_LOGIN_BUTTON, CODEX_UNAUTHENTICATED_GUIDANCE,
    CODEX_WAITING_LABEL, CodexAuthBackend, CodexAuthError, CodexAuthModel, CodexAuthState,
    CodexAuthSummary,
};
use gui::model::provider_settings::ProviderSettingsModel;
use gui::panes::codex_auth::{
    CODEX_LOGIN_FAILED_REJECTED, CODEX_LOGIN_FAILED_STORE, CODEX_LOGIN_FAILED_UNAVAILABLE,
};
use workspace_ui::UiSettings;

fn workbench(backend: Arc<dyn CodexAuthBackend>) -> HeadlessWorkbench<DemoSource> {
    let state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .expect("workbench")
        .with_provider_settings(ProviderSettingsModel {
            open: true,
            tab: gui::model::provider_settings::ProviderSettingsTab::Codex,
            ..Default::default()
        })
        .with_codex_auth(CodexAuthModel::with_backend(backend, "codex"));
    HeadlessWorkbench::new(state, [1200.0, 900.0])
}

fn step_until(
    harness: &mut HeadlessWorkbench<DemoSource>,
    pred: impl Fn(&CodexAuthState) -> bool,
    what: &str,
) {
    for _ in 0..200 {
        harness.step();
        if pred(&harness.state().codex_auth().state) {
            return;
        }
        std::thread::yield_now();
    }
    panic!(
        "{what} not reached: {:?}",
        harness.state().codex_auth().state
    );
}

#[test]
fn unauthenticated_state_shows_sign_in_button() {
    // Given
    let (backend, _gate) = ScriptedCodexAuthBackend::gated();
    let mut harness = workbench(backend.clone());
    // When
    harness.run();
    // Then
    for label in [
        CODEX_UNAUTHENTICATED_GUIDANCE,
        CODEX_LOGIN_BUTTON,
        "Provider settings",
        "Save",
    ] {
        assert!(harness.has_label(label), "{label}");
    }
    assert_eq!(backend.authenticate_calls(), 0);
    assert_eq!(
        harness.state().codex_auth().state,
        CodexAuthState::Unauthenticated
    );
}

#[test]
fn authenticating_state_shows_waiting_and_reopen_link() {
    // Given
    let (backend, gate) = ScriptedCodexAuthBackend::gated();
    let mut harness = workbench(backend.clone());
    harness.run();
    // When
    harness.click_label(CODEX_LOGIN_BUTTON);
    step_until(
        &mut harness,
        |state| {
            matches!(
                state,
                CodexAuthState::Authenticating {
                    prompt: Some(_),
                    ..
                }
            )
        },
        "prompt",
    );
    // Then
    assert!(harness.has_label("Open the sign-in page again"));
    assert!(harness.has_label(CODEX_WAITING_LABEL));
    harness.state_mut().start_codex_login();
    assert_eq!(backend.authenticate_calls(), 1);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    gate.send(Ok(CodexAuthSummary {
        expires_at_unix: Some(now + 3600),
    }))
    .expect("approval");
    step_until(
        &mut harness,
        |state| matches!(state, CodexAuthState::Authenticated { .. }),
        "authenticated",
    );
    assert!(harness.has_label(CODEX_AUTHENTICATED_LABEL));
    assert!(!harness.has_label("ABCD-1234"));
    assert!(!harness.has_label(CODEX_UNAUTHENTICATED_GUIDANCE));
}

#[test]
fn failed_login_shows_error_and_keeps_login_available() {
    // Given
    let backend = ScriptedCodexAuthBackend::immediate(Err(CodexAuthError::Rejected));
    let mut harness = workbench(backend);
    harness.run();
    // When
    harness.click_label(CODEX_LOGIN_BUTTON);
    step_until(
        &mut harness,
        |state| matches!(state, CodexAuthState::Failed { .. }),
        "failed",
    );
    // Then
    assert!(harness.has_label(CODEX_LOGIN_FAILED_REJECTED));
    assert!(!harness.has_label("sentinel-access-abc"));
    assert!(harness.has_label(CODEX_LOGIN_BUTTON));
}

#[test]
fn authenticated_store_seeds_authenticated_state() {
    // Given
    let backend = ScriptedCodexAuthBackend::authenticated(CodexAuthSummary {
        expires_at_unix: Some(1_893_456_000),
    });
    let mut harness = workbench(backend);
    // When
    harness.run();
    // Then
    assert!(harness.has_label(CODEX_AUTHENTICATED_LABEL));
    assert!(!harness.has_label(CODEX_UNAUTHENTICATED_GUIDANCE));
}

#[test]
fn save_failure_shows_fixed_message_and_refresh_stays_unauthenticated() {
    // Given: a login whose credential save fails.
    let backend = ScriptedCodexAuthBackend::immediate(Err(CodexAuthError::StoreUnavailable));
    let mut harness = workbench(backend);
    harness.run();
    // When: the login finishes with a store failure.
    harness.click_label(CODEX_LOGIN_BUTTON);
    step_until(
        &mut harness,
        |state| matches!(state, CodexAuthState::Failed { .. }),
        "save failure",
    );
    // Then: the fixed store message is shown and refresh cannot authenticate.
    assert!(harness.has_label(CODEX_LOGIN_FAILED_STORE));
    harness.state_mut().open_provider_settings();
    harness.step();
    assert_eq!(
        harness.state().codex_auth().state,
        CodexAuthState::Unauthenticated
    );
    assert!(!harness.has_label(CODEX_AUTHENTICATED_LABEL));
}

#[test]
fn open_provider_settings_refreshes_codex_state_from_store() {
    // Given
    let (backend, _gate) = ScriptedCodexAuthBackend::gated();
    let mut harness = workbench(backend.clone());
    harness.state_mut().provider_settings_mut().open = false;
    harness.run();
    backend.set_stored(Ok(Some(CodexAuthSummary {
        expires_at_unix: None,
    })));
    // When
    harness.state_mut().open_provider_settings();
    harness.step();
    // Then
    assert!(harness.has_label(CODEX_AUTHENTICATED_LABEL));
    assert!(!harness.has_label(CODEX_UNAUTHENTICATED_GUIDANCE));
}

#[test]
fn login_without_backend_fails_closed_in_ui() {
    // Given
    let state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .expect("workbench")
        .with_provider_settings(ProviderSettingsModel {
            open: true,
            tab: gui::model::provider_settings::ProviderSettingsTab::Codex,
            ..Default::default()
        });
    let mut harness = HeadlessWorkbench::new(state, [1200.0, 900.0]);
    harness.run();
    // When
    harness.click_label(CODEX_LOGIN_BUTTON);
    step_until(
        &mut harness,
        |state| matches!(state, CodexAuthState::Failed { .. }),
        "missing backend",
    );
    harness.step();
    // Then
    assert!(harness.has_label(CODEX_LOGIN_FAILED_UNAVAILABLE));
}

#[test]
fn take_url_to_open_fires_exactly_once() {
    // Given
    let mut model = CodexAuthModel::default();
    model.state = CodexAuthState::Authenticating {
        prompt: Some(ScriptedCodexAuthBackend::prompt()),
        opened_browser: false,
    };
    // When
    let first = model.take_url_to_open();
    let second = model.take_url_to_open();
    // Then
    assert_eq!(
        first,
        Some(ScriptedCodexAuthBackend::prompt().authorize_url)
    );
    assert_eq!(second, None);
}
