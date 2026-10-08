use crate::ProviderError;
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    sync::{Mutex, PoisonError},
};

/// A complete rotated credential family. Persist atomically in the OS keyring.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorTokenBundle {
    pub access_token: String,
    pub refresh_token: String,
    /// UNIX seconds. The refresh skew is applied only by `needs_refresh`.
    pub expires_at: u64,
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
}
impl CursorTokenBundle {
    pub fn needs_refresh(&self, now: u64) -> bool {
        self.expires_at.saturating_sub(now) <= 300
    }
}
impl fmt::Debug for CursorTokenBundle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CursorTokenBundle")
            .field("credentials", &"<redacted>")
            .finish()
    }
}
pub trait CursorTokenStore: Send + Sync {
    fn load(&self) -> Result<Option<CursorTokenBundle>, ProviderError>;
    fn save(&self, bundle: &CursorTokenBundle) -> Result<(), ProviderError>;
    fn refresh_lock(&self) -> std::sync::Arc<tokio::sync::Mutex<()>> {
        static LOCK: std::sync::LazyLock<std::sync::Arc<tokio::sync::Mutex<()>>> =
            std::sync::LazyLock::new(|| std::sync::Arc::new(tokio::sync::Mutex::new(())));
        LOCK.clone()
    }
}
#[derive(Default)]
pub struct InMemoryCursorTokenStore {
    bundle: Mutex<Option<CursorTokenBundle>>,
}
impl InMemoryCursorTokenStore {
    pub fn new() -> Self {
        Self::default()
    }
}
impl CursorTokenStore for InMemoryCursorTokenStore {
    fn load(&self) -> Result<Option<CursorTokenBundle>, ProviderError> {
        Ok(self
            .bundle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone())
    }
    fn save(&self, bundle: &CursorTokenBundle) -> Result<(), ProviderError> {
        *self.bundle.lock().unwrap_or_else(PoisonError::into_inner) = Some(bundle.clone());
        Ok(())
    }
}
