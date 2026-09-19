use super::*;
use sandbox::{CredentialError, CredentialStore, FileCredentialStore, Secret};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

struct SlowFileStore {
    inner: Arc<FileCredentialStore>,
    read_started: Arc<AtomicBool>,
}

impl CredentialStore for SlowFileStore {
    fn get(&self, key: &str) -> Result<Option<Secret>, CredentialError> {
        self.read_started.store(true, Ordering::SeqCst);
        // Inject slow I/O; this is not the test's completion-wait mechanism.
        std::thread::sleep(Duration::from_secs(3));
        self.inner.get(key)
    }

    fn set(&self, key: &str, value: &Secret) -> Result<(), CredentialError> {
        self.inner.set(key, value)
    }

    fn delete(&self, key: &str) -> Result<(), CredentialError> {
        self.inner.delete(key)
    }
}

#[test]
fn saves_existing_keyring_profile_when_injected_store_is_slow() {
    // Given
    let temp = tempfile::tempdir().unwrap();
    let (_, store) = edit_save::keyring_editor(temp.path());
    let read_started = Arc::new(AtomicBool::new(false));
    let state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .unwrap()
        .with_provider_settings_path(temp.path().join("evorch.toml"))
        .with_provider_settings(ProviderSettingsModel::seed_from_config(&load_config(
            temp.path(),
        )))
        .with_credential_store(Arc::new(SlowFileStore {
            inner: store.clone(),
            read_started: read_started.clone(),
        }));
    let mut harness = HeadlessWorkbench::new(state, [1200.0, 900.0]);
    harness.state_mut().open_provider_settings();
    harness.state_mut().provider_settings_mut().edit("A");
    harness.run();
    // When
    harness.click_label("Save");
    finish_save(&mut harness);
    // Then
    assert!(
        read_started.load(Ordering::SeqCst),
        "Save must use the injected store"
    );
    assert!(harness.state().provider_settings().error.is_none());
    assert!(harness.state().provider_settings().editor.is_none());
    assert_eq!(load_config(temp.path()).providers.len(), 1);
    assert_eq!(store.get("acct-A").unwrap().unwrap().expose(), "old-token");
}
